// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

/// Per-instance config: ~/.gosh/agent/state/{name}/config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub authority_url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub principal_auth_token: Option<String>,
    pub install_id: String,
    /// Memory namespace key for this agent instance.
    /// Takes priority over project-level .gosh-memory.toml.
    #[serde(default)]
    pub key: Option<String>,
    /// Swarm ID for captured data. Required for swarm-shared scope.
    #[serde(default)]
    pub swarm_id: Option<String>,
}

/// Per-project config: .gosh-memory.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub key: String,
}

/// Base directory: ~/.gosh/agent/
fn base_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")).join(".gosh").join("agent")
}

impl GlobalConfig {
    pub fn path(agent_name: &str) -> PathBuf {
        state_dir(agent_name).join("config.toml")
    }

    pub fn load(agent_name: &str) -> Result<Self> {
        let path = Self::path(agent_name);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config at {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self, agent_name: &str) -> Result<()> {
        let path = Self::path(agent_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Resolve memory key for current working directory.
///
/// 1. Walk up from cwd looking for .gosh-memory.toml -> use key
/// 2. Walk up looking for .git/ -> hash(git remote URL) if remote exists
/// 3. No remote -> hash(repo root path) + warn
/// 4. No git -> error
pub fn resolve_key(cwd: &Path) -> Result<String> {
    if let Some(key) = find_project_key(cwd) {
        return Ok(key);
    }

    if let Some(repo_root) = find_git_root(cwd) {
        if let Some(remote_url) = git_remote_url(&repo_root) {
            return Ok(short_hash(&remote_url));
        }
        let hash = short_hash(&repo_root.to_string_lossy());
        tracing::warn!(
            "no git remote found — using path-based key '{}' (not stable across machines)",
            hash
        );
        return Ok(hash);
    }

    bail!(
        "no .gosh-memory.toml or git repo found. Run `gosh-agent setup` \
         or create .gosh-memory.toml with key = \"your-project\""
    )
}

fn find_project_key(start: &Path) -> Option<String> {
    let mut dir = start;
    loop {
        let candidate = dir.join(".gosh-memory.toml");
        if candidate.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                if let Ok(cfg) = toml::from_str::<ProjectConfig>(&text) {
                    return Some(cfg.key);
                }
            }
        }
        dir = dir.parent()?;
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

fn git_remote_url(repo_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    None
}

fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    hash.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// State directory for offsets, buffers, etc. — per agent instance.
pub fn state_dir(agent_name: &str) -> PathBuf {
    base_dir().join("state").join(agent_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_hash_deterministic() {
        let a = short_hash("https://github.com/example/repo.git");
        let b = short_hash("https://github.com/example/repo.git");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn test_short_hash_different_inputs() {
        assert_ne!(short_hash("repo-a"), short_hash("repo-b"));
    }

    #[test]
    fn test_find_project_key_in_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_project_key(tmp.path()).is_none());

        std::fs::write(tmp.path().join(".gosh-memory.toml"), "key = \"test-project\"\n").unwrap();
        assert_eq!(find_project_key(tmp.path()), Some("test-project".to_string()));
    }
}
