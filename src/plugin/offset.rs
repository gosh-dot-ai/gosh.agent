// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use super::config;

/// Tracks transcript offset and turn count per session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionOffset {
    pub byte_offset: u64,
    pub turn_count: u32,
}

fn offset_path(agent_name: &str, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    config::state_dir(agent_name).join("offsets").join(format!("{safe}.json"))
}

pub fn load(agent_name: &str, session_id: &str) -> SessionOffset {
    let path = offset_path(agent_name, session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(agent_name: &str, session_id: &str, offset: &SessionOffset) -> Result<()> {
    let path = offset_path(agent_name, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string(offset)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_different_agents_have_separate_offset_paths() {
        let path_a = offset_path("alpha", "sess-1");
        let path_b = offset_path("beta", "sess-1");

        assert_ne!(path_a, path_b);
        assert!(path_a.to_string_lossy().contains("/state/alpha/"));
        assert!(path_b.to_string_lossy().contains("/state/beta/"));
    }
}
