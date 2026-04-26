// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use tokio::fs;
use tokio::sync::mpsc;

use super::config::GlobalConfig;

/// Run as a stdio MCP proxy.
///
/// Reads JSON-RPC from stdin, injects default key + default swarm, and
/// forwards to authority. Supports both newline-delimited and Content-Length
/// framed messages.
///
/// Stdin reading runs in a blocking thread to avoid stalling the tokio runtime.
pub async fn run(
    agent_name: &str,
    default_key: Option<&str>,
    default_swarm: Option<&str>,
    full_memory_surface: bool,
) -> Result<()> {
    let global_config = GlobalConfig::load(agent_name)?;
    let authority_url = format!("{}/mcp", global_config.authority_url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let mut mcp_session_id: Option<String> = None;

    // Spawn blocking stdin reader to avoid stalling the tokio runtime
    let (tx, mut rx) = mpsc::channel::<(Framing, String)>(16);
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let framing = match detect_framing(&mut reader) {
            Ok(f) => f,
            Err(_) => return,
        };
        loop {
            let msg = match framing {
                Framing::ContentLength => read_content_length_message(&mut reader),
                Framing::Newline => read_newline_message(&mut reader),
            };
            match msg {
                Ok(line) => {
                    if tx.blocking_send((framing, line)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut session_framing: Option<Framing> = None;

    while let Some((incoming_framing, line)) = rx.recv().await {
        let framing = session_output_framing(&mut session_framing, incoming_framing);

        let mut request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let error_resp = make_error_response(None, -32700, &format!("parse error: {e}"));
                write_response(&mut stdout, &error_resp, framing)?;
                continue;
            }
        };

        let request_id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();

        if method == "tools/call" {
            // Resolve tool name once: drives both the grounded-proxy allowlist
            // check and the memory-only injection guard below.
            let tool_name =
                request.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            if !tool_name.is_empty() && !proxy_tool_call_allowed(&tool_name, full_memory_surface) {
                let error_resp = make_error_response(
                    request_id,
                    -32601,
                    &format!("tool unavailable in grounded proxy profile: {tool_name}"),
                );
                write_response(&mut stdout, &error_resp, framing)?;
                continue;
            }

            // Inject default key + swarm only into memory tools. Non-memory
            // tools (custom MCP servers reachable through the same proxy)
            // pass through with their arguments byte-for-byte unchanged —
            // mutating them would feed memory-specific args into unrelated
            // tool schemas.
            if is_memory_tool_name(&tool_name) {
                let Some(default_key) = default_key else {
                    let error_resp = make_error_response(
                        request_id,
                        -32602,
                        "memory proxy requires --default-key for memory tools",
                    );
                    write_response(&mut stdout, &error_resp, framing)?;
                    continue;
                };
                inject_default_key(&mut request, default_key);
                if let Some(default_swarm) = default_swarm {
                    inject_default_swarm(&mut request, default_swarm);
                }
            }
        }

        // Forward to authority with session management
        let mut http_req = client
            .post(&authority_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&request);

        if let Some(token) = &global_config.token {
            http_req = http_req.header("x-server-token", token);
        }
        if let Some(token) = global_config
            .principal_auth_token
            .clone()
            .or_else(|| std::env::var("GOSH_MEMORY_AUTH_TOKEN").ok())
        {
            if !token.is_empty() {
                http_req = http_req.header("Authorization", format!("Bearer {token}"));
            }
        }
        if let Some(sid) = &mcp_session_id {
            http_req = http_req.header("Mcp-Session-Id", sid);
        }

        let mut response = match http_req.send().await {
            Ok(resp) => {
                // Capture session ID from response
                if let Some(sid) = resp.headers().get("Mcp-Session-Id") {
                    if let Ok(s) = sid.to_str() {
                        mcp_session_id = Some(s.to_string());
                    }
                }

                if resp.status().is_success() {
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();

                    let raw = resp.text().await.unwrap_or_default();

                    if content_type.contains("text/event-stream") {
                        parse_sse_response(&raw, &request_id)
                    } else {
                        serde_json::from_str(&raw).unwrap_or_else(|e| {
                            make_error_response(
                                request_id.clone(),
                                -32603,
                                &format!("invalid response: {e}"),
                            )
                        })
                    }
                } else {
                    make_error_response(
                        request_id,
                        -32603,
                        &format!("authority returned HTTP {}", resp.status()),
                    )
                }
            }
            Err(e) => {
                make_error_response(request_id, -32603, &format!("authority unreachable: {e}"))
            }
        };

        if method == "tools/list" {
            rewrite_tools_list_response(&mut response, full_memory_surface);
        }
        let _ = trace_memory_proxy_call(&request, &response).await;

        // Notifications (no id) — don't send a response
        if method.starts_with("notifications/") {
            continue;
        }

        write_response(&mut stdout, &response, framing)?;
    }

    Ok(())
}

// ── Framing ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    ContentLength,
    Newline,
}

/// Peek at first bytes to detect if client uses Content-Length framing.
fn detect_framing(reader: &mut impl BufRead) -> Result<Framing> {
    let buf = reader.fill_buf()?;
    if buf.starts_with(b"Content-Length:") || buf.starts_with(b"content-length:") {
        Ok(Framing::ContentLength)
    } else {
        Ok(Framing::Newline)
    }
}

/// Read a Content-Length framed message: `Content-Length: N\r\n\r\n<N bytes>`.
fn read_content_length_message(reader: &mut impl BufRead) -> Result<String> {
    // Read headers until empty line
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 {
            anyhow::bail!("EOF");
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            content_length = Some(val.trim().parse().context("invalid Content-Length")?);
        }
    }

    let len = content_length.context("missing Content-Length header")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).context("invalid UTF-8 in message body")
}

