// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::config::GlobalConfig;

const VALID_PLATFORMS: &[&str] = &["claude", "codex", "gemini"];

/// Run `gosh-agent setup`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent_name: &str,
    authority_url: Option<&str>,
    token: Option<&str>,
    principal_auth_token: Option<&str>,
    key: Option<&str>,
    swarm_id: Option<&str>,
    platforms: &[String],
    scope: &str,
) -> Result<()> {
    // Validate --platform values
    for p in platforms {
        if !VALID_PLATFORMS.contains(&p.as_str()) {
            anyhow::bail!("unknown platform '{}'. Valid values: {}", p, VALID_PLATFORMS.join(", "));
        }
    }

    let cwd = std::env::current_dir()?;

    // The cwd=/ guard exists because project-scope writes hooks AND/or MCP
    // config rooted at `<cwd>` — Claude refuses to load `.mcp.json` from
    // filesystem root for security, and per-project hook files in `/` are
    // useless anyway. The guard does **not** apply when no project-rooted
    // file would be written, i.e. when `--scope user` is selected (all
    // platforms write to `~/.<platform>/...` then) or when no platform
    // would write project-rooted files for some other reason.
    if writes_project_files_in_cwd(scope, platforms) && cwd.parent().is_none() {
        anyhow::bail!(
            "refusing to run `agent setup` with cwd = `{}` (filesystem root).\n\
             At project scope (the default) we would write hooks and MCP config\n\
             rooted at the current directory; Claude refuses to load `.mcp.json`\n\
             from the root, and per-project hook files at `/` are unusable.\n\
             Either re-run from a project directory (e.g. `mkdir -p ~/my-project\n\
             && cd ~/my-project && gosh agent setup ...`) or pass `--scope user`\n\
             to install user-globally instead — but note `--scope user` makes the\n\
             agent capture every session of the coding CLI on this host.",
            cwd.display(),
        );
    }

    let project_key = resolve_or_prompt_key(key, &cwd)?;

    let global_config = write_global_config(
        agent_name,
        authority_url,
        token,
        principal_auth_token,
        &project_key,
        swarm_id,
    )?;
    eprintln!("Config written to {}", GlobalConfig::path(agent_name).display());

    let binary_path = find_self_binary();

    let mut detected = detect_clis();

    // Filter by --platform if specified
    if !platforms.is_empty() {
        detected.retain(|cli| platforms.iter().any(|p| p == cli));

        // Remove hooks/MCP for platforms not in the selected set.
        // This runs before the empty-detected early return so that cleanup
        // is authoritative even when none of the selected CLIs are installed.
        for p in VALID_PLATFORMS {
            if !platforms.iter().any(|sel| sel == p) {
                remove_platform(agent_name, p, &cwd);
            }
        }
    }

    if detected.is_empty() {
        if platforms.is_empty() {
            eprintln!(
                "No coding CLIs detected (claude, codex, gemini). Install one and re-run setup."
            );
        } else {
            eprintln!("None of the specified platforms ({}) are installed.", platforms.join(", "));
        }
        return Ok(());
    }

    for cli_name in &detected {
        match cli_name.as_str() {
            "claude" => {
                configure_claude_hooks(agent_name, &binary_path, scope, &cwd)?;
                configure_claude_mcp(
                    agent_name,
                    &cwd,
                    &project_key,
                    swarm_id,
                    scope,
                    &binary_path,
                )?;
                eprintln!("Configured Claude Code hooks + MCP (scope: {scope})");
            }
            "codex" => {
                configure_codex_hooks(agent_name, &binary_path, scope, &cwd)?;
                configure_codex_mcp(agent_name, &cwd, &project_key, swarm_id, &binary_path)?;
                if scope == "project" {
                    eprintln!(
                        "Configured Codex CLI hooks (scope: project) + MCP (scope: user — \
                         `codex mcp add` has no per-project mode upstream)"
                    );
                } else {
                    eprintln!("Configured Codex CLI hooks + MCP (scope: {scope})");
                }
            }
            "gemini" => {
                configure_gemini_hooks(agent_name, &binary_path, scope, &cwd)?;
                configure_gemini_mcp(
                    agent_name,
                    &cwd,
                    &project_key,
                    swarm_id,
                    scope,
                    &binary_path,
                )?;
                eprintln!("Configured Gemini CLI hooks + MCP (scope: {scope})");
            }
            _ => {}
        }
    }

    eprintln!("\nSetup complete. Authority: {}", global_config.authority_url);
    match swarm_id {
        Some(s) => eprintln!("Capture scope: swarm-shared (swarm: {s})"),
        None => eprintln!(
            "Capture scope: agent-private — capture stays local to this agent.\n\
             To share with team, re-run with --swarm <swarm_id>"
        ),
    }
    Ok(())
}

