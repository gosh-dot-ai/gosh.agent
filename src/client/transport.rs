// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::bail;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Low-level transport for MCP JSON-RPC messages.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC body. Returns (response_body, session_id).
    async fn send(&self, body: &Value, session_id: Option<&str>)
        -> Result<(Value, Option<String>)>;
}

/// MCP transport over HTTP (Streamable HTTP spec).
pub struct HttpTransport {
    base_url: String,
    http: reqwest::Client,
    server_token: Option<String>,
    principal_auth_token: Option<String>,
}

impl HttpTransport {
    pub fn new(
        base_url: &str,
        server_token: Option<String>,
        principal_auth_token: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            server_token,
            principal_auth_token,
        }
    }

    /// Create transport with a custom reqwest Client (e.g. for TLS pinning).
    pub fn with_client(
        base_url: &str,
        server_token: Option<String>,
        principal_auth_token: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: client,
            server_token,
            principal_auth_token,
        }
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send(
        &self,
        body: &Value,
        session_id: Option<&str>,
    ) -> Result<(Value, Option<String>)> {
        let url = format!("{}/mcp", self.base_url);

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(token) = &self.server_token {
            req = req.header("x-server-token", token);
        }
        if let Some(token) = &self.principal_auth_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        let mut resp = req.json(body).send().await?;

        let sid = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("MCP call failed (HTTP {status}): {text}");
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            let mut buffer = String::new();
            while let Some(chunk) = resp.chunk().await? {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                if let Some(result) = extract_sse_response(&buffer) {
                    return Ok((result, sid));
                }
            }
            let result = parse_sse_response(&buffer)?;
            Ok((result, sid))
        } else {
            let result: Value = resp.json().await?;
            Ok((result, sid))
        }
    }
}

fn extract_sse_response(body: &str) -> Option<Value> {
    for line in body.lines() {
        let line = line.trim();
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                if parsed.get("id").is_some() {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn parse_sse_response(body: &str) -> Result<Value> {
    extract_sse_response(body)
        .ok_or_else(|| anyhow::anyhow!("no JSON-RPC response found in SSE stream"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_sse_response;
    use super::parse_sse_response;

    #[test]
    fn extract_sse_response_reads_first_complete_event_from_partial_stream() {
        let partial = "event: message\ndata: {\"jsonrpc\":\"2.0\"";
        assert!(extract_sse_response(partial).is_none());

        let complete = format!("{partial},\"id\":1,\"result\":{{\"ok\":true}}}}\n\n");
        let parsed = extract_sse_response(&complete).expect("expected parsed event");
        assert_eq!(parsed, json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}));
    }

    #[test]
    fn parse_sse_response_ignores_non_json_data_lines() {
        let body = "event: ping\ndata: not-json\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"value\":1}}\n\n";
        let parsed = parse_sse_response(body).expect("expected parsed event");
        assert_eq!(parsed["id"], 2);
        assert_eq!(parsed["result"]["value"], 1);
    }
}
