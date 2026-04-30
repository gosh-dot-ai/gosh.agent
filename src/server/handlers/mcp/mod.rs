// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

pub mod courier_subscribe;
pub mod courier_unsubscribe;
pub mod create_task;
pub mod start;
pub mod status;
pub mod task_list;

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use serde_json::Value;
use tracing::warn;

use crate::client::memory::MemoryMcpClient;
use crate::client::memory_inject;
use crate::server::AppState;

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = body.get("id").cloned();
    let params = body.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => {
            let mut counter = state.session_counter.lock().await;
            *counter += 1;
            let sid = format!("{:032x}", *counter);
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "gosh-agent", "version": env!("CARGO_PKG_VERSION") }
                }
            });
            (
                StatusCode::OK,
                [("content-type", "application/json"), ("Mcp-Session-Id", &sid)],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response()
        }

        "notifications/initialized" => (StatusCode::OK, "").into_response(),

        "tools/list" => {
            let tools = build_tools_list(&state.memory).await;
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools }
            });
            let sid = headers
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();
            (
                StatusCode::OK,
                [("content-type", "application/json"), ("Mcp-Session-Id", sid.as_str())],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response()
        }

        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = match tool_name {
                "agent_start" => start::handle(&state, &args).await,
                "agent_status" => status::handle(&state, &args).await,
                "agent_create_task" => create_task::handle(&state, &args).await,
                "agent_task_list" => task_list::handle(&state, &args).await,
                "agent_courier_subscribe" => courier_subscribe::handle(state.clone(), &args).await,
                "agent_courier_unsubscribe" => courier_unsubscribe::handle(&state).await,
                name if memory_inject::is_memory_tool_name(name) => {
                    if !grounded_memory_tool_allowed(name) {
                        // Defense-in-depth: even if the upstream caller
                        // (stdio proxy / curl / Claude.ai connector)
                        // didn't filter the tool, the daemon refuses
                        // tools outside the grounded surface. Mirrors
                        // the `tools/list` filter so the surface stays
                        // consistent across discovery and dispatch.
                        json!({
                            "error": format!(
                                "tool '{name}' is not part of the daemon's grounded \
                                 memory surface"
                            ),
                            "code": "TOOL_NOT_ALLOWED",
                        })
                    } else {
                        forward_memory_tool(
                            &state.memory,
                            state.default_key.as_deref(),
                            state.default_swarm_id.as_deref(),
                            name,
                            args,
                        )
                        .await
                    }
                }
                _ => json!({
                    "error": format!("unknown tool: {tool_name}"),
                    "code": "UNKNOWN_TOOL"
                }),
            };

            let is_error =
                result.get("error").is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap() }],
                    "isError": is_error,
                }
            });

            let sid = headers
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();

            (
                StatusCode::OK,
                [("content-type", "application/json"), ("Mcp-Session-Id", sid.as_str())],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response()
        }

        _ => (StatusCode::OK, "").into_response(),
    }
}

/// Compose the daemon's exposed `tools/list` surface: filtered
/// `memory_*` tools fetched from memory, plus the daemon's own
/// externally-exposed `agent_*` tools with LLM-tuned descriptions.
///
/// If memory is unreachable or returns an error, log and degrade
/// gracefully — the daemon still surfaces its `agent_*` tools so a
/// coding-CLI session that cares about task dispatch can still
/// function. The next call attempt will re-query memory; we don't
/// cache.
async fn build_tools_list(memory: &MemoryMcpClient) -> Vec<Value> {
    let mut tools = match memory.list_tools().await {
        Ok(result) => filtered_memory_tools(result),
        Err(e) => {
            warn!(error = %e, "failed to fetch memory tools/list; surfacing only agent_* tools");
            Vec::new()
        }
    };
    tools.extend(externally_exposed_agent_tools());
    tools
}

/// Filter memory's raw `tools/list` response down to the grounded
/// surface the daemon advertises to coding-CLI LLMs.
///
/// 1. Drops tools outside the grounded allowlist (e.g. `memory_ask`,
///    `memory_store`, anything `auth_*` / `principal_*`).
/// 2. Strips `key` from each remaining tool's input schema, since the daemon's
///    forwarder injects it from the configured default when the caller omits
///    it. The schema would otherwise advertise `key` as required and confuse
///    the LLM into prompting the user for it.
fn filtered_memory_tools(memory_result: Value) -> Vec<Value> {
    let Some(tools) = memory_result.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter(|tool| {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            grounded_memory_tool_allowed(name)
        })
        .cloned()
        .map(|mut tool| {
            strip_key_from_tool_schema(&mut tool);
            tool
        })
        .collect()
}