fn write_global_config(
    agent_name: &str,
    authority_url: Option<&str>,
    token: Option<&str>,
    principal_auth_token: Option<&str>,
    key: &str,
    swarm_id: Option<&str>,
) -> Result<GlobalConfig> {
    let mut config = GlobalConfig::load(agent_name).unwrap_or_else(|_| GlobalConfig {
        authority_url: "http://127.0.0.1:8765".to_string(),
        token: None,
        principal_auth_token: None,
        install_id: uuid::Uuid::new_v4().to_string(),
        key: None,
        swarm_id: None,
    });

    if let Some(url) = authority_url {
        config.authority_url = url.to_string();
    }
    if let Some(t) = token {
        config.token = Some(t.to_string());
    }
    if let Some(t) = principal_auth_token {
        config.principal_auth_token = Some(t.to_string());
    }
    config.key = Some(key.to_string());
    // Always update swarm_id — None clears it (reverts to agent-private scope)
    config.swarm_id = swarm_id.map(|s| s.to_string());

    config.save(agent_name)?;
    Ok(config)
}

fn resolve_or_prompt_key(explicit_key: Option<&str>, cwd: &Path) -> Result<String> {
    if let Some(k) = explicit_key {
        return Ok(k.to_string());
    }

    super::config::resolve_key(cwd)
}

fn find_self_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "gosh-agent".to_string())
}

fn detect_clis() -> Vec<String> {
    let mut found = Vec::new();
    if which::which("claude").is_ok() || home_dir_join(".claude").is_dir() {
        found.push("claude".to_string());
    }
    if which::which("codex").is_ok() || home_dir_join(".codex").is_dir() {
        found.push("codex".to_string());
    }
    if which::which("gemini").is_ok() || home_dir_join(".gemini").is_dir() {
        found.push("gemini".to_string());
    }
    found
}

fn home_dir_join(name: &str) -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")).join(name)
}

/// True iff this run will write any project-rooted file under `<cwd>`.
/// At project scope (the default), every supported platform writes at
/// least its hook config under `<cwd>/.<platform>/...`, and Claude
/// additionally writes `<cwd>/.mcp.json`. At user scope, all writes
/// go under `~/.<platform>/...` and `<cwd>` is not touched.
///
/// `platforms.is_empty()` means "auto-detect all installed CLIs", which
/// could include any of the three — treat as "project files possible"
/// pessimistically so auto-detect from `/` still fails loudly instead
/// of producing a half install.
fn writes_project_files_in_cwd(scope: &str, platforms: &[String]) -> bool {
    if scope != "project" {
        return false;
    }
    // Any selected (or auto-detected) platform writes at least its
    // hook file under `<cwd>` at project scope.
    platforms.is_empty() || platforms.iter().any(|p| VALID_PLATFORMS.contains(&p.as_str()))
}

// --- Claude Code ---

/// Path to Claude Code's settings.json at the requested scope.
///
/// - `project` (default): `<cwd>/.claude/settings.json`. Hooks here only fire
///   when claude is launched from this directory. This is the privacy-safe
///   default — capture stays scoped to the project.
/// - `user`: `~/.claude/settings.json`. Hooks fire for **every** claude session
///   on this host. Opt-in for users who deliberately want one agent capturing
///   across all their projects.
fn claude_settings_path(scope: &str, cwd: &Path) -> PathBuf {
    match scope {
        "user" => home_dir_join(".claude").join("settings.json"),
        _ => cwd.join(".claude").join("settings.json"),
    }
}

fn configure_claude_hooks(agent_name: &str, binary: &str, scope: &str, cwd: &Path) -> Result<()> {
    let settings_path = claude_settings_path(scope, cwd);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut settings = load_json_or_empty(&settings_path);

    let hooks = settings
        .as_object_mut()
        .context("settings.json root is not object")?
        .entry("hooks")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let hooks_obj = hooks.as_object_mut().context("hooks is not object")?;

    upsert_hook_array(
        hooks_obj,
        "UserPromptSubmit",
        agent_name,
        &format!("{binary} capture --name {agent_name} --platform claude --event prompt"),
    );
    upsert_hook_array(
        hooks_obj,
        "Stop",
        agent_name,
        &format!("{binary} capture --name {agent_name} --platform claude --event response"),
    );

    save_json(&settings_path, &settings)?;

    // Auto-migrate: remove this agent's hooks from the OTHER scope so we
    // don't end up with simultaneous user-level + project-level installs
    // (the original privacy bug). The "other scope" file may not exist;
    // helper is a no-op in that case.
    let other_path = claude_settings_path(other_scope(scope), cwd);
    if other_path != settings_path && other_path.exists() {
        let removed = remove_hooks_for_agent(&other_path, agent_name)?;
        if removed {
            eprintln!(
                "Removed stale `{agent_name}` Claude hooks from {} (superseded by `--scope {scope}`)",
                other_path.display(),
            );
        }
    }

    Ok(())
}

