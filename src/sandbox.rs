// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SandboxStatus {
    Active,
    NotAvailable,
    Failed(String),
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use landlock::Access;
    use landlock::AccessFs;
    use landlock::PathBeneath;
    use landlock::PathFd;
    use landlock::Ruleset;
    use landlock::RulesetAttr;
    use landlock::RulesetCreatedAttr;
    use landlock::RulesetStatus;
    use landlock::ABI;
    use tracing::info;
    use tracing::warn;

    use super::SandboxStatus;

    pub fn enable(rw_paths: &[String], ro_paths: &[String]) -> SandboxStatus {
        let abi = ABI::V5;
        let access_all = AccessFs::from_all(abi);
        if access_all.is_empty() {
            return SandboxStatus::NotAvailable;
        }
        let access_read = AccessFs::from_read(abi) | AccessFs::Execute;

        let mut ruleset =
            match Ruleset::default().handle_access(access_all).and_then(|r| r.create()) {
                Ok(r) => r,
                Err(e) => return SandboxStatus::Failed(format!("create ruleset: {e}")),
            };

        for path in rw_paths {
            if Path::new(path).exists() {
                let fd = match PathFd::new(path) {
                    Ok(fd) => fd,
                    Err(e) => return SandboxStatus::Failed(format!("rw path {path}: {e}")),
                };
                ruleset = match ruleset.add_rule(PathBeneath::new(fd, access_all)) {
                    Ok(r) => r,
                    Err(e) => return SandboxStatus::Failed(format!("rw rule {path}: {e}")),
                };
                info!(path, "sandbox: rw access");
            }
        }

        for path in ro_paths {
            if Path::new(path).exists() {
                let fd = match PathFd::new(path) {
                    Ok(fd) => fd,
                    Err(e) => return SandboxStatus::Failed(format!("ro path {path}: {e}")),
                };
                ruleset = match ruleset.add_rule(PathBeneath::new(fd, access_read)) {
                    Ok(r) => r,
                    Err(e) => return SandboxStatus::Failed(format!("ro rule {path}: {e}")),
                };
                info!(path, "sandbox: ro access");
            }
        }

        match ruleset.restrict_self() {
            Ok(status) => match status.ruleset {
                RulesetStatus::FullyEnforced => {
                    info!("sandbox: active (Landlock ABI v5, fully enforced)");
                    SandboxStatus::Active
                }
                RulesetStatus::PartiallyEnforced => {
                    warn!("sandbox: active (Landlock, partially enforced)");
                    SandboxStatus::Active
                }
                _ => SandboxStatus::NotAvailable,
            },
            Err(e) => SandboxStatus::Failed(format!("restrict_self: {e}")),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod fallback {
    use super::SandboxStatus;

    pub fn enable(_rw_paths: &[String], _ro_paths: &[String]) -> SandboxStatus {
        SandboxStatus::NotAvailable
    }
}

pub fn enable_sandbox(rw_paths: &[String], ro_paths: &[String]) -> SandboxStatus {
    #[cfg(target_os = "linux")]
    return linux::enable(rw_paths, ro_paths);

    #[cfg(not(target_os = "linux"))]
    return fallback::enable(rw_paths, ro_paths);
}

/// Apply the agent-specific sandbox policy.
/// Agent needs: read /usr, /etc, ~/.gosh/agent/config; write buffer + offsets +
/// prompt cache.
pub fn apply_agent_sandbox() {
    let home = dirs::home_dir().unwrap_or_default();
    let gosh_dir = home.join(".gosh");
    let state_dir = gosh_dir.join("agent/state");
    let cache_dir = dirs::cache_dir().unwrap_or_else(|| home.join(".cache")).join("gosh-agent");

    // Ensure writable directories exist before sandbox locks filesystem access.
    let _ = std::fs::create_dir_all(&state_dir);
    let _ = std::fs::create_dir_all(&cache_dir);

    let mut rw_paths = vec![
        state_dir.to_string_lossy().to_string(),
        cache_dir.to_string_lossy().to_string(),
        "/tmp".to_string(),
    ];
    rw_paths.extend(validated_workspace_rw_paths());

    let ro_paths = vec![
        "/usr".to_string(),
        "/etc".to_string(),
        "/lib".to_string(),
        "/lib64".to_string(),
        gosh_dir.join("agent/instances").to_string_lossy().to_string(),
    ];

    match enable_sandbox(&rw_paths, &ro_paths) {
        SandboxStatus::Active => {}
        SandboxStatus::NotAvailable => {
            tracing::info!("sandbox: unavailable, running without isolation");
        }
        SandboxStatus::Failed(e) => {
            tracing::warn!("sandbox: failed to activate: {e}");
        }
    }
}

fn validated_workspace_rw_paths() -> Vec<String> {
    let Some(raw) = std::env::var_os("GOSH_AGENT_WORKSPACE_DIRS") else {
        return Vec::new();
    };
    std::env::split_paths(&raw)
        .filter_map(|path| {
            if path.as_os_str().is_empty() {
                return None;
            }
            if !path.exists() || !path.is_dir() {
                tracing::warn!(
                    path = %path.display(),
                    "sandbox: failed to activate: workspace path is not an existing directory"
                );
                // Workspace allowlists come from the agent process environment before Landlock
                // is active. An invalid entry means the process cannot safely
                // guarantee local_cli workspace access, so fail closed at
                // startup instead of running unsandboxed.
                std::process::exit(78);
            }
            match path.canonicalize() {
                Ok(path) => Some(path.to_string_lossy().to_string()),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "sandbox: failed to activate: workspace path cannot be resolved"
                    );
                    // See the missing-directory branch above: this is startup configuration, not a
                    // per-task validation path.
                    std::process::exit(78);
                }
            }
        })
        .collect()
}