/// Mirror of the proxy-side grounded whitelist. After Commit 4 in the
/// MCP unification spec the proxy stops filtering and the duplicate
/// in `plugin/proxy.rs` goes away; until then the two lists must stay
/// in sync.
fn grounded_memory_tool_allowed(name: &str) -> bool {
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

fn strip_key_from_tool_schema(tool: &mut Value) {
    for field in ["inputSchema", "input_schema"] {
        if let Some(schema) = tool.get_mut(field) {
            if let Some(props) = schema.get_mut("properties").and_then(|v| v.as_object_mut()) {
                props.remove("key");
            }
            if let Some(required) = schema.get_mut("required").and_then(|v| v.as_array_mut()) {
                required.retain(|item| item.as_str() != Some("key"));
            }
        }
    }
}

/// The `agent_*` tools the daemon advertises to coding-CLI LLMs.
///
/// Three are exposed:
///   - `agent_create_task` — fire-and-forget dispatch onto the headless agent's
///     courier-driven task queue.
///   - `agent_status` — query the state of a previously-dispatched task by id.
///   - `agent_task_list` — discovery of recent tasks for this agent.
///
/// `agent_start` (synchronous task execution), `agent_courier_subscribe`,
/// and `agent_courier_unsubscribe` stay internal — the first because
/// it's a CLI / curl path, not a chat-LLM pattern; the others because
/// they require stateful SSE that per-turn LLM tool calls cannot
/// consume.
///
/// Descriptions are written for LLMs, not human operators. The tone
/// emphasises the dispatch-and-poll semantics so the model doesn't
/// loop expecting synchronous completion.
fn externally_exposed_agent_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "agent_create_task",
            "description": "Enqueue a task description for the headless gosh-agent to execute asynchronously. Returns a `task_id` immediately and DOES NOT wait for completion. The agent picks the task up via its courier subscription within ~30 seconds and writes results back to memory. To check whether the task was picked up or finished, call `agent_task_list` or `agent_status` later, or recall the result via `memory_recall`. Do not call this tool in a loop expecting synchronous behaviour.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Free-form natural-language task instruction."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["agent-private", "swarm-shared", "system-wide"],
                        "description": "Visibility of the resulting facts. `agent-private` is for the namespace owner only; non-owners writing into a swarm-bound namespace must use `swarm-shared`."
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Optional dispatch priority hint."
                    },
                    "swarm_id": { "type": "string" },
                    "context_key": { "type": "string" },
                    "task_id": { "type": "string" },
                    "workflow_id": { "type": "string" },
                    "route": { "type": "string" },
                    "target": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Principal IDs (e.g. `agent:name`) the task is addressed to."
                    },
                    "metadata": { "type": "object" }
                },
                "required": ["description"]
            }
        }),
        json!({
            "name": "agent_status",
            "description": "Query the state of a task previously enqueued via `agent_create_task`. Use after a dispatch to check whether the agent picked up the task and whether it finished. Returns running / done / failed plus result metadata when terminal.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "swarm_id": { "type": "string" }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "agent_task_list",
            "description": "List recent tasks for this agent. Use to confirm a previously-created task was picked up and to see the queued / running / done / failed mix. Filterable by swarm.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "swarm_id": { "type": "string" }
                }
            }
        }),
    ]
}

