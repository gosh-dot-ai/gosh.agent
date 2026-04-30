// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

pub mod memory;
pub mod memory_inject;
pub mod secrets;
pub mod transport;

use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
pub use transport::McpTransport;

/// MCP client.
pub struct McpClient {
    transport: Box<dyn McpTransport>,
    client_name: String,
}

impl McpClient {
    pub fn new(transport: impl McpTransport + 'static, client_name: &str) -> Self {
        Self { transport: Box::new(transport), client_name: client_name.to_string() }
    }

    #[allow(dead_code)]
    pub fn transport(&self) -> &dyn McpTransport {
        &*self.transport
    }

    pub async fn initialize(&self) -> Result<String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": &self.client_name, "version": env!("CARGO_PKG_VERSION") }
            }
        });

        let (resp, sid) = self.transport.send(&body, None).await?;
        let session_id =
            sid.ok_or_else(|| anyhow::anyhow!("server did not return Mcp-Session-Id"))?;

        if let Some(error) = resp.get("error") {
            bail!("MCP initialize error: {error}");
        }

        let notify = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = self.transport.send(&notify, Some(&session_id)).await;

        Ok(session_id)
    }

    /// Query the upstream MCP server for its `tools/list`. Returns the
    /// `result` field of the JSON-RPC response, which is normally
    /// `{ "tools": [ ... ] }`. Errors at the JSON-RPC envelope level
    /// bail; the caller handles the inner shape.
    pub async fn list_tools(&self) -> Result<Value> {
        let session_id = self.initialize().await?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        });
        let (response, _) = self.transport.send(&body, Some(&session_id)).await?;
        if let Some(error) = response.get("error") {
            bail!("MCP tools/list error: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<Value> {
        let session_id = self.initialize().await?;

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool_name, "arguments": args }
        });

        let (result, _) = self.transport.send(&body, Some(&session_id)).await?;

        if let Some(error) = result.get("error") {
            bail!("MCP error: {error}");
        }

        let mcp_result = result.get("result");
        let is_error =
            mcp_result.and_then(|r| r.get("isError")).and_then(|v| v.as_bool()).unwrap_or(false);

        if let Some(content) = mcp_result.and_then(|r| r.get("content")).and_then(|c| c.as_array())
        {
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        if is_error {
                            bail!("{tool_name}: {text}");
                        }
                        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                            // Check for application-level errors returned as JSON
                            if let Some(err_msg) = parsed.get("error").and_then(|v| v.as_str()) {
                                let code = parsed
                                    .get("code")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("UNKNOWN");
                                bail!("{tool_name} error: {err_msg} (code: {code})");
                            }
                            return Ok(parsed);
                        }
                        return Ok(Value::String(text.to_string()));
                    }
                }
            }
        }

        Ok(result.get("result").cloned().unwrap_or(Value::Null))
    }
}