/// Inverse of the requested scope, used when cleaning up the unselected one.
fn other_scope(scope: &str) -> &'static str {
    match scope {
        "user" => "project",
        _ => "user",
    }
}

/// Remove this agent's hook entries (UserPromptSubmit + Stop for claude
/// & codex; BeforeModel + AfterModel for gemini) from a settings.json /
/// hooks.json file. Returns `true` if any entry was removed.
fn remove_hooks_for_agent(path: &Path, agent_name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut settings = load_json_or_empty(path);
    let mut changed = false;
    if let Some(hooks_obj) =
        settings.as_object_mut().and_then(|o| o.get_mut("hooks")).and_then(|h| h.as_object_mut())
    {
        for event in ["UserPromptSubmit", "Stop", "BeforeModel", "AfterModel"] {
            if let Some(arr) = hooks_obj.get_mut(event).and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                arr.retain(|item| !item_matches_agent(item, agent_name));
                if arr.len() != before {
                    changed = true;
                }
            }
        }
    }
    if changed {
        save_json(path, &settings)?;
    }
    Ok(changed)
}

/// Build args for `gosh-agent mcp-proxy` invocation by an LLM CLI.
/// `swarm` is appended as `--default-swarm <id>` only when set, so default
/// (no-swarm) installs stay clean.
fn build_mcp_proxy_args(agent_name: &str, key: &str, swarm: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "mcp-proxy".to_string(),
        "--name".to_string(),
        agent_name.to_string(),
        "--default-key".to_string(),
        key.to_string(),
    ];
    if let Some(s) = swarm {
        args.push("--default-swarm".to_string());
        args.push(s.to_string());
    }
    args
}

fn configure_claude_mcp(
    agent_name: &str,
    cwd: &Path,
    key: &str,
    swarm: Option<&str>,
    mcp_scope: &str,
    binary: &str,
) -> Result<()> {
    match mcp_scope {
        "user" => configure_claude_mcp_user(agent_name, cwd, key, swarm, binary),
        // "project" is the default; any unexpected value is treated as project
        // (clap already restricts valid values, so this is just defensive).
        _ => configure_claude_mcp_project(agent_name, cwd, key, swarm, binary),
    }
}

/// Argument vector for `claude mcp remove -s user gosh-memory-{agent}`.
/// Factored out so the construction is unit-testable independently of
/// actually invoking the `claude` binary.
fn claude_mcp_remove_user_args(agent_name: &str) -> [String; 5] {
    [
        "mcp".to_string(),
        "remove".to_string(),
        "-s".to_string(),
        "user".to_string(),
        format!("gosh-memory-{agent_name}"),
    ]
}

/// Best-effort: drop any prior `claude mcp add -s user gosh-memory-{agent}`
/// registration. Used both:
///   - by `configure_claude_mcp_user` for idempotency before re-adding;
///   - by `configure_claude_mcp_project` to migrate away from a previous
///     `--scope user` install (without this, a user→project flip would leave
///     the global registration alive, exposing the agent's memory tools to
///     every claude session on the host even after the user switched back to
///     the privacy-safe project default);
///   - by `remove_claude_mcp` for full cleanup on platform deselect.
///
/// Failure is non-fatal — `claude` may be missing, the registration may
/// not exist, or it may have been removed concurrently. Caller doesn't
/// need to react.
fn remove_claude_user_mcp_entry(agent_name: &str) {
    let _ =
        std::process::Command::new("claude").args(claude_mcp_remove_user_args(agent_name)).output();
}

fn configure_claude_mcp_project(
    agent_name: &str,
    cwd: &Path,
    key: &str,
    swarm: Option<&str>,
    binary: &str,
) -> Result<()> {
    // Migration: a previous `agent setup --scope user` may have called
    // `claude mcp add -s user gosh-memory-{agent}`, leaving a global
    // registration that survives a switch back to project scope.
    // Project scope is supposed to confine `gosh-memory-{agent}` to this
    // directory, so the user-scope leftover is exactly the kind of
    // cross-project tool exposure the project-default change is meant
    // to fix. Drop it before writing the project entry.
    remove_claude_user_mcp_entry(agent_name);

    let mcp_path = cwd.join(".mcp.json");
    let mut mcp = load_json_or_empty(&mcp_path);

    let servers = mcp
        .as_object_mut()
        .context("mcp.json root is not object")?
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    servers.as_object_mut().context("mcpServers not object")?.insert(
        format!("gosh-memory-{agent_name}"),
        serde_json::json!({
            "command": binary,
            "args": build_mcp_proxy_args(agent_name, key, swarm),
        }),
    );

    save_json(&mcp_path, &mcp)
}