/// Forward a `memory_*` tool call onto memory itself, applying per-call
/// scoping (LLM-supplied `key` / `swarm_id` win, missing fields fall
/// back to the daemon's configured defaults).
///
/// Returns a `Value` shaped like the agent's other in-process handlers
/// — on success, the unwrapped tool result; on failure, an
/// `{ "error": ..., "code": ... }` object that the caller in `handle()`
/// detects via `result.get("error")` and surfaces as `isError: true`.
/// Errors lose some structured detail compared to memory's native MCP
/// response shape (the underlying `MemoryMcpClient::forward_tool` bails
/// on JSON-RPC errors and `isError=true` with a flattened message); a
/// future refinement can preserve the raw shape if and when callers
/// need it. For now, the flattened error matches what coding-CLI LLMs
/// are accustomed to seeing from the agent's existing tools.
///
/// Takes its dependencies explicitly (rather than `&Arc<AppState>`) to
/// keep the function unit-testable without spinning up a full `AppState`.
async fn forward_memory_tool(
    memory: &MemoryMcpClient,
    default_key: Option<&str>,
    default_swarm_id: Option<&str>,
    tool_name: &str,
    mut args: Value,
) -> Value {
    if let Some(k) = default_key {
        memory_inject::set_default_key_if_absent(&mut args, k);
    }
    if let Some(s) = default_swarm_id {
        memory_inject::set_default_swarm_id_if_absent(&mut args, s);
    }

    match memory.forward_tool(tool_name, args).await {
        Ok(result) => result,
        Err(e) => json!({
            "error": format!("memory tool '{tool_name}' failed: {e}"),
            "code": "MEMORY_FORWARD_FAILED",
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::build_tools_list;
    use super::externally_exposed_agent_tools;
    use super::filtered_memory_tools;
    use super::forward_memory_tool;
    use super::grounded_memory_tool_allowed;
    use crate::client::memory::MemoryMcpClient;
    use crate::test_support::take_calls;
    use crate::test_support::wrap_mcp_response;
    use crate::test_support::MockTransport;

    #[tokio::test]
    async fn forward_memory_tool_injects_defaults_when_caller_omits() {
        // Per-call scoping: caller didn't provide `key` / `swarm_id`,
        // so the daemon's bound defaults fill in.
        let canned = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![canned]);
        let memory = MemoryMcpClient::new(transport);

        let result = forward_memory_tool(
            &memory,
            Some("bound-namespace"),
            Some("bound-swarm"),
            "memory_recall",
            json!({"query": "anything"}),
        )
        .await;

        // Successful response — no `error` key.
        assert!(result.get("error").is_none(), "expected success result, got {result:?}");

        // Mock saw one call with the injected defaults.
        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "memory_recall");
        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("bound-namespace"));
        assert_eq!(args.get("swarm_id").and_then(|v| v.as_str()), Some("bound-swarm"));
        assert_eq!(args.get("query").and_then(|v| v.as_str()), Some("anything"));
    }

    #[tokio::test]
    async fn forward_memory_tool_preserves_caller_supplied_scope() {
        // Per-call scoping: caller's explicit values win over defaults.
        // This is the central behavioural difference from the stdio
        // proxy, which always overwrites for security; the daemon
        // trusts the LLM to specify a scope when it has one in mind
        // because agents are multi-swarm/multi-namespace by design.
        let canned = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![canned]);
        let memory = MemoryMcpClient::new(transport);

        let _ = forward_memory_tool(
            &memory,
            Some("default-namespace"),
            Some("default-swarm"),
            "memory_recall",
            json!({
                "query": "x",
                "key": "explicit-namespace",
                "swarm_id": "explicit-swarm"
            }),
        )
        .await;

        let calls = take_calls(&mock_state);
        let (_, args) = &calls[0];
        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("explicit-namespace"));
        assert_eq!(args.get("swarm_id").and_then(|v| v.as_str()), Some("explicit-swarm"));
    }

    #[tokio::test]
    async fn forward_memory_tool_with_no_defaults_passes_args_through() {
        // If the daemon has no configured defaults, missing scope is
        // not synthesised. The downstream (memory) is responsible for
        // rejecting the malformed call. We only assert the proxy did
        // not invent a value of its own.
        let canned = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![canned]);
        let memory = MemoryMcpClient::new(transport);

        let _ =
            forward_memory_tool(&memory, None, None, "memory_recall", json!({"query": "x"})).await;

        let calls = take_calls(&mock_state);
        let (_, args) = &calls[0];
        assert!(args.get("key").is_none());
        assert!(args.get("swarm_id").is_none());
    }

    #[tokio::test]
    async fn forward_memory_tool_surfaces_underlying_failure_as_error_value() {
        // When `MemoryMcpClient::forward_tool` bails (e.g., the mock
        // canned response is `isError: true`), the daemon's wrapper
        // converts the bail into an `{error, code}` Value matching the
        // shape its in-process handlers (`agent_*`) use for failures.
        // The outer `handle()` then surfaces it as `isError: true` to
        // the caller.
        let canned = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{"type": "text", "text": "memory bombed"}],
                "isError": true
            }
        });
        let (transport, _) = MockTransport::new(vec![canned]);
        let memory = MemoryMcpClient::new(transport);

        let result =
            forward_memory_tool(&memory, None, None, "memory_recall", json!({"query": "x"})).await;

        assert_eq!(result.get("code").and_then(|v| v.as_str()), Some("MEMORY_FORWARD_FAILED"));
        assert!(
            result
                .get("error")
                .and_then(|v| v.as_str())
                .is_some_and(|msg| msg.contains("memory_recall") && msg.contains("memory bombed")),
            "error message should mention the tool name and the bombed reason: {result:?}"
        );
    }

    #[test]
    fn grounded_memory_tool_allowed_matches_proxy_whitelist() {
        // Until Commit 4 collapses the proxy, this whitelist must stay
        // in sync with `plugin::proxy::grounded_proxy_tool_allowed`.
        // Rather than reach across modules in tests, sanity-check the
        // expected set explicitly; if either side ever drifts the
        // assertion here will catch it on the daemon side.
        for allowed in [
            "memory_recall",
            "memory_list",
            "memory_get",
            "memory_query",
            "memory_write",
            "memory_write_status",
            "memory_ingest_asserted_facts",
        ] {
            assert!(grounded_memory_tool_allowed(allowed), "{allowed} should be allowed");
        }
        for blocked in ["memory_ask", "memory_store", "memory_plan_inference", "auth_principal"] {
            assert!(!grounded_memory_tool_allowed(blocked), "{blocked} should be blocked");
        }
    }

    #[test]
    fn filtered_memory_tools_drops_blocked_tools_and_strips_key() {
        let memory_response = json!({
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
                    "inputSchema": {"type": "object"}
                },
                {
                    "name": "memory_write",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["key", "content"]
                    }
                }
            ]
        });

        let filtered = filtered_memory_tools(memory_response);

        // memory_ask was blocked → 2 tools remain.
        assert_eq!(filtered.len(), 2);
        let names: Vec<&str> =
            filtered.iter().map(|t| t.get("name").unwrap().as_str().unwrap()).collect();
        assert_eq!(names, vec!["memory_recall", "memory_write"]);

        // `key` stripped from both schemas (properties + required).
        for tool in &filtered {
            let schema = tool.get("inputSchema").unwrap();
            assert!(schema.pointer("/properties/key").is_none(), "{tool:?}");
            let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
            assert!(!required.iter().any(|r| r.as_str() == Some("key")), "{tool:?}");
        }
    }

    #[test]
    fn filtered_memory_tools_handles_missing_or_malformed_input() {
        // No `tools` key → empty list, no panic.
        assert!(filtered_memory_tools(json!({})).is_empty());
        assert!(filtered_memory_tools(json!({"tools": "not an array"})).is_empty());
        assert!(filtered_memory_tools(json!({"tools": []})).is_empty());
    }

    #[test]
    fn externally_exposed_agent_tools_lists_the_three_chat_facing_tools() {
        // The selection is part of the unification spec: only
        // dispatch + status + listing for chat. `agent_start` and
        // courier subscriptions stay internal.
        let tools = externally_exposed_agent_tools();
        let names: Vec<&str> =
            tools.iter().map(|t| t.get("name").unwrap().as_str().unwrap()).collect();
        assert_eq!(names, vec!["agent_create_task", "agent_status", "agent_task_list"]);

        for tool in &tools {
            assert!(
                tool.get("description").and_then(|v| v.as_str()).is_some(),
                "agent tool missing description: {tool:?}"
            );
            assert!(tool.get("inputSchema").is_some(), "agent tool missing inputSchema: {tool:?}");
        }

        // `agent_create_task` description must steer the LLM away from
        // looping for synchronous completion — the most easily-broken
        // semantic of dispatch-and-poll.
        let create = tools.iter().find(|t| t.get("name").unwrap() == "agent_create_task").unwrap();
        let description = create.get("description").and_then(|v| v.as_str()).unwrap();
        assert!(
            description.contains("DOES NOT wait") || description.contains("does not wait"),
            "agent_create_task description should warn against blocking on completion: {description}"
        );
    }

    #[tokio::test]
    async fn build_tools_list_merges_memory_and_agent_tools() {
        // tools/list responses don't follow the content+isError shape
        // that `wrap_mcp_response` emits for tools/call — they put the
        // `tools` array directly under `result`. Build the envelope
        // explicitly here.
        let memory_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    {
                        "name": "memory_recall",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}}
                        }
                    },
                    {"name": "memory_ask", "inputSchema": {"type": "object"}}
                ]
            }
        });
        let (transport, _) = MockTransport::new(vec![memory_response]);
        let memory = MemoryMcpClient::new(transport);

        let tools = build_tools_list(&memory).await;
        let names: Vec<&str> =
            tools.iter().map(|t| t.get("name").unwrap().as_str().unwrap()).collect();
        // memory_recall (kept), memory_ask (filtered), then the three agent_* tools.
        assert_eq!(
            names,
            vec!["memory_recall", "agent_create_task", "agent_status", "agent_task_list"]
        );
    }

    #[tokio::test]
    async fn build_tools_list_degrades_to_agent_tools_when_memory_unreachable() {
        // No canned responses → mock returns empty default; first
        // initialize succeeds but we'll pre-empt it by providing a
        // canned error response so list_tools bails. To keep the test
        // simple, use a response shaped as a JSON-RPC error.
        let canned_error = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32603, "message": "memory is sad"}
        });
        let (transport, _) = MockTransport::new(vec![canned_error]);
        let memory = MemoryMcpClient::new(transport);

        let tools = build_tools_list(&memory).await;
        let names: Vec<&str> =
            tools.iter().map(|t| t.get("name").unwrap().as_str().unwrap()).collect();
        assert_eq!(names, vec!["agent_create_task", "agent_status", "agent_task_list"]);
    }
}