/// Read a newline-delimited message.
fn read_newline_message(reader: &mut impl BufRead) -> Result<String> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            anyhow::bail!("EOF");
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
}

/// Write a response in the appropriate framing.
fn write_response(stdout: &mut impl Write, response: &Value, framing: Framing) -> Result<()> {
    let json = serde_json::to_string(response)?;
    match framing {
        Framing::ContentLength => {
            write!(stdout, "Content-Length: {}\r\n\r\n{}", json.len(), json)?;
        }
        Framing::Newline => {
            writeln!(stdout, "{}", json)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn session_output_framing(
    session_framing: &mut Option<Framing>,
    incoming_framing: Framing,
) -> Framing {
    *session_framing.get_or_insert(incoming_framing)
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Overwrite `arguments.key` with the proxy's bound key. Caller is
/// responsible for invoking this only on memory tools — the helper does
/// not check the tool name itself (see the gate in `run()`).
fn inject_default_key(request: &mut Value, default_key: &str) {
    if let Some(args) = request.pointer_mut("/params/arguments").and_then(|a| a.as_object_mut()) {
        args.insert("key".to_string(), Value::String(default_key.to_string()));
    }
}

/// Overwrite `arguments.swarm_id` with the proxy's bound swarm. Caller is
/// responsible for invoking this only on memory tools — the helper does
/// not check the tool name itself (see the gate in `run()`).
fn inject_default_swarm(request: &mut Value, default_swarm: &str) {
    if let Some(args) = request.pointer_mut("/params/arguments").and_then(|a| a.as_object_mut()) {
        args.insert("swarm_id".to_string(), Value::String(default_swarm.to_string()));
    }
}

async fn trace_memory_proxy_call(request: &Value, response: &Value) -> Result<()> {
    let tool_name = request.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
    if !is_memory_tool_name(tool_name) {
        return Ok(());
    }
    let trace_path = match std::env::var("GOSH_AGENT_MCP_PROXY_TRACE_PATH") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => return Ok(()),
    };
    let recall_dir = std::env::var("GOSH_AGENT_MCP_PROXY_RECALL_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            trace_path
                .parent()
                .map(|parent| parent.join("recalls"))
                .unwrap_or_else(|| PathBuf::from("recalls"))
        });
    fs::create_dir_all(&recall_dir).await?;
    if let Some(parent) = trace_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut trace = fs::read_to_string(&trace_path)
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| {
            json!({
                "prefetch_recall_count": 0,
                "memory_ask_used_for_patch_generation": false,
                "model_requested_recalls": [],
                "model_requested_tool_calls": [],
                "memory_ingest_asserted_facts_attempts": [],
                "memory_ingest_asserted_facts_attempt_count": 0,
                "memory_ingest_asserted_facts_failure_count": 0
            })
        });
    let existing_tool_calls = trace
        .get("model_requested_tool_calls")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let tool_idx = existing_tool_calls.len() + 1;
    let existing_recalls = trace
        .get("model_requested_recalls")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let recall_idx = existing_recalls.len() + 1;
    let args = request.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or(tool_name);
    let status = if response.get("error").is_some()
        || response.pointer("/result/isError").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        "error"
    } else {
        "ok"
    };
    let error = response
        .get("error")
        .map(|value| value.to_string())
        .or_else(|| {
            response
                .pointer("/result/isError")
                .and_then(|v| v.as_bool())
                .filter(|is_error| *is_error)
                .map(|_| "mcp tool returned isError=true".to_string())
        })
        .unwrap_or_default();
    let parsed_response = response
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| response.clone());
    let safe = safe_trace_label(query);
    let result_path = recall_dir.join(format!("{tool_idx:02}_{tool_name}_{safe}.json"));
    let timestamp = Utc::now().to_rfc3339();
    let record = json!({
        "caller": "model",
        "tool": tool_name,
        "timestamp": timestamp,
        "request": args,
        "status": status,
        "error": error,
        "response": parsed_response,
    });
    fs::write(&result_path, serde_json::to_vec_pretty(&record)?).await?;

    let tool_call_summary = json!({
        "caller": "model",
        "tool": tool_name,
        "timestamp": timestamp,
        "result_path": result_path.to_string_lossy(),
        "status": status,
        "error": error,
    });
    let mut tool_calls = existing_tool_calls;
    tool_calls.push(tool_call_summary.clone());
    trace["model_requested_tool_calls"] = Value::Array(tool_calls);
    trace["model_requested_tool_call_count"] = json!(tool_idx);

    if tool_name == "memory_recall" {
        let mut recalls = existing_recalls;
        recalls.push(json!({
            "caller": "model",
            "timestamp": timestamp,
            "query": query,
            "query_type": record.pointer("/request/query_type").cloned().unwrap_or(Value::Null),
            "token_budget": record.pointer("/request/token_budget").cloned().unwrap_or(Value::Null),
            "query_metadata": record.pointer("/request/query_metadata").cloned().unwrap_or(Value::Null),
            "result_path": result_path.to_string_lossy(),
            "status": status,
            "error": error,
        }));
        trace["model_requested_recalls"] = Value::Array(recalls);
        trace["model_requested_recall_count"] = json!(recall_idx);
    }
    if tool_name == "memory_ingest_asserted_facts" {
        let mut attempts = trace
            .get("memory_ingest_asserted_facts_attempts")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        attempts.push(tool_call_summary);
        let failure_count = attempts
            .iter()
            .filter(|attempt| attempt.get("status").and_then(|v| v.as_str()) != Some("ok"))
            .count();
        trace["memory_ingest_asserted_facts_attempt_count"] = json!(attempts.len());
        trace["memory_ingest_asserted_facts_failure_count"] = json!(failure_count);
        trace["memory_ingest_asserted_facts_attempts"] = Value::Array(attempts);
    }
    fs::write(&trace_path, serde_json::to_vec_pretty(&trace)?).await?;
    Ok(())
}

