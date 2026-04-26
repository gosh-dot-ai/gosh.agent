// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::CapturedContent;
use crate::plugin::offset;

/// Extract content from Claude Code hooks.
///
/// UserPromptSubmit stdin: { "session_id", "prompt", "transcript_path" }
/// Stop stdin: { "session_id", "transcript_path", "stop_reason" }
pub async fn extract(agent_name: &str, event: &str, stdin_json: &Value) -> Result<CapturedContent> {
    match event {
        "prompt" => extract_prompt(stdin_json),
        "response" => extract_response(agent_name, stdin_json).await,
        _ => bail!("unknown event for claude: {event}"),
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

    // Wrap the per-platform parser in `read_with_flush_retry`: Claude Code
    // can fire the Stop hook before the latest assistant entry is flushed
    // to disk, leaving us reading a "stale" file and silently dropping the
    // turn. Helper retries once after a short delay if the first read came
    // back empty. See `super::read_with_flush_retry` for the full rationale.
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
            if !is_assistant_entry(&entry) {
                continue;
            }
            if let Some(content) = entry.get("message").and_then(|m| m.get("content")) {
                if let Some(text) = extract_text_from_content(content) {
                    assistant_parts.push(text);
                }
            } else if let Some(content) = entry.get("content") {
                if let Some(text) = extract_text_from_content(content) {
                    assistant_parts.push(text);
                }
            }
        }
    }

    Ok(assistant_parts.join("\n"))
}

fn is_assistant_entry(entry: &Value) -> bool {
    // Claude Code has changed transcript shapes over time. Keep this predicate
    // deliberately forgiving so capture does not silently drop response turns
    // when one assistant marker moves between top-level and message fields.
    entry.get("role").and_then(|r| r.as_str()) == Some("assistant")
        || entry.get("message").and_then(|m| m.get("role")).and_then(|r| r.as_str())
            == Some("assistant")
        || entry.get("type").and_then(|t| t.as_str()) == Some("assistant")
}

fn extract_text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|item| {
                    if item.get("type")?.as_str()? == "text" {
                        item.get("text")?.as_str()
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn diff_transcript_extracts_current_claude_code_assistant_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("claude.jsonl");
        std::fs::write(
            &transcript,
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "248911, 248912."}]
                }
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let text = diff_transcript(
            "claude-current-shape-test",
            "session-current-shape-test",
            transcript.to_str().unwrap(),
        )
        .unwrap();

        assert_eq!(text, "248911, 248912.");
    }

    #[test]
    fn diff_transcript_extracts_legacy_top_level_role_assistant_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("claude.jsonl");
        std::fs::write(
            &transcript,
            serde_json::json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "legacy response"}]
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let text = diff_transcript(
            "claude-legacy-shape-test",
            "session-legacy-shape-test",
            transcript.to_str().unwrap(),
        )
        .unwrap();

        assert_eq!(text, "legacy response");
    }

    // Integration-style regression for the Stop-hook flush race. The
    // mock-closure tests in `super::tests` cover the helper's branching;
    // this one wires `read_with_flush_retry` to the real
    // `diff_transcript` parser against a real on-disk transcript so the
    // exact failure mode that motivated the fix (first read sees a
    // not-yet-flushed file, second read sees the assistant entry) is
    // pinned end-to-end.
    #[tokio::test(start_paused = true)]
    async fn read_with_flush_retry_recovers_assistant_entry_flushed_between_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("claude.jsonl");
        // Initial state: file exists but the CLI hasn't flushed the
        // turn yet — what the Stop hook actually sees in the race.
        std::fs::write(&transcript, "").unwrap();

        let path = transcript.to_str().unwrap().to_string();
        let agent_name = "claude-flush-race-test";
        let session_id = "session-flush-race-test";

        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "late flush"}]
            }
        })
        .to_string()
            + "\n";

        let calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = Arc::clone(&calls);
        let path_for_closure = path.clone();

        let content = crate::plugin::platform::read_with_flush_retry(move || {
            let n = calls_for_closure.fetch_add(1, Ordering::SeqCst);
            // Between the first failed read and the helper's retry,
            // the CLI finishes flushing the assistant entry. Modeling
            // that by appending here, before the retry's actual read,
            // is what makes this a real race regression rather than a
            // closure-counting test.
            if n == 1 {
                std::fs::write(&path_for_closure, &entry).unwrap();
            }
            diff_transcript(agent_name, session_id, &path_for_closure)
        })
        .await
        .unwrap();

        assert_eq!(content, "late flush");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "helper should have retried exactly once after the empty first read",
        );
    }
}
