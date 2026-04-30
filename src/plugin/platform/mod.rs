// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

pub mod claude;
pub mod codex;
pub mod gemini;

use std::time::Duration;

use anyhow::bail;
use anyhow::Result;
use serde_json::Value;

/// Content extracted from a hook event.
pub struct CapturedContent {
    pub session_id: String,
    pub content: String,
    pub transcript_path: Option<String>,
}

/// Parse hook stdin and extract content based on platform + event.
pub async fn extract(
    agent_name: &str,
    platform: &str,
    event: &str,
    stdin_json: &Value,
) -> Result<CapturedContent> {
    match platform {
        "claude" => claude::extract(agent_name, event, stdin_json).await,
        "codex" => codex::extract(agent_name, event, stdin_json).await,
        "gemini" => gemini::extract(event, stdin_json).await,
        _ => bail!("unknown platform: {platform}"),
    }
}

/// Default delay between the first transcript read and the retry, when the
/// first read returns empty. 500 ms is empirically enough for Claude Code
/// to flush a freshly-completed assistant turn under typical loads
/// (observed window: tens of milliseconds; cushion to be safe).
pub(super) const TRANSCRIPT_FLUSH_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Read assistant content from a transcript with one retry on empty result.
///
/// **Why this exists.** Claude Code (and Codex CLI, by symmetry of the
/// hook contract) fires the Stop hook the moment a turn ends. The hook
/// process can start reading the transcript file before the CLI has
/// finished flushing the latest assistant entry to disk — observed gap
/// is on the order of tens of milliseconds, but races are races. When
/// that happens, the first read returns no assistant text, capture's
/// empty-content guard skips `memory_write`, and a real assistant turn
/// is silently lost. (Symptom seen for short single-token replies in
/// <gosh.agent> v0.5.1; see CHANGELOG.)
///
/// `read` is the per-platform transcript parser (e.g. `diff_transcript`).
/// It stays synchronous — `std::fs::read` underneath. We only need
/// async for the sleep itself: this helper runs inside the tokio
/// runtime that drives `pub async fn extract`, so blocking the worker
/// thread with `std::thread::sleep` would stall any other in-flight
/// task on the same worker. `tokio::time::sleep` yields cleanly.
///
/// The retry delay is short enough to not annoy operators on real empty
/// responses (assistant only invoked tools, nothing to capture) and
/// long enough to clear the race in practice.
pub(super) async fn read_with_flush_retry<F>(read: F) -> Result<String>
where
    F: Fn() -> Result<String>,
{
    let first = read()?;
    if !first.is_empty() {
        return Ok(first);
    }
    tokio::time::sleep(TRANSCRIPT_FLUSH_RETRY_DELAY).await;
    read()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::*;

    // `start_paused = true` makes `tokio::time::sleep` advance the
    // virtual clock instead of actually sleeping. The retry tests
    // therefore complete in microseconds, not 500 ms — fast and
    // deterministic, no flake.

    #[tokio::test(start_paused = true)]
    async fn read_with_flush_retry_returns_first_read_when_non_empty() {
        // Happy path: transcript already flushed by the time the hook
        // fired, parser found assistant text on the first try. The
        // helper must not introduce any delay or extra read.
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = read_with_flush_retry(move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok::<_, anyhow::Error>("found".to_string())
        })
        .await
        .unwrap();
        assert_eq!(result, "found");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "should not retry when first read is non-empty",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_with_flush_retry_retries_once_when_first_read_is_empty() {
        // The race we're guarding against: parser returned "" (Claude
        // hadn't flushed yet), then on retry the assistant entry is
        // present. Closure simulates that by returning "" then content.
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = read_with_flush_retry(move || {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok::<_, anyhow::Error>(String::new())
            } else {
                Ok("flushed after retry".to_string())
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "flushed after retry");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "should retry exactly once on empty first read",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_with_flush_retry_returns_empty_when_both_reads_empty() {
        // Real empty response (assistant only used tools, no text to
        // capture). Both reads return empty; helper returns the second
        // (and final) empty value. Caller's `if content.is_empty()`
        // guard then correctly skips the write.
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let result = read_with_flush_retry(move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(String::new())
        })
        .await
        .unwrap();
        assert_eq!(result, "");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "should retry once even when both reads are empty",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_with_flush_retry_propagates_error_from_first_read() {
        // Errors from the parser (file unreadable, bad path, etc.) are
        // not the race we guard against — they should surface
        // immediately without burning the retry delay.
        let result =
            read_with_flush_retry(|| -> Result<String> { anyhow::bail!("disk on fire") }).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("disk on fire"), "got: {err}");
    }
}
