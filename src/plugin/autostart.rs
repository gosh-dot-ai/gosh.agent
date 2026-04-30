// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::bail;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::config::state_dir;

/// Install the autostart artifact for `agent_name`. Idempotent — safe to
/// re-run; a previously-installed unit is replaced and reloaded so the
/// daemon picks up any `GlobalConfig` changes setup just wrote.
pub fn install(agent_name: &str) -> Result<()> {
    let binary = std::env::current_exe()
        .context("could not resolve gosh-agent binary path for autostart artifact")?;
    install_with_binary(agent_name, &binary)
}

/// Remove the autostart artifact. Idempotent — missing unit / already-
/// unloaded service are not errors. Used by `gosh-agent uninstall`.
pub fn uninstall(agent_name: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd_uninstall(agent_name)
    }
    #[cfg(target_os = "linux")]
    {
        systemd_uninstall(agent_name)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = agent_name;
        Ok(())
    }
}

fn install_with_binary(agent_name: &str, binary: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd_install(agent_name, binary)
    }
    #[cfg(target_os = "linux")]
    {
        systemd_install(agent_name, binary)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (agent_name, binary);
        eprintln!(
            "Skipping autostart artifact: unsupported platform. \
             Supervise `gosh-agent serve --name {agent_name}` yourself."
        );
        Ok(())
    }
}

// ── macOS / launchd ─────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn launchd_label(agent_name: &str) -> String {
    format!("com.gosh.agent.{agent_name}")
}

#[cfg(target_os = "macos")]
fn launchd_plist_path(agent_name: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    home.join("Library").join("LaunchAgents").join(format!("{}.plist", launchd_label(agent_name)))
}

#[cfg(target_os = "macos")]
fn launchd_install(agent_name: &str, binary: &Path) -> Result<()> {
    let plist_path = launchd_plist_path(agent_name);
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let state = state_dir(agent_name);
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let stdout_path = state.join("daemon.out.log");
    let stderr_path = state.join("daemon.err.log");

    let plist = render_launchd_plist(
        &launchd_label(agent_name),
        binary,
        agent_name,
        &stdout_path,
        &stderr_path,
    );
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("writing {}", plist_path.display()))?;

    // Reload: bootout the previous instance (best-effort — fine if absent),
    // then bootstrap the freshly-written plist so the daemon picks up any
    // GlobalConfig changes setup just wrote.
    let domain = launchd_user_domain()?;
    let _ =
        Command::new("launchctl").args(["bootout", &domain, &plist_to_str(&plist_path)?]).output();
    let bootstrap = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_to_str(&plist_path)?])
        .output()
        .context("running `launchctl bootstrap`")?;
    if !bootstrap.status.success() {
        let stderr = String::from_utf8_lossy(&bootstrap.stderr);
        bail!("`launchctl bootstrap` failed for {}: {}", plist_path.display(), stderr.trim());
    }

    eprintln!("Installed launchd autostart at {}", plist_path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_uninstall(agent_name: &str) -> Result<()> {
    let plist_path = launchd_plist_path(agent_name);
    if plist_path.exists() {
        let domain = launchd_user_domain()?;
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &plist_to_str(&plist_path)?])
            .output();
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("removing {}", plist_path.display()))?;
        eprintln!("Removed launchd autostart at {}", plist_path.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_user_domain() -> Result<String> {
    // `gui/$(id -u)` is the user's GUI session domain — required so the
    // daemon shares the user's keychain unlock state. Shell out to `id -u`
    // rather than depend on libc just for getuid().
    let out = Command::new("id").arg("-u").output().context("running `id -u`")?;
    if !out.status.success() {
        bail!("`id -u` exited with {}", out.status);
    }
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("`id -u` returned empty output");
    }
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn plist_to_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(|s| s.to_string())
        .with_context(|| format!("plist path is not valid UTF-8: {}", path.display()))
}

#[cfg(target_os = "macos")]
fn render_launchd_plist(
    label: &str,
    binary: &Path,
    agent_name: &str,
    stdout_path: &Path,
    stderr_path: &Path,
) -> String {
    // Minimal escaping: `<` and `&` are the only characters that matter
    // in plist string content, and label/agent_name are constrained
    // upstream. Paths come from std::env::current_exe / dirs::home_dir.
    let xml_escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>serve</string>
        <string>--name</string>
        <string>{name}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        binary = xml_escape(&binary.to_string_lossy()),
        name = xml_escape(agent_name),
        stdout = xml_escape(&stdout_path.to_string_lossy()),
        stderr = xml_escape(&stderr_path.to_string_lossy()),
    )
}

// ── Linux / systemd user units ──────────────────────────────────────────

#[cfg(target_os = "linux")]
fn systemd_unit_name(agent_name: &str) -> String {
    format!("gosh-agent-{agent_name}.service")
}

