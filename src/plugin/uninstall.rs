// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::Result;

use super::autostart;
use super::config::state_dir;
use super::setup;

/// Tear down `agent_name`'s autostart, hooks/MCP, and per-instance state.
pub async fn run(agent_name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;

    // Stop & remove the supervised autostart artifact first so the daemon
    // isn't holding the state dir open while we try to remove it.
    if let Err(e) = autostart::uninstall(agent_name) {
        eprintln!("autostart cleanup: {e}");
    }

    // Strip hooks + MCP for every supported coding CLI. `remove_platform`
    // is best-effort per-platform and operates against both user and
    // project scopes (project = current cwd).
    for platform in ["claude", "codex", "gemini"] {
        setup::remove_platform(agent_name, platform, &cwd);
    }

    // Drop per-instance state (`~/.gosh/agent/state/<name>/`).
    let state = state_dir(agent_name);
    if state.exists() {
        match std::fs::remove_dir_all(&state) {
            Ok(()) => eprintln!("Removed state dir at {}", state.display()),
            Err(e) => eprintln!("could not remove {}: {e}", state.display()),
        }
    }

    eprintln!("Uninstalled agent '{agent_name}'.");
    Ok(())
}