fn safe_trace_label(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars().take(80) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "memory_recall".to_string()
    } else {
        trimmed
    }
}

fn rewrite_tools_list_response(response: &mut Value, full_memory_surface: bool) {
    let Some(tools) = response.pointer_mut("/result/tools").and_then(|v| v.as_array_mut()) else {
        return;
    };

    tools.retain(|tool| {
        let name = tool.get("name").and_then(|value| value.as_str()).unwrap_or("");
        proxy_tool_call_allowed(name, full_memory_surface)
    });

    for tool in tools {
        let name = tool.get("name").and_then(|value| value.as_str()).unwrap_or("");
        if is_memory_tool_name(name) {
            strip_key_from_tool_schema(tool);
        }
    }
}

fn strip_key_from_tool_schema(tool: &mut Value) {
    if let Some(schema) = tool.get_mut("inputSchema") {
        strip_key_from_schema_value(schema);
    }
    if let Some(schema) = tool.get_mut("input_schema") {
        strip_key_from_schema_value(schema);
    }
}

fn strip_key_from_schema_value(schema: &mut Value) {
    if let Some(properties) = schema.get_mut("properties").and_then(|v| v.as_object_mut()) {
        properties.remove("key");
    }
    if let Some(required) = schema.get_mut("required").and_then(|v| v.as_array_mut()) {
        required.retain(|item| item.as_str() != Some("key"));
    }
}

/// True for memory data-plane tools that the proxy should auto-inject
/// `key` and `swarm_id` into.
///
/// **Invariant (maintained by gosh-ai-memory):** every data-plane tool
/// registered in `gosh-ai-memory/src/mcp_server.py` is named `memory_*`.
/// Control-plane tools (`auth_*`, `principal_*`, `swarm_*`, `membership_*`,
/// `courier_*`) deliberately do **not** match — for those, `swarm_id`
/// and `key` are the *target* of the operation (which swarm to create,
/// whose membership to grant), not the agent's bound scope, so silently
/// rewriting them with the proxy defaults would change the meaning of
/// the call.
///
/// If memory ever adds a data-plane tool without the `memory_` prefix,
/// it will silently miss injection here — keep the prefix convention.
fn is_memory_tool_name(name: &str) -> bool {
    name.starts_with("memory_")
}

