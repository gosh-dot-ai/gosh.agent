// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::io::BufRead;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::info;

/// Run the stdio proxy: read JSON-RPC from stdin, forward to the
/// daemon over HTTP, write responses back to stdout. Loop until stdin
/// closes.
///
/// `daemon_host` / `daemon_port` may be `None`, in which case the
/// proxy reads `GlobalConfig` for `agent_name` and uses the
/// `host`/`port` recorded there. This is the path that fires for
/// `.mcp.json` / `.codex/...` configs written by `gosh-agent setup`
/// — they emit explicit `--daemon-host` / `--daemon-port` for
/// auditability, but if either is missing (older configs that
/// pre-date the explicit-args change, or hand-edited entries that
/// dropped them) the GlobalConfig fallback ensures the proxy still
/// dials the *right* daemon for `--name`, not whatever process
/// happens to listen on `127.0.0.1:8767`. That mismatch is what
/// the post-v0.8.0 review caught: a second instance configured on
/// a non-default port would have its `/mcp` traffic silently
/// routed to the first instance's daemon (direct-loopback
/// bypasses Bearer), executing under the wrong agent's namespace
/// and bypassing OAuth.
pub async fn run(
    agent_name: &str,
    daemon_host: Option<&str>,
    daemon_port: Option<u16>,
) -> Result<()> {
    let (resolved_host, resolved_port) =
        resolve_daemon_endpoint(agent_name, daemon_host, daemon_port)?;
    let daemon_url = format!("http://{resolved_host}:{resolved_port}/mcp");

    info!(agent = agent_name, daemon_url = %daemon_url, "mcp-proxy initialized (thin transport)");

    let client = reqwest::Client::new();
    let mut mcp_session_id: Option<String> = None;

    // Spawn a blocking stdin reader so the tokio runtime never stalls
    // on a slow / silent coding-CLI session.
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

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let error_resp = make_error_response(None, -32700, &format!("parse error: {e}"));
                write_response(&mut stdout, &error_resp, framing)?;
                continue;
            }
        };

        let request_id = request.get("id").cloned();
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();

        // Build the HTTP request: forward the JSON-RPC body verbatim,
        // attach the cached `Mcp-Session-Id` if we have one. No auth
        // headers — the daemon trusts localhost callers.
        let mut http_req = client
            .post(&daemon_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&request);
        if let Some(sid) = &mcp_session_id {
            http_req = http_req.header("Mcp-Session-Id", sid);
        }

        let response = match http_req.send().await {
            Ok(resp) => {
                // Capture the daemon's session id (for protocol
                // compliance — the daemon issues one on initialize and
                // echoes it back, but doesn't validate; we still pass
                // it through so the coding CLI sees a well-formed
                // session lifecycle).
                if let Some(sid) = resp.headers().get("Mcp-Session-Id") {
                    if let Ok(s) = sid.to_str() {
                        let new_id = s.to_string();
                        match mcp_session_id.as_deref() {
                            None => {
                                debug!(
                                    session_id = %new_id,
                                    method = %method,
                                    "mcp-proxy: captured Mcp-Session-Id from daemon"
                                );
                            }
                            Some(prev) if prev != new_id => {
                                debug!(
                                    previous = %prev,
                                    session_id = %new_id,
                                    method = %method,
                                    "mcp-proxy: Mcp-Session-Id changed mid-session"
                                );
                            }
                            _ => {}
                        }
                        mcp_session_id = Some(new_id);
                    }
                }

                if resp.status().is_success() {
                    let raw = resp.text().await.unwrap_or_default();
                    serde_json::from_str(&raw).unwrap_or_else(|e| {
                        make_error_response(
                            request_id.clone(),
                            -32603,
                            &format!("invalid response from daemon: {e}"),
                        )
                    })
                } else {
                    make_error_response(
                        request_id.clone(),
                        -32603,
                        &format!("daemon returned HTTP {}", resp.status()),
                    )
                }
            }
            Err(e) => make_error_response(
                request_id.clone(),
                -32603,
                &format!(
                    "agent daemon unreachable at {daemon_url}: {e}. \
                     The daemon may not be running — try `gosh agent start` \
                     in another shell."
                ),
            ),
        };

        // JSON-RPC notifications carry no id; the spec says servers
        // must not respond. Skip writing.
        if method.starts_with("notifications/") {
            continue;
        }

        write_response(&mut stdout, &response, framing)?;
    }

    Ok(())
}

// ── Framing ──────────────────────────────────────────────────────────────

/// Resolve the daemon HTTP endpoint the proxy should dial. Explicit
/// CLI args win when present; otherwise read `GlobalConfig` for
/// `agent_name` and pull host/port from there (with bind→client host
/// normalisation via `client_host_for_local`). When the config also
/// has no explicit value, fall back to `127.0.0.1:8767` to match
/// the pre-existing default behaviour.
fn resolve_daemon_endpoint(
    agent_name: &str,
    daemon_host: Option<&str>,
    daemon_port: Option<u16>,
) -> Result<(String, u16)> {
    if let (Some(host), Some(port)) = (daemon_host, daemon_port) {
        return Ok((super::net::client_host_for_local(host), port));
    }

    // At least one is missing — load GlobalConfig and use it as the
    // source of truth. Any value the operator did pass on the CLI
    // still wins (defence-in-depth: explicit args override config).
    let cfg = super::config::GlobalConfig::load(agent_name).with_context(|| {
        format!(
            "no --daemon-host / --daemon-port and no GlobalConfig for agent '{agent_name}'. \
             Run `gosh agent setup --instance {agent_name}` to write the per-instance \
             config the proxy reads."
        )
    })?;

    let host_raw =
        daemon_host.map(str::to_string).or(cfg.host).unwrap_or_else(|| "127.0.0.1".to_string());
    let port = daemon_port.or(cfg.port).unwrap_or(8767);
    Ok((super::net::client_host_for_local(&host_raw), port))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    ContentLength,
    Newline,
}

/// Peek at first bytes to detect if the client uses Content-Length framing.
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

/// Lock the per-session output framing to whatever the first message
/// arrived in. Mixing framings within one session is undefined; the
/// coding CLI picks one and sticks with it.
fn session_output_framing(
    session_framing: &mut Option<Framing>,
    incoming_framing: Framing,
) -> Framing {
    *session_framing.get_or_insert(incoming_framing)
}

fn make_error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

#[cfg(test)]
mod tests {
    use super::session_output_framing;
    use super::Framing;

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
