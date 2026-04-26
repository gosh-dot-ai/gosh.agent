// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::io::Read;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use sha2::Digest;
use sha2::Sha256;

use super::buffer;
use super::config;
use super::config::GlobalConfig;
use super::offset;
use super::platform;
use crate::client::transport::HttpTransport;
use crate::client::McpClient;

/// Main entry point for `gosh-agent capture`.
pub async fn run(agent_name: &str, platform_name: &str, event: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("failed to read stdin")?;

    let stdin_json: serde_json::Value =
        serde_json::from_str(&input).context("stdin is not valid JSON")?;

    let global_config = GlobalConfig::load(agent_name)?;

    let key = match global_config.key {
        Some(ref k) => k.clone(),
        None => {
            let cwd = std::env::current_dir()?;
            config::resolve_key(&cwd)?
        }
    };

    let captured = platform::extract(agent_name, platform_name, event, &stdin_json).await?;
    if captured.content.is_empty() {
        tracing::debug!("empty content — skipping write");
        return Ok(());
    }

    let mut session_offset = offset::load(agent_name, &captured.session_id);

    let (turn_number, event_suffix) = match event {
        "prompt" => {
            session_offset.turn_count += 1;
            (session_offset.turn_count, "prompt")
        }
        "response" => (session_offset.turn_count, "response"),
        _ => (session_offset.turn_count, event),
    };

    let message_id = deterministic_id(&captured.session_id, turn_number, event_suffix);
    let timestamp_ms =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let agent_id = agent_name.to_string();

    let args = build_write_payload(&WritePayloadParams {
        key: &key,
        message_id: &message_id,
        timestamp_ms,
        content: &captured.content,
        session_id: &captured.session_id,
        agent_id: &agent_id,
        swarm_id: global_config.swarm_id.as_deref(),
        event,
        turn_number,
        platform_name,
    });
    // Use existing McpClient infrastructure (handles initialize + session + SSE)
    let url = global_config.authority_url.trim_end_matches('/');
    let principal_auth_token = global_config
        .principal_auth_token
        .clone()
        .or_else(|| std::env::var("GOSH_MEMORY_AUTH_TOKEN").ok());
    let transport = HttpTransport::new(url, global_config.token.clone(), principal_auth_token);
    let client = McpClient::new(transport, "gosh-agent-plugin");

    match client.call_tool("memory_write", args.clone()).await {
        Ok(resp) => {
            let state = resp.get("extraction_state").and_then(|v| v.as_str()).unwrap_or("unknown");
            tracing::info!("captured {event} message_id={message_id} extraction_state={state}");

            // Advance transcript offset on success
            if event == "response" {
                if let Some(ref tp) = captured.transcript_path {
                    if let Ok(meta) = std::fs::metadata(tp) {
                        session_offset.byte_offset = meta.len();
                    }
                }
            }
            offset::save(agent_name, &captured.session_id, &session_offset)?;
        }
        Err(e) => {
            tracing::warn!("authority unreachable, buffering locally: {e}");
            buffer::enqueue(agent_name, &args)?;

            if event == "prompt" {
                offset::save(agent_name, &captured.session_id, &session_offset)?;
            }
        }
    }

    Ok(())
}

struct WritePayloadParams<'a> {
    key: &'a str,
    message_id: &'a str,
    timestamp_ms: i64,
    content: &'a str,
    session_id: &'a str,
    agent_id: &'a str,
    swarm_id: Option<&'a str>,
    event: &'a str,
    turn_number: u32,
    platform_name: &'a str,
}

fn build_write_payload(p: &WritePayloadParams) -> serde_json::Value {
    let scope = if p.swarm_id.is_some() { "swarm-shared" } else { "agent-private" };

    let mut args = serde_json::json!({
        "key": p.key,
        "message_id": p.message_id,
        "timestamp_ms": p.timestamp_ms,
        "content": p.content,
        "content_family": "chat",
        "session_id": p.session_id,
        "agent_id": p.agent_id,
        "scope": scope,
        "metadata": {
            "role": if p.event == "prompt" { "user" } else { "assistant" },
            "turn_number": p.turn_number.to_string(),
            "platform": p.platform_name,
        }
    });
    if let Some(sid) = p.swarm_id {
        args["swarm_id"] = serde_json::json!(sid);
    }
    args
}

/// sha256(session_id + ":" + turn_number + ":" + event)[:32]
fn deterministic_id(session_id: &str, turn_number: u32, event: &str) -> String {
    let input = format!("{session_id}:{turn_number}:{event}");
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    hash.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_id_stable() {
        let a = deterministic_id("session-abc", 1, "prompt");
        let b = deterministic_id("session-abc", 1, "prompt");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn test_deterministic_id_varies() {
        let prompt = deterministic_id("s", 1, "prompt");
        let response = deterministic_id("s", 1, "response");
        assert_ne!(prompt, response);
    }

    #[test]
    fn payload_swarm_shared_when_swarm_configured() {
        let payload = super::build_write_payload(&super::WritePayloadParams {
            key: "ns",
            message_id: "msg-1",
            timestamp_ms: 1000,
            content: "hello",
            session_id: "sess-1",
            agent_id: "my-agent",
            swarm_id: Some("team-alpha"),
            event: "prompt",
            turn_number: 1,
            platform_name: "claude",
        });
        assert_eq!(payload["scope"], "swarm-shared");
        assert_eq!(payload["swarm_id"], "team-alpha");
        assert_eq!(payload["agent_id"], "my-agent");
        assert!(!payload["agent_id"].as_str().unwrap().contains(':'));
    }

    #[test]
    fn payload_agent_private_when_no_swarm() {
        let payload = super::build_write_payload(&super::WritePayloadParams {
            key: "ns",
            message_id: "msg-1",
            timestamp_ms: 1000,
            content: "hello",
            session_id: "sess-1",
            agent_id: "my-agent",
            swarm_id: None,
            event: "response",
            turn_number: 2,
            platform_name: "codex",
        });
        assert_eq!(payload["scope"], "agent-private");
        assert!(payload.get("swarm_id").is_none());
        assert_eq!(payload["agent_id"], "my-agent");
    }

    #[test]
    fn payload_metadata_role_matches_event() {
        let prompt = super::build_write_payload(&super::WritePayloadParams {
            key: "ns",
            message_id: "msg-1",
            timestamp_ms: 1000,
            content: "hi",
            session_id: "s",
            agent_id: "a",
            swarm_id: None,
            event: "prompt",
            turn_number: 1,
            platform_name: "claude",
        });
        let response = super::build_write_payload(&super::WritePayloadParams {
            key: "ns",
            message_id: "msg-2",
            timestamp_ms: 1000,
            content: "ok",
            session_id: "s",
            agent_id: "a",
            swarm_id: None,
            event: "response",
            turn_number: 1,
            platform_name: "claude",
        });
        assert_eq!(prompt["metadata"]["role"], "user");
        assert_eq!(response["metadata"]["role"], "assistant");
    }
}
