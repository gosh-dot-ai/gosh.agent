// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::CapturedContent;

/// Extract content from Gemini CLI hooks.
///
/// BeforeModel: prompt payload on stdin.
/// AfterModel: response payload directly on stdin (no transcript diff needed).
pub async fn extract(event: &str, stdin_json: &Value) -> Result<CapturedContent> {
    match event {
        "prompt" => extract_prompt(stdin_json),
        "response" => extract_response(stdin_json),
        _ => bail!("unknown event for gemini: {event}"),
    }
}

fn extract_prompt(input: &Value) -> Result<CapturedContent> {
    let session_id =
        input.get("session_id").and_then(|v| v.as_str()).unwrap_or("gemini-default").to_string();

    let content = if let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) {
        prompt.to_string()
    } else if let Some(parts) = input.get("contents").and_then(|c| c.as_array()) {
        extract_last_user_text(parts)
    } else {
        serde_json::to_string(input).context("cannot serialize gemini prompt")?
    };

    Ok(CapturedContent { session_id, content, transcript_path: None })
}

fn extract_response(input: &Value) -> Result<CapturedContent> {
    let session_id =
        input.get("session_id").and_then(|v| v.as_str()).unwrap_or("gemini-default").to_string();

    let content = if let Some(text) = input.get("text").and_then(|v| v.as_str()) {
        text.to_string()
    } else if let Some(candidates) = input.get("candidates").and_then(|c| c.as_array()) {
        extract_candidate_text(candidates)
    } else if let Some(content_val) = input.get("content") {
        extract_parts_text(content_val)
    } else {
        serde_json::to_string(input).context("cannot serialize gemini response")?
    };

    Ok(CapturedContent { session_id, content, transcript_path: None })
}

fn extract_last_user_text(contents: &[Value]) -> String {
    for item in contents.iter().rev() {
        if item.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(parts) = item.get("parts").and_then(|p| p.as_array()) {
                let texts: Vec<&str> =
                    parts.iter().filter_map(|p| p.get("text")?.as_str()).collect();
                if !texts.is_empty() {
                    return texts.join("\n");
                }
            }
        }
    }
    String::new()
}

fn extract_candidate_text(candidates: &[Value]) -> String {
    candidates
        .iter()
        .filter_map(|c| {
            let text = extract_parts_text(c.get("content")?);
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_parts_text(content: &Value) -> String {
    if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
        let texts: Vec<&str> = parts.iter().filter_map(|p| p.get("text")?.as_str()).collect();
        texts.join("\n")
    } else if let Some(text) = content.as_str() {
        text.to_string()
    } else {
        String::new()
    }
}