fn configure_claude_mcp_user(
    agent_name: &str,
    cwd: &Path,
    key: &str,
    swarm: Option<&str>,
    binary: &str,
) -> Result<()> {
    // Idempotent: remove any prior user-scope registration before adding,
    // so re-running setup never errors on "server already exists".
    remove_claude_user_mcp_entry(agent_name);

    // Migration: a previous `agent setup` (without `--scope user`) may
    // have left a project-scope entry for this agent in
    // `<cwd>/.mcp.json`. Without cleaning it, Claude would see *two*
    // `gosh-memory-{agent}` registrations in this directory (project +
    // user), preserving the per-project trust prompt and stale args
    // path the user-scope mode is meant to avoid.
    if remove_claude_project_entry(agent_name, cwd)? {
        eprintln!(
            "Removed stale project-scope `gosh-memory-{agent_name}` entry from {} (superseded by user scope)",
            cwd.join(".mcp.json").display(),
        );
    }

    let proxy_args = build_mcp_proxy_args(agent_name, key, swarm);
    let mut cmd_args: Vec<String> = vec![
        "mcp".to_string(),
        "add".to_string(),
        "-s".to_string(),
        "user".to_string(),
        format!("gosh-memory-{agent_name}"),
        "--".to_string(),
        binary.to_string(),
    ];
    cmd_args.extend(proxy_args);

    let status = std::process::Command::new("claude")
        .args(&cmd_args)
        .status()
        .context("failed to run `claude mcp add -s user`")?;

    if !status.success() {
        anyhow::bail!("`claude mcp add -s user gosh-memory-{agent_name}` exited with {status}");
    }
    Ok(())
}

/// Strip the `gosh-memory-{agent}` entry from `<cwd>/.mcp.json` if
/// present. Returns `true` when an entry was removed (so the caller can
/// surface a migration message), `false` when the file or entry was
/// already absent. Used by both `configure_claude_mcp_user` (migrate
/// project→user) and `remove_claude_mcp` (full cleanup).
fn remove_claude_project_entry(agent_name: &str, cwd: &Path) -> Result<bool> {
    let mcp_path = cwd.join(".mcp.json");
    if !mcp_path.exists() {
        return Ok(false);
    }
    let mut mcp = load_json_or_empty(&mcp_path);
    let removed = mcp
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
        .map(|servers| servers.remove(&format!("gosh-memory-{agent_name}")).is_some())
        .unwrap_or(false);
    if removed {
        save_json(&mcp_path, &mcp)?;
    }
    Ok(removed)
}

fn remove_platform(agent_name: &str, platform: &str, cwd: &Path) {
    let removed = match platform {
        "claude" => {
            remove_claude_hooks(agent_name, cwd).is_ok()
                | remove_claude_mcp(agent_name, cwd).is_ok()
        }
        "codex" => {
            remove_codex_hooks(agent_name, cwd).is_ok() | remove_codex_mcp(agent_name).is_ok()
        }
        "gemini" => {
            remove_gemini_hooks(agent_name, cwd).is_ok()
                | remove_gemini_mcp(agent_name, cwd).is_ok()
        }
        _ => false,
    };
    if removed {
        eprintln!("Removed {platform} hooks + MCP");
    }
}

fn remove_claude_hooks(agent_name: &str, cwd: &Path) -> Result<()> {
    // Strip from BOTH user-level and project-level settings.json — when
    // this function is called for cleanup (claude unselected by
    // --platform, or a stale install), we want every trace gone, not
    // just one scope.
    for path in [claude_settings_path("user", cwd), claude_settings_path("project", cwd)] {
        let _ = remove_hooks_for_agent(&path, agent_name);
    }
    Ok(())
}

fn remove_claude_mcp(agent_name: &str, cwd: &Path) -> Result<()> {
    // Try removing the user-scope registration too — it might exist from
    // a prior `--scope user` setup (or, before the flag rename, a
    // `--mcp-scope user` setup on an older build). Helper is best-effort.
    remove_claude_user_mcp_entry(agent_name);

    remove_claude_project_entry(agent_name, cwd).map(|_| ())
}

// --- Codex CLI ---

/// Path to Codex's hooks.json at the requested scope. Codex 0.117+
/// discovers hooks at both `~/.codex/hooks.json` (user) and
/// `<repo>/.codex/hooks.json` (per-project) — both are read; the
/// per-project file fires only when codex is launched from inside
/// that directory.
fn codex_hooks_path(scope: &str, cwd: &Path) -> PathBuf {
    match scope {
        "user" => home_dir_join(".codex").join("hooks.json"),
        _ => cwd.join(".codex").join("hooks.json"),
    }
}

