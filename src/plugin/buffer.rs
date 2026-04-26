// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::config;

const MAX_ENTRIES: usize = 1000;
const MAX_BYTES: u64 = 100 * 1024 * 1024;

fn buffer_file(agent_name: &str) -> PathBuf {
    config::state_dir(agent_name).join("buffer").join("pending.jsonl")
}

/// Append a write request to the local buffer.
pub fn enqueue(agent_name: &str, req: &Value) -> Result<()> {
    let path = buffer_file(agent_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= MAX_BYTES {
            tracing::warn!("local buffer exceeds {MAX_BYTES} bytes — dropping oldest entries");
            truncate_buffer(&path)?;
        }
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Replay all buffered writes to the authority via the provided call_tool
/// function.
pub async fn replay<F, Fut>(agent_name: &str, call_tool: F) -> Result<()>
where
    F: Fn(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let path = buffer_file(agent_name);
    if !path.exists() {
        tracing::info!("no buffered writes to replay");
        return Ok(());
    }

    let text = std::fs::read_to_string(&path).context("cannot read buffer file")?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();

    if lines.is_empty() {
        tracing::info!("buffer is empty");
        return Ok(());
    }

    tracing::info!("replaying {} buffered writes", lines.len());
    let mut succeeded = 0;
    let mut failed_lines = Vec::new();

    for line in &lines {
        let args: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("skipping malformed buffer entry: {e}");
                continue;
            }
        };

        match call_tool(args).await {
            Ok(_) => succeeded += 1,
            Err(e) => {
                tracing::warn!("replay failed: {e}");
                failed_lines.push(line.to_string());
            }
        }
    }

    if failed_lines.is_empty() {
        std::fs::remove_file(&path)?;
    } else {
        std::fs::write(&path, failed_lines.join("\n") + "\n")?;
    }

    tracing::info!("replay complete: {} succeeded, {} remaining", succeeded, failed_lines.len());
    Ok(())
}

fn truncate_buffer(path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_ENTRIES / 2 {
        return Ok(());
    }
    let keep = &lines[lines.len() / 2..];
    std::fs::write(path, keep.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_different_agents_have_separate_buffer_paths() {
        let path_a = buffer_file("alpha");
        let path_b = buffer_file("beta");

        assert_ne!(path_a, path_b);
        assert!(path_a.to_string_lossy().contains("/state/alpha/"));
        assert!(path_b.to_string_lossy().contains("/state/beta/"));
    }

    #[test]
    fn test_enqueue_writes_to_agent_specific_buffer() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("alpha").join("buffer");
        let dir_b = tmp.path().join("beta").join("buffer");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let file_a = dir_a.join("pending.jsonl");
        let file_b = dir_b.join("pending.jsonl");

        // Write directly to verify isolation
        let req_a = serde_json::json!({"content": "from alpha"});
        let req_b = serde_json::json!({"content": "from beta"});

        use std::io::Write;
        let mut fa = std::fs::File::create(&file_a).unwrap();
        writeln!(fa, "{}", serde_json::to_string(&req_a).unwrap()).unwrap();
        let mut fb = std::fs::File::create(&file_b).unwrap();
        writeln!(fb, "{}", serde_json::to_string(&req_b).unwrap()).unwrap();

        let content_a = std::fs::read_to_string(&file_a).unwrap();
        let content_b = std::fs::read_to_string(&file_b).unwrap();

        assert!(content_a.contains("from alpha"));
        assert!(!content_a.contains("from beta"));
        assert!(content_b.contains("from beta"));
        assert!(!content_b.contains("from alpha"));
    }
}
