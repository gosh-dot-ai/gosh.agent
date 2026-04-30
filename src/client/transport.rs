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

/// Canonicalise an authority URL into the full MCP JSON-RPC endpoint
/// (`<authority>/mcp`), regardless of which redundant suffix variants
/// the caller passed in.
///
/// `authority_url` legitimately appears in three shapes across the
/// codebase: bare (`http://h:8765`), trailing slash (`http://h:8765/`),
/// or already-canonical (`http://h:8765/mcp` — the form
/// `--public-url` overrides and some remote-bundle imports actually
/// emit). Without this normalisation, a `format!("{base}/mcp")` style
/// concatenation on the third variant produces `/mcp/mcp`, which
/// FastMCP answers with a bare 404 and which historically masqueraded
/// as a "stale URL" production incident.
///
/// Centralised here so every MCP transport path — `HttpTransport`
/// (used by capture and replay-buffer), the stdio mcp-proxy in
/// `plugin::proxy`, and the courier SSE subscription in `crate::courier`
/// — converges on the same canonical form. Sub-path mounts (e.g.
/// reverse-proxy at `/memory`) are preserved: only a trailing `/mcp`
/// is stripped, never an interior one.
pub fn canonical_mcp_url(authority_url: &str) -> String {
    let trimmed = authority_url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/mcp").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    format!("{trimmed}/mcp")
}

/// MCP transport over HTTP (Streamable HTTP spec). Stores the resolved
/// canonical `<authority>/mcp` endpoint so a config value that already
/// includes `/mcp` doesn't double up at request time.
pub struct HttpTransport {
    mcp_endpoint: String,
    http: reqwest::Client,
    server_token: Option<String>,
    principal_auth_token: Option<String>,
}

impl HttpTransport {
    pub fn new(
        authority_url: &str,
        server_token: Option<String>,
        principal_auth_token: Option<String>,
    ) -> Self {
        Self {
            mcp_endpoint: canonical_mcp_url(authority_url),
            http: reqwest::Client::new(),
            server_token,
            principal_auth_token,
        }
    }

    /// Create transport with a custom reqwest Client (e.g. for TLS pinning).
    pub fn with_client(
        authority_url: &str,
        server_token: Option<String>,
        principal_auth_token: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            mcp_endpoint: canonical_mcp_url(authority_url),
            http: client,
            server_token,
            principal_auth_token,
        }
    }

    /// The full canonical MCP JSON-RPC endpoint this transport posts to.
    /// Exposed for tests so the
    /// `http_transport_endpoint_is_canonical_for_every_input_shape`
    /// regression below can pin the constructor's centralisation
    /// without a network round-trip; production callers go through
    /// `send`.
    #[cfg(test)]
    pub(crate) fn mcp_endpoint(&self) -> &str {
        &self.mcp_endpoint
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send(
        &self,
        body: &Value,
        session_id: Option<&str>,
    ) -> Result<(Value, Option<String>)> {
        let mut req = self
            .http
            .post(&self.mcp_endpoint)
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

    use super::canonical_mcp_url;
    use super::extract_sse_response;
    use super::parse_sse_response;
    use super::HttpTransport;

    #[test]
    fn canonical_mcp_url_normalises_trailing_variants() {
        // Whatever the caller put in `authority_url` — bare host, trailing
        // slash, already-includes-/mcp, includes-/mcp-with-slash — we
        // converge on a single canonical form so `format!("{base}/mcp")`-
        // style concatenation can't double up into `/mcp/mcp` (which
        // FastMCP 404's). This regression historically masqueraded as a
        // "stale URL" production incident.
        let canonical = "http://localhost:8765/mcp";
        for input in [
            "http://localhost:8765",
            "http://localhost:8765/",
            "http://localhost:8765/mcp",
            "http://localhost:8765/mcp/",
        ] {
            assert_eq!(canonical_mcp_url(input), canonical, "input was {input:?}");
        }
    }

    #[test]
    fn canonical_mcp_url_preserves_subpath_mounts() {
        // A reverse-proxy mount at e.g. `/memory` is legitimate; we must
        // append `/mcp` to it (giving `/memory/mcp`) and NOT strip the
        // host's subpath as if it were a bogus `/mcp` suffix.
        assert_eq!(
            canonical_mcp_url("https://example.com/memory"),
            "https://example.com/memory/mcp",
        );
        assert_eq!(
            canonical_mcp_url("https://example.com/memory/"),
            "https://example.com/memory/mcp",
        );
        assert_eq!(
            canonical_mcp_url("https://example.com/memory/mcp"),
            "https://example.com/memory/mcp",
        );
    }

    #[test]
    fn http_transport_endpoint_is_canonical_for_every_input_shape() {
        // Pin the contract that `HttpTransport::new` actually runs the
        // canonicalisation rather than just trimming. Without this the
        // capture and replay-buffer call paths could regress back to
        // double-`/mcp` shapes if the constructor was changed in
        // isolation. Same expectation for `with_client`.
        for input in [
            "http://localhost:8765",
            "http://localhost:8765/",
            "http://localhost:8765/mcp",
            "http://localhost:8765/mcp/",
            "https://example.com/memory/mcp",
        ] {
            let t = HttpTransport::new(input, None, None);
            assert!(
                t.mcp_endpoint().ends_with("/mcp"),
                "endpoint should end in /mcp, got {} from {input:?}",
                t.mcp_endpoint(),
            );
            assert!(
                !t.mcp_endpoint().ends_with("/mcp/mcp"),
                "endpoint must not double up, got {} from {input:?}",
                t.mcp_endpoint(),
            );
            assert_eq!(t.mcp_endpoint(), canonical_mcp_url(input));

            let tw = HttpTransport::with_client(input, None, None, reqwest::Client::new());
            assert_eq!(tw.mcp_endpoint(), canonical_mcp_url(input));
        }
    }

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