fn configure_codex_hooks(agent_name: &str, binary: &str, scope: &str, cwd: &Path) -> Result<()> {
    // Always nuke the legacy `hooks-gosh-{name}.json` (any scope). Earlier
    // versions of this code wrote a separate file there which Codex never
    // read; clean it up before writing to the canonical location.
    for legacy in [
        home_dir_join(".codex").join(format!("hooks-gosh-{agent_name}.json")),
        cwd.join(".codex").join(format!("hooks-gosh-{agent_name}.json")),
    ] {
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }
    }

    let hooks_path = codex_hooks_path(scope, cwd);
    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut settings = load_json_or_empty(&hooks_path);
    if !settings.is_object() {
        settings = Value::Object(serde_json::Map::new());
    }

    let hooks = settings
        .as_object_mut()
        .context("hooks.json root is not object")?
        .entry("hooks")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let hooks_obj = hooks.as_object_mut().context("hooks is not object")?;

    upsert_hook_array(
        hooks_obj,
        "UserPromptSubmit",
        agent_name,
        &format!("{binary} capture --name {agent_name} --platform codex --event prompt"),
    );
    upsert_hook_array(
        hooks_obj,
        "Stop",
        agent_name,
        &format!("{binary} capture --name {agent_name} --platform codex --event response"),
    );

    save_json(&hooks_path, &settings)?;

    // Auto-migrate: clean this agent's hooks from the OTHER scope so we
    // don't end up with simultaneous user-level + project-level installs.
    let other_path = codex_hooks_path(other_scope(scope), cwd);
    if other_path != hooks_path && other_path.exists() {
        let removed = remove_hooks_for_agent(&other_path, agent_name)?;
        if removed {
            eprintln!(
                "Removed stale `{agent_name}` Codex hooks from {} (superseded by `--scope {scope}`)",
                other_path.display(),
            );
        }
    }

    Ok(())
}

fn configure_codex_mcp(
    agent_name: &str,
    _cwd: &Path,
    key: &str,
    swarm: Option<&str>,
    binary: &str,
) -> Result<()> {
    // Codex CLI requires `codex mcp add` — config.toml [mcp-servers] is not read.
    let output = std::process::Command::new("codex")
        .args(["mcp", "remove", &format!("gosh-memory-{agent_name}")])
        .output();
    if let Ok(o) = &output {
        if !o.status.success() {
            // Ignore remove failure (server may not exist yet)
        }
    }

    let proxy_args = build_mcp_proxy_args(agent_name, key, swarm);
    let mut cmd_args: Vec<String> = vec![
        "mcp".to_string(),
        "add".to_string(),
        format!("gosh-memory-{agent_name}"),
        "--".to_string(),
        binary.to_string(),
    ];
    cmd_args.extend(proxy_args);

    let status = std::process::Command::new("codex")
        .args(&cmd_args)
        .status()
        .context("failed to run `codex mcp add`")?;

    if !status.success() {
        anyhow::bail!("`codex mcp add gosh-memory` exited with {status}");
    }
    Ok(())
}

fn remove_codex_hooks(agent_name: &str, cwd: &Path) -> Result<()> {
    // Legacy file cleanup (any scope) — old code wrote `hooks-gosh-{name}.json`
    // which Codex never read.
    for legacy in [
        home_dir_join(".codex").join(format!("hooks-gosh-{agent_name}.json")),
        cwd.join(".codex").join(format!("hooks-gosh-{agent_name}.json")),
    ] {
        if legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }
    }

    // Strip from BOTH user-level and project-level hooks.json — when this
    // function is called for cleanup (codex unselected by --platform, or
    // a stale install), we want every trace gone, not just the one scope
    // we happen to be writing today.
    for path in [codex_hooks_path("user", cwd), codex_hooks_path("project", cwd)] {
        let _ = remove_hooks_for_agent(&path, agent_name);
    }
    Ok(())
}

fn remove_codex_mcp(agent_name: &str) -> Result<()> {
    let _ = std::process::Command::new("codex")
        .args(["mcp", "remove", &format!("gosh-memory-{agent_name}")])
        .output();
    Ok(())
}

// --- Gemini CLI ---

/// Path to Gemini's settings.json at the requested scope. Gemini stores
/// both hooks and `mcpServers` in the same file; the project-level layer
/// overrides the user-level one when the CLI is launched from a project
/// dir (per Gemini CLI's documented hierarchical config loader).
fn gemini_settings_path(scope: &str, cwd: &Path) -> PathBuf {
    match scope {
        "user" => home_dir_join(".gemini").join("settings.json"),
        _ => cwd.join(".gemini").join("settings.json"),
    }
}

