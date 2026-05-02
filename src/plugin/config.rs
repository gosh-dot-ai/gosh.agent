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
///
/// This is the single source of truth for everything about an agent
/// instance. `gosh agent setup` is the canonical writer (via
/// `gosh-agent setup`), and re-running setup with a subset of flags
/// patches the existing values rather than overwriting them, so
/// configuration stays atomic across the agent's lifetime.
///
/// `gosh agent start` and the autostart artifact (`launchd plist` /
/// `systemd user unit`) just spawn the daemon; the daemon reads
/// everything below at startup.
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
    /// Daemon HTTP bind host. `None` falls back to `127.0.0.1` at startup.
    #[serde(default)]
    pub host: Option<String>,
    /// Daemon HTTP bind port. `None` falls back to `8767` at startup.
    #[serde(default)]
    pub port: Option<u16>,
    /// Watch loop on/off. When `true`, the daemon's task watcher subscribes
    /// to memory's courier and dispatches inbound tasks. Independent of MCP
    /// gateway operation — capture/MCP work whether watch is on or off.
    #[serde(default)]
    pub watch: bool,
    /// Namespace key the watcher subscribes to for task discovery.
    #[serde(default)]
    pub watch_key: Option<String>,
    /// Swarm filter for the watcher's courier subscription.
    #[serde(default)]
    pub watch_swarm_id: Option<String>,
    /// Agent-id filter for the watcher (default: derived from principal_id).
    #[serde(default)]
    pub watch_agent_id: Option<String>,
    /// Context retrieval namespace, distinct from `watch_key` when an agent
    /// watches one namespace and recalls context from another.
    #[serde(default)]
    pub watch_context_key: Option<String>,
    /// USD budget cap for autonomous task execution (per-task).
    #[serde(default)]
    pub watch_budget: Option<f64>,
    /// Polling interval (seconds) for the watcher loop fallback when courier
    /// SSE is unavailable.
    #[serde(default)]
    pub poll_interval: Option<u64>,
    /// Whether the daemon's OAuth authorization-server side accepts
    /// Dynamic Client Registration (RFC 7591) requests at
    /// `/oauth/register`. When `false`, that endpoint returns
    /// `405 Method Not Allowed` and the metadata document at
    /// `/.well-known/oauth-authorization-server` omits
    /// `registration_endpoint` so Claude.ai's auto-detection falls
    /// back to expecting manually-issued credentials.
    ///
    /// Defaults to `true` — DCR is the spec-default UX path. Operator
    /// opts out via `gosh agent setup --no-oauth-dcr` when they want
    /// explicit per-client registration through
    /// `gosh agent oauth clients register --name <X> --redirect-uri <URI>`.
    #[serde(default = "default_true")]
    pub oauth_dcr_enabled: bool,
    /// Operator-facing daemon log level. `RUST_LOG` still wins when set.
    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,
}

/// Serde default helper for bool fields whose absence means `true`.
/// `#[serde(default)]` alone would default to `false`, which is the
/// wrong fallback for opt-in-by-default fields like `oauth_dcr_enabled`.
fn default_true() -> bool {
    true
}

fn default_log_level() -> LogLevel {
    LogLevel::Info
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(format!(
                "unknown log level '{other}'; expected one of error, warn, info, debug, trace"
            )),
        }
    }
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

    /// `gosh-agent serve` re-reads `GlobalConfig` on every startup
    /// (fresh launch, daemon respawn after a crash, autostart on
    /// boot, `gosh agent restart`), so the flag the operator set
    /// with `gosh agent setup --no-oauth-dcr` must round-trip
    /// through TOML untouched. A regression in `#[serde(default
    /// = "default_true")]` or in the manual `Serialize` would
    /// silently re-enable DCR on the next daemon restart — exactly
    /// the kind of change that's painless to merge and impossible
    /// to spot in production logs.
    #[test]
    fn oauth_dcr_disabled_round_trips_through_save_and_load() {
        let cfg = GlobalConfig {
            authority_url: "http://127.0.0.1:8765".to_string(),
            token: None,
            principal_auth_token: None,
            install_id: "test".to_string(),
            key: None,
            swarm_id: None,
            host: None,
            port: None,
            watch: false,
            watch_key: None,
            watch_swarm_id: None,
            watch_agent_id: None,
            watch_context_key: None,
            watch_budget: None,
            poll_interval: None,
            oauth_dcr_enabled: false,
            log_level: LogLevel::Info,
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let loaded: GlobalConfig = toml::from_str(&text).unwrap();
        assert!(
            !loaded.oauth_dcr_enabled,
            "oauth_dcr_enabled=false must survive save+load — daemon \
             restart re-reads GlobalConfig and mustn't silently flip \
             DCR back on. Got config TOML:\n{text}",
        );
    }

    #[test]
    fn legacy_global_config_without_oauth_dcr_field_defaults_to_enabled() {
        // Older config files (pre-7a) didn't carry `oauth_dcr_enabled`
        // at all. Loading one of those into the post-7a struct must
        // default to DCR ON — the spec's UX-default. A bare
        // `#[serde(default)]` would default to `false` and silently
        // disable DCR on every legacy install; pin the helper.
        let legacy = r#"
            authority_url = "http://127.0.0.1:8765"
            install_id = "x"
        "#;
        let parsed: GlobalConfig = toml::from_str(legacy).unwrap();
        assert!(
            parsed.oauth_dcr_enabled,
            "missing oauth_dcr_enabled in legacy TOML must default to true \
             (DCR on) — this is the helper-function regression guard"
        );
    }

    #[test]
    fn legacy_global_config_without_log_level_defaults_to_info() {
        let legacy = r#"
            authority_url = "http://127.0.0.1:8765"
            install_id = "x"
        "#;
        let parsed: GlobalConfig = toml::from_str(legacy).unwrap();
        assert_eq!(parsed.log_level, LogLevel::Info);
    }

    #[test]
    fn log_level_parses_case_insensitively() {
        assert_eq!("debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("WARN".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert!("verbose".parse::<LogLevel>().is_err());
    }
}