fn proxy_tool_call_allowed(name: &str, full_memory_surface: bool) -> bool {
    full_memory_surface || !is_memory_tool_name(name) || grounded_proxy_tool_allowed(name)
}

// Grounded proxy is still bounded: retrieval plus the smallest write surface
// needed for external CLI flows to persist terminal deliverables back to
// memory.
fn grounded_proxy_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "memory_recall"
            | "memory_list"
            | "memory_get"
            | "memory_query"
            | "memory_write"
            | "memory_write_status"
            | "memory_ingest_asserted_facts"
    )
}

fn make_error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Extract JSON-RPC response from SSE stream.
fn parse_sse_response(raw: &str, fallback_id: &Option<Value>) -> Value {
    for line in raw.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(val) = serde_json::from_str::<Value>(data) {
                return val;
            }
        }
    }
    make_error_response(fallback_id.clone(), -32603, "no data in SSE response")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use serde_json::Value;

    use super::grounded_proxy_tool_allowed;
    use super::inject_default_key;
    use super::inject_default_swarm;
    use super::is_memory_tool_name;
    use super::proxy_tool_call_allowed;
    use super::rewrite_tools_list_response;
    use super::session_output_framing;
    use super::Framing;

    #[test]
    fn proxy_overwrites_incoming_memory_key_with_default_key() {
        let mut request = json!({
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": {
                    "key": "evil",
                    "query": "Atlas March metrics"
                }
            }
        });

        inject_default_key(&mut request, "bound-context");

        assert_eq!(
            request.pointer("/params/arguments/key").and_then(|v| v.as_str()),
            Some("bound-context")
        );
    }

    #[test]
    fn proxy_overwrites_incoming_swarm_id_with_default_swarm() {
        let mut request = json!({
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": {
                    "swarm_id": "evil-swarm",
                    "query": "Atlas March metrics"
                }
            }
        });

        inject_default_swarm(&mut request, "bound-swarm");

        assert_eq!(
            request.pointer("/params/arguments/swarm_id").and_then(|v| v.as_str()),
            Some("bound-swarm")
        );
    }

    #[test]
    fn proxy_inserts_swarm_id_when_caller_omits_it() {
        let mut request = json!({
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": { "query": "anything" }
            }
        });

        inject_default_swarm(&mut request, "bound-swarm");

        assert_eq!(
            request.pointer("/params/arguments/swarm_id").and_then(|v| v.as_str()),
            Some("bound-swarm")
        );
    }

    /// Mirror the run() gating: only inject defaults into memory tools.
    /// Non-memory tools (custom MCP servers reachable through the same
    /// proxy) must pass through with arguments byte-for-byte unchanged.
    fn apply_run_injection_gate(
        request: &mut Value,
        default_key: Option<&str>,
        default_swarm: Option<&str>,
    ) {
        let tool_name =
            request.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !is_memory_tool_name(&tool_name) {
            return;
        }
        if let Some(k) = default_key {
            inject_default_key(request, k);
        }
        if let Some(s) = default_swarm {
            inject_default_swarm(request, s);
        }
    }

    #[test]
    fn proxy_does_not_inject_defaults_into_non_memory_tool_calls() {
        let original_args = json!({ "foo": "bar", "nested": { "x": 1 } });
        let mut request = json!({
            "method": "tools/call",
            "params": {
                "name": "custom_tool",
                "arguments": original_args.clone()
            }
        });

        apply_run_injection_gate(&mut request, Some("bound-key"), Some("bound-swarm"));

        assert_eq!(request.pointer("/params/arguments"), Some(&original_args));
    }

    #[test]
    fn proxy_injects_defaults_into_memory_tool_calls() {
        let mut request = json!({
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": { "query": "anything" }
            }
        });

        apply_run_injection_gate(&mut request, Some("bound-key"), Some("bound-swarm"));

        assert_eq!(
            request.pointer("/params/arguments/key").and_then(|v| v.as_str()),
            Some("bound-key")
        );
        assert_eq!(
            request.pointer("/params/arguments/swarm_id").and_then(|v| v.as_str()),
            Some("bound-swarm")
        );
    }

    #[test]
    fn grounded_proxy_tools_list_strips_key_and_hides_memory_ask() {
        let mut response = json!({
            "result": {
                "tools": [
                    {
                        "name": "memory_recall",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string"},
                                "query": {"type": "string"}
                            },
                            "required": ["key", "query"]
                        }
                    },
                    {
                        "name": "memory_ask",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string"},
                                "question": {"type": "string"}
                            },
                            "required": ["key", "question"]
                        }
                    },
                    {
                        "name": "memory_query",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "key": {"type": "string"},
                                "filter": {"type": "object"}
                            },
                            "required": ["key"]
                        }
                    }
                ]
            }
        });

        rewrite_tools_list_response(&mut response, false);

        let tools = response.pointer("/result/tools").and_then(|v| v.as_array()).unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools
            .iter()
            .all(|tool| tool.get("name").and_then(|v| v.as_str()) != Some("memory_ask")));
        let recall = tools
            .iter()
            .find(|tool| tool.get("name").and_then(|v| v.as_str()) == Some("memory_recall"))
            .unwrap();
        assert!(recall.pointer("/inputSchema/properties/key").is_none());
        assert_eq!(
            recall.pointer("/inputSchema/required").and_then(|v| v.as_array()).unwrap(),
            &vec![json!("query")]
        );
    }

    #[test]
    fn grounded_proxy_filters_only_memory_surface_not_other_tools() {
        let mut response = json!({
            "result": {
                "tools": [
                    {"name": "memory_recall", "inputSchema": {"type": "object", "properties": {}, "required": []}},
                    {"name": "memory_ask", "inputSchema": {"type": "object", "properties": {}, "required": []}},
                    {"name": "custom_tool", "inputSchema": {"type": "object", "properties": {}, "required": []}}
                ]
            }
        });

        rewrite_tools_list_response(&mut response, false);
        let names: Vec<_> = response
            .pointer("/result/tools")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(names, vec!["memory_recall", "custom_tool"]);
    }

    #[test]
    fn full_surface_keeps_memory_ask_but_still_hides_key() {
        let mut response = json!({
            "result": {
                "tools": [
                    {
                        "name": "memory_ask",
                        "input_schema": {
                            "type": "object",
                            "properties": {"key": {"type": "string"}, "question": {"type": "string"}},
                            "required": ["key", "question"]
                        }
                    }
                ]
            }
        });

        rewrite_tools_list_response(&mut response, true);

        let ask = response.pointer("/result/tools/0").unwrap();
        assert_eq!(ask.get("name").and_then(|v| v.as_str()), Some("memory_ask"));
        assert!(ask.pointer("/input_schema/properties/key").is_none());
        assert_eq!(
            ask.pointer("/input_schema/required").and_then(|v| v.as_array()).unwrap(),
            &vec![json!("question")]
        );
    }

    #[test]
    fn grounded_proxy_tool_allowlist_keeps_minimal_write_surface_for_terminal_artifacts() {
        assert!(is_memory_tool_name("memory_query"));
        assert!(grounded_proxy_tool_allowed("memory_recall"));
        assert!(grounded_proxy_tool_allowed("memory_list"));
        assert!(grounded_proxy_tool_allowed("memory_get"));
        assert!(grounded_proxy_tool_allowed("memory_query"));
        assert!(grounded_proxy_tool_allowed("memory_write"));
        assert!(grounded_proxy_tool_allowed("memory_write_status"));
        assert!(grounded_proxy_tool_allowed("memory_ingest_asserted_facts"));
        assert!(!grounded_proxy_tool_allowed("memory_ask"));
        assert!(!grounded_proxy_tool_allowed("memory_store"));
    }

    #[test]
    fn grounded_proxy_tools_call_enforces_same_memory_allowlist() {
        assert!(proxy_tool_call_allowed("memory_recall", false));
        assert!(proxy_tool_call_allowed("memory_write", false));
        assert!(proxy_tool_call_allowed("custom_tool", false));
        assert!(!proxy_tool_call_allowed("memory_store", false));
        assert!(!proxy_tool_call_allowed("memory_ask", false));
        assert!(proxy_tool_call_allowed("memory_store", true));
        assert!(proxy_tool_call_allowed("memory_ask", true));
    }

    #[test]
    fn proxy_session_framing_sticks_to_first_content_length_message() {
        let mut session_framing = None;

        assert_eq!(
            session_output_framing(&mut session_framing, Framing::ContentLength),
            Framing::ContentLength
        );
        assert_eq!(
            session_output_framing(&mut session_framing, Framing::Newline),
            Framing::ContentLength
        );
    }

    #[test]
    fn proxy_session_framing_sticks_to_first_newline_message() {
        let mut session_framing = None;

        assert_eq!(
            session_output_framing(&mut session_framing, Framing::Newline),
            Framing::Newline
        );
        assert_eq!(
            session_output_framing(&mut session_framing, Framing::ContentLength),
            Framing::Newline
        );
    }
}