fn configure_gemini_hooks(agent_name: &str, binary: &str, scope: &str, cwd: &Path) -> Result<()> {
    let settings_path = gemini_settings_path(scope, cwd);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut settings = load_json_or_empty(&settings_path);

    let hooks = settings
        .as_object_mut()
        .context("settings.json root is not object")?
        .entry("hooks")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    let hooks_obj = hooks.as_object_mut().context("hooks is not object")?;

    upsert_gemini_hook(
        hooks_obj,
        "BeforeModel",
        agent_name,
        &format!("gosh-prompt-{agent_name}"),
        &format!("{binary} capture --name {agent_name} --platform gemini --event prompt"),
    );
    upsert_gemini_hook(
        hooks_obj,
        "AfterModel",
        agent_name,
        &format!("gosh-response-{agent_name}"),
        &format!("{binary} capture --name {agent_name} --platform gemini --event response"),
    );

    save_json(&settings_path, &settings)?;

    // Auto-migrate: clean this agent's hooks from the OTHER scope.
    let other_path = gemini_settings_path(other_scope(scope), cwd);
    if other_path != settings_path && other_path.exists() {
        let removed = remove_hooks_for_agent(&other_path, agent_name)?;
        if removed {
            eprintln!(
                "Removed stale `{agent_name}` Gemini hooks from {} (superseded by `--scope {scope}`)",
                other_path.display(),
            );
        }
    }

    Ok(())
}

fn configure_gemini_mcp(
    agent_name: &str,
    cwd: &Path,
    key: &str,
    swarm: Option<&str>,
    scope: &str,
    binary: &str,
) -> Result<()> {
    let settings_path = gemini_settings_path(scope, cwd);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut settings = load_json_or_empty(&settings_path);
    let servers = settings
        .as_object_mut()
        .context("settings.json root is not object")?
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    servers.as_object_mut().context("mcpServers not object")?.insert(
        format!("gosh-memory-{agent_name}"),
        serde_json::json!({
            "command": binary,
            "args": build_mcp_proxy_args(agent_name, key, swarm),
        }),
    );

    save_json(&settings_path, &settings)?;

    // Auto-migrate the MCP entry too — same reasoning as hooks.
    let other_path = gemini_settings_path(other_scope(scope), cwd);
    if other_path != settings_path && other_path.exists() {
        let removed = remove_gemini_mcp_entry(&other_path, agent_name)?;
        if removed {
            eprintln!(
                "Removed stale `gosh-memory-{agent_name}` from {} mcpServers (superseded by `--scope {scope}`)",
                other_path.display(),
            );
        }
    }

    Ok(())
}

fn remove_gemini_hooks(agent_name: &str, cwd: &Path) -> Result<()> {
    // Strip from BOTH scopes — see remove_codex_hooks for rationale.
    for path in [gemini_settings_path("user", cwd), gemini_settings_path("project", cwd)] {
        let _ = remove_hooks_for_agent(&path, agent_name);
    }
    Ok(())
}

fn remove_gemini_mcp(agent_name: &str, cwd: &Path) -> Result<()> {
    // Strip the `gosh-memory-{agent}` mcpServers entry from BOTH scopes.
    for path in [gemini_settings_path("user", cwd), gemini_settings_path("project", cwd)] {
        let _ = remove_gemini_mcp_entry(&path, agent_name);
    }
    Ok(())
}

fn remove_gemini_mcp_entry(path: &Path, agent_name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut settings = load_json_or_empty(path);
    let mut changed = false;
    if let Some(servers) = settings
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
    {
        if servers.remove(&format!("gosh-memory-{agent_name}")).is_some() {
            changed = true;
        }
    }
    if changed {
        save_json(path, &settings)?;
    }
    Ok(changed)
}

// --- Helpers ---

fn load_json_or_empty(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

fn save_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn upsert_hook_array(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    agent_name: &str,
    command: &str,
) {
    let entry = serde_json::json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": command }]
    });
    let arr = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = arr.as_array_mut() {
        arr.retain(|item| !item_matches_agent(item, agent_name));
        arr.push(entry);
    }
}

fn upsert_gemini_hook(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    agent_name: &str,
    name: &str,
    command: &str,
) {
    let entry = serde_json::json!({
        "matcher": ".*",
        "hooks": [{ "name": name, "type": "command", "command": command }]
    });
    let arr = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = arr.as_array_mut() {
        arr.retain(|item| !item_matches_agent(item, agent_name));
        arr.push(entry);
    }
}

