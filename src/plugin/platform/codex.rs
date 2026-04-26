// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::CapturedContent;
use crate::plugin::offset;

/// Extract content from Codex CLI hooks (same model as Claude Code).
pub async fn extract(agent_name: &str, event: &str, stdin_json: &Value) -> Result<CapturedContent> {
    match event {
        "prompt" => extract_prompt(stdin_json),
        "response" => extract_response(agent_name, stdin_json).await,
        _ => bail!("unknown event for codex: {event}"),
    }
}

fn extract_prompt(input: &Value) -> Result<CapturedContent> {
    let session_id = input["session_id"].as_str().context("missing session_id")?.to_string();
    let prompt = input["prompt"].as_str().context("missing prompt")?.to_string();

    Ok(CapturedContent {
        session_id,
        content: prompt,
        transcript_path: input["transcript_path"].as_str().map(String::from),
    })
}

async fn extract_response(agent_name: &str, input: &Value) -> Result<CapturedContent> {
    let session_id = input["session_id"].as_str().context("missing session_id")?.to_string();
    let transcript_path = input["transcript_path"].as_str().context("missing transcript_path")?;

    // Same race guard as the Claude path: Codex CLI can fire the Stop hook
    // before the assistant entry is flushed to the transcript file. The
    // observable bug was on Claude, but the architecture is symmetric —
    // both platforms read the transcript synchronously on Stop. See
    // `super::read_with_flush_retry`.
    let content =
        super::read_with_flush_retry(|| diff_transcript(agent_name, &session_id, transcript_path))
            .await?;

    Ok(CapturedContent { session_id, content, transcript_path: Some(transcript_path.to_string()) })
}

fn diff_transcript(agent_name: &str, session_id: &str, transcript_path: &str) -> Result<String> {
    let data = std::fs::read(transcript_path)
        .with_context(|| format!("cannot read transcript at {transcript_path}"))?;

    let off = offset::load(agent_name, session_id);
    let start = off.byte_offset as usize;
    if start >= data.len() {
        return Ok(String::new());
    }

    let new_text = String::from_utf8_lossy(&data[start..]);
    let mut assistant_parts = Vec::new();

    for line in new_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            if entry.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                if let Some(Value::String(text)) = entry.get("content") {
                    assistant_parts.push(text.clone());
                } else if let Some(Value::Array(arr)) = entry.get("content") {
                    for item in arr {
                        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                assistant_parts.push(t.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(assistant_parts.join("\n"))
}