#[cfg(target_os = "linux")]
fn systemd_unit_path(agent_name: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    home.join(".config").join("systemd").join("user").join(systemd_unit_name(agent_name))
}

#[cfg(target_os = "linux")]
fn systemd_install(agent_name: &str, binary: &Path) -> Result<()> {
    let unit_path = systemd_unit_path(agent_name);
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let state = state_dir(agent_name);
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;

    let unit = render_systemd_unit(binary, agent_name);
    std::fs::write(&unit_path, unit).with_context(|| format!("writing {}", unit_path.display()))?;

    let unit_name = systemd_unit_name(agent_name);

    // Reload daemon to pick up changes, then enable+start (idempotent;
    // re-running on an already-enabled unit just refreshes it).
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    let restart = Command::new("systemctl")
        .args(["--user", "enable", "--now", &unit_name])
        .output()
        .context("running `systemctl --user enable --now`")?;
    if !restart.status.success() {
        let stderr = String::from_utf8_lossy(&restart.stderr);
        bail!("`systemctl --user enable --now {unit_name}` failed: {}", stderr.trim());
    }
    // If the unit was already enabled before, `enable --now` doesn't
    // restart it — issue an explicit restart so GlobalConfig changes take
    // effect on the running process.
    let _ = Command::new("systemctl").args(["--user", "restart", &unit_name]).output();

    eprintln!("Installed systemd autostart at {}", unit_path.display());

    // systemd user units only start at boot when the user has lingering
    // enabled — otherwise the user instance is torn down on logout and
    // there's no daemon to spawn at boot. We can't enable linger from
    // here (it needs root), so we check and emit a hint when missing.
    if !linger_enabled() {
        eprintln!(
            "  hint: `loginctl enable-linger $USER` (needs sudo) so the agent comes \
             back up on reboot without an interactive login. Skip if you only run \
             this user session interactively."
        );
    }
    Ok(())
}

/// True iff systemd-logind reports lingering enabled for the current
/// user — required for `systemctl --user` units to come up at boot
/// without an active session. Best-effort: any error (logind unavailable,
/// command shape changed) returns `true` so we don't false-alarm
/// operators on systems where the question is moot.
#[cfg(target_os = "linux")]
fn linger_enabled() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        return true;
    }
    let out = match Command::new("loginctl").args(["show-user", &user, "-p", "Linger"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return true,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    !text.lines().any(|line| line.trim() == "Linger=no")
}

#[cfg(target_os = "linux")]
fn systemd_uninstall(agent_name: &str) -> Result<()> {
    let unit_path = systemd_unit_path(agent_name);
    let unit_name = systemd_unit_name(agent_name);
    if unit_path.exists() {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", &unit_name]).output();
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        eprintln!("Removed systemd autostart at {}", unit_path.display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn render_systemd_unit(binary: &Path, agent_name: &str) -> String {
    format!(
        "[Unit]\n\
         Description=GOSH Agent (instance: {agent_name})\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary} serve --name {agent_name}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = binary.display(),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_label_and_path_track_agent_name() {
        let label = super::launchd_label("alpha");
        assert_eq!(label, "com.gosh.agent.alpha");

        let path = super::launchd_plist_path("alpha");
        let s = path.to_string_lossy();
        assert!(s.ends_with("Library/LaunchAgents/com.gosh.agent.alpha.plist"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn render_launchd_plist_contains_serve_name() {
        let xml = super::render_launchd_plist(
            "com.gosh.agent.alpha",
            std::path::Path::new("/usr/local/bin/gosh-agent"),
            "alpha",
            std::path::Path::new("/tmp/out.log"),
            std::path::Path::new("/tmp/err.log"),
        );
        assert!(xml.contains("<string>com.gosh.agent.alpha</string>"));
        assert!(xml.contains("<string>/usr/local/bin/gosh-agent</string>"));
        assert!(xml.contains("<string>serve</string>"));
        assert!(xml.contains("<string>--name</string>"));
        assert!(xml.contains("<string>alpha</string>"));
        assert!(xml.contains("<string>/tmp/out.log</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_name_and_path_track_agent_name() {
        assert_eq!(super::systemd_unit_name("alpha"), "gosh-agent-alpha.service");
        let path = super::systemd_unit_path("alpha");
        let s = path.to_string_lossy();
        assert!(s.ends_with(".config/systemd/user/gosh-agent-alpha.service"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_systemd_unit_contains_serve_name() {
        let unit =
            super::render_systemd_unit(std::path::Path::new("/usr/local/bin/gosh-agent"), "alpha");
        assert!(unit.contains("ExecStart=/usr/local/bin/gosh-agent serve --name alpha"));
        assert!(unit.contains("Description=GOSH Agent (instance: alpha)"));
    }
}