/// Check if a hook entry belongs to a specific agent instance.
/// Matches `--name <agent_name>` in the command string.
fn item_matches_agent(item: &Value, agent_name: &str) -> bool {
    let cmd_matches = |cmd: &str| -> bool {
        let needle = format!("--name {agent_name}");
        if let Some(pos) = cmd.find(&needle) {
            let after = pos + needle.len();
            // Ensure the match is not a prefix of a longer name
            after == cmd.len() || cmd.as_bytes()[after] == b' ' || cmd[after..].starts_with(" --")
        } else {
            false
        }
    };

    if let Some(cmd) = item.get("command").and_then(|c| c.as_str()) {
        if cmd_matches(cmd) {
            return true;
        }
    }
    if let Some(hooks) = item.get("hooks").and_then(|h| h.as_array()) {
        for h in hooks {
            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                if cmd_matches(cmd) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::item_matches_agent;

    #[test]
    fn exact_agent_name_matches() {
        let item =
            json!({"hooks": [{"command": "gosh-agent capture --name prod --platform claude"}]});
        assert!(item_matches_agent(&item, "prod"));
    }

    #[test]
    fn agent_name_at_end_of_command_matches() {
        let item = json!({"hooks": [{"command": "gosh-agent capture --name prod"}]});
        assert!(item_matches_agent(&item, "prod"));
    }

    #[test]
    fn prefix_agent_name_does_not_match_longer_name() {
        let item = json!({"hooks": [{"command": "gosh-agent capture --name production --platform claude"}]});
        assert!(!item_matches_agent(&item, "prod"));
    }

    #[test]
    fn similar_agent_name_does_not_match() {
        let item =
            json!({"hooks": [{"command": "gosh-agent capture --name alpha --event prompt"}]});
        assert!(!item_matches_agent(&item, "a"));
    }

    use super::remove_claude_project_entry;
    use super::writes_project_files_in_cwd;

    #[test]
    fn cwd_root_guard_skips_user_scope() {
        // user-scope never writes any <cwd>-rooted file, so the cwd=/
        // guard must not fire regardless of which platforms are selected.
        assert!(!writes_project_files_in_cwd("user", &[]));
        assert!(!writes_project_files_in_cwd("user", &["claude".to_string()]));
        assert!(
            !writes_project_files_in_cwd("user", &["claude".to_string(), "codex".to_string()],)
        );
        assert!(!writes_project_files_in_cwd(
            "user",
            &["claude".to_string(), "codex".to_string(), "gemini".to_string()],
        ));
    }

    #[test]
    fn cwd_root_guard_fires_for_project_scope_with_any_platform() {
        // After the project-default scope change, EVERY supported platform
        // writes at least its hooks file under <cwd>/.<platform>/...
        // at project scope, so the guard must fire for any individual
        // platform, any combination, and the auto-detect (empty) case.
        assert!(writes_project_files_in_cwd("project", &["claude".to_string()]));
        assert!(writes_project_files_in_cwd("project", &["codex".to_string()]));
        assert!(writes_project_files_in_cwd("project", &["gemini".to_string()]));
        assert!(writes_project_files_in_cwd(
            "project",
            &["claude".to_string(), "codex".to_string()],
        ));
        // Empty platforms means auto-detect: any of the three could be
        // installed and trigger a project-rooted write — fire the guard
        // pessimistically rather than risk a half-written install.
        assert!(writes_project_files_in_cwd("project", &[]));
    }

    #[test]
    fn cwd_root_guard_ignores_unknown_platforms() {
        // An unknown platform name (clap should reject these earlier, but
        // the helper is defensive) doesn't on its own trigger the guard.
        assert!(!writes_project_files_in_cwd("project", &["unknown".to_string()]));
    }

    #[test]
    fn remove_claude_project_entry_strips_only_target_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        std::fs::write(
            &mcp_path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "gosh-memory-target": { "command": "x", "args": [] },
                    "gosh-memory-other":  { "command": "y", "args": [] },
                    "unrelated-tool":     { "command": "z", "args": [] },
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let removed = remove_claude_project_entry("target", dir.path()).unwrap();
        assert!(removed);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let servers = after.get("mcpServers").unwrap().as_object().unwrap();
        assert!(!servers.contains_key("gosh-memory-target"));
        assert!(servers.contains_key("gosh-memory-other"));
        assert!(servers.contains_key("unrelated-tool"));
    }

    #[test]
    fn remove_claude_project_entry_is_noop_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let removed = remove_claude_project_entry("any", dir.path()).unwrap();
        assert!(!removed);
        assert!(!dir.path().join(".mcp.json").exists());
    }

    #[test]
    fn remove_claude_project_entry_is_noop_when_entry_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_path = dir.path().join(".mcp.json");
        let original = serde_json::to_string_pretty(&json!({
            "mcpServers": { "unrelated-tool": { "command": "z", "args": [] } }
        }))
        .unwrap();
        std::fs::write(&mcp_path, &original).unwrap();

        let removed = remove_claude_project_entry("missing-agent", dir.path()).unwrap();
        assert!(!removed);
        // File untouched (not even reformatted) when nothing to remove.
        assert_eq!(std::fs::read_to_string(&mcp_path).unwrap(), original);
    }

    // ── Scope-aware path resolution ───────────────────────────────────

    use super::claude_settings_path;
    use super::codex_hooks_path;
    use super::gemini_settings_path;
    use super::other_scope;
    use super::remove_gemini_mcp_entry;
    use super::remove_hooks_for_agent;

    #[test]
    fn settings_paths_split_project_vs_user_for_each_platform() {
        let cwd = std::path::PathBuf::from("/tmp/proj");

        // Project scope → <cwd>/.<platform>/...
        assert_eq!(
            claude_settings_path("project", &cwd),
            cwd.join(".claude").join("settings.json"),
        );
        assert_eq!(codex_hooks_path("project", &cwd), cwd.join(".codex").join("hooks.json"),);
        assert_eq!(
            gemini_settings_path("project", &cwd),
            cwd.join(".gemini").join("settings.json"),
        );

        // User scope → ~/.<platform>/... (uses real home; we just check
        // it does NOT collapse to <cwd>-rooted paths).
        assert!(!claude_settings_path("user", &cwd).starts_with(&cwd));
        assert!(!codex_hooks_path("user", &cwd).starts_with(&cwd));
        assert!(!gemini_settings_path("user", &cwd).starts_with(&cwd));

        // Unknown scope falls through to the project branch (defensive —
        // clap restricts inputs upstream, but the helpers shouldn't crash).
        assert_eq!(
            claude_settings_path("anything-else", &cwd),
            cwd.join(".claude").join("settings.json"),
        );
    }

    #[test]
    fn other_scope_is_inverse_for_known_values_user_for_unknown() {
        assert_eq!(other_scope("project"), "user");
        assert_eq!(other_scope("user"), "project");
        // Defensive: unknown values map to "user" (the inverse-of-default).
        assert_eq!(other_scope("garbage"), "user");
    }

    // ── remove_hooks_for_agent (used by auto-migration when scope flips
    // and by every remove_*_hooks cleanup helper) ────────────────────

    fn settings_with_two_agents() -> serde_json::Value {
        json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "hooks": [{ "command": "gosh-agent capture --name target --platform claude --event prompt", "type": "command" }] },
                    { "hooks": [{ "command": "gosh-agent capture --name keepme --platform claude --event prompt", "type": "command" }] },
                ],
                "Stop": [
                    { "hooks": [{ "command": "gosh-agent capture --name target --platform claude --event response", "type": "command" }] },
                    { "hooks": [{ "command": "gosh-agent capture --name keepme --platform claude --event response", "type": "command" }] },
                ],
            }
        })
    }

    #[test]
    fn remove_hooks_for_agent_strips_only_target_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, serde_json::to_string_pretty(&settings_with_two_agents()).unwrap())
            .unwrap();

        let removed = remove_hooks_for_agent(&path, "target").unwrap();
        assert!(removed);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let hooks = after.get("hooks").unwrap().as_object().unwrap();
        for event in ["UserPromptSubmit", "Stop"] {
            let arr = hooks.get(event).unwrap().as_array().unwrap();
            assert_eq!(arr.len(), 1, "only `keepme` entry should remain on {event}");
            let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains("--name keepme"), "wrong agent survived on {event}: {cmd}");
        }
    }

    #[test]
    fn remove_hooks_for_agent_returns_false_when_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = serde_json::to_string_pretty(&settings_with_two_agents()).unwrap();
        std::fs::write(&path, &original).unwrap();

        let removed = remove_hooks_for_agent(&path, "missing-agent").unwrap();
        assert!(!removed);
        // File untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn remove_hooks_for_agent_is_noop_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let removed = remove_hooks_for_agent(&dir.path().join("missing.json"), "any").unwrap();
        assert!(!removed);
    }

    #[test]
    fn claude_mcp_remove_user_args_targets_agent_specific_user_scope_server() {
        // Regression for the missing user→project migration in Claude MCP:
        // configure_claude_mcp_project (and remove_claude_mcp, and
        // configure_claude_mcp_user idempotency) must invoke
        // `claude mcp remove -s user gosh-memory-{agent}` so that a
        // prior `--scope user` registration doesn't survive a switch
        // back to project scope. We can't easily test the actual shell
        // call (no claude binary in CI), so this asserts the args are
        // assembled with the right agent name and `-s user` flag.
        use super::claude_mcp_remove_user_args;

        let args = claude_mcp_remove_user_args("alpha");
        assert_eq!(args[0], "mcp", "subcommand");
        assert_eq!(args[1], "remove", "operation");
        assert_eq!(args[2], "-s", "scope flag");
        assert_eq!(args[3], "user", "scope value — must be `user`");
        assert_eq!(args[4], "gosh-memory-alpha", "server name includes agent name");
    }

    #[test]
    fn remove_gemini_mcp_entry_strips_only_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "gosh-memory-target": { "command": "x", "args": [] },
                    "gosh-memory-keepme": { "command": "y", "args": [] },
                    "unrelated":          { "command": "z", "args": [] },
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let removed = remove_gemini_mcp_entry(&path, "target").unwrap();
        assert!(removed);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let servers = after.get("mcpServers").unwrap().as_object().unwrap();
        assert!(!servers.contains_key("gosh-memory-target"));
        assert!(servers.contains_key("gosh-memory-keepme"));
        assert!(servers.contains_key("unrelated"));
    }
}
