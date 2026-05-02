// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

use super::LlmProvider;
use super::LlmResponse;
use super::Message;
use super::ToolCall;
use super::ToolDef;
use super::Usage;

const CLAUDE_STDIN_PROMPT: &str = "Read the complete task from stdin and complete it.";

#[derive(Debug, Clone, PartialEq)]
pub struct LocalCliConfig {
    pub command: LocalCliCommand,
    pub workspace_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalCliCommand {
    /// Legacy/custom command shape: prompt is written to stdin.
    Stdin {
        cli_bin: String,
        cli_args_prefix: Vec<String>,
    },
    Claude,
    Codex,
    Gemini,
}

impl LocalCliConfig {
    pub fn legacy_stdin(
        cli_bin: impl Into<String>,
        cli_args_prefix: Vec<String>,
        workspace_dir: Option<String>,
    ) -> Self {
        Self {
            command: LocalCliCommand::Stdin { cli_bin: cli_bin.into(), cli_args_prefix },
            workspace_dir,
        }
    }

    pub fn cli_label(&self) -> &'static str {
        match self.command {
            LocalCliCommand::Stdin { .. } => "custom",
            LocalCliCommand::Claude => "claude",
            LocalCliCommand::Codex => "codex",
            LocalCliCommand::Gemini => "gemini",
        }
    }

    pub fn cli_bin_display(&self) -> &str {
        match &self.command {
            LocalCliCommand::Stdin { cli_bin, .. } => cli_bin,
            LocalCliCommand::Claude => "claude",
            LocalCliCommand::Codex => "codex",
            LocalCliCommand::Gemini => "gemini",
        }
    }
}

pub struct LocalCliProvider {
    config: LocalCliConfig,
}

impl LocalCliProvider {
    pub fn new(config: LocalCliConfig) -> Self {
        Self { config }
    }
}

pub fn resolve_local_cli_config(
    requested: Option<&str>,
    workspace_dir: Option<String>,
) -> Result<LocalCliConfig> {
    if let Some(provider) = normalize_requested_provider(requested) {
        return Ok(LocalCliConfig { command: command_for_provider(&provider)?, workspace_dir });
    }
    if let Ok(provider) = std::env::var("GOSH_LOCAL_CLI_BACKEND") {
        let provider = provider.trim().to_ascii_lowercase();
        if !provider.is_empty() {
            return Ok(LocalCliConfig { command: command_for_provider(&provider)?, workspace_dir });
        }
    }
    for provider in ["claude", "codex", "gemini"] {
        if command_exists(provider) {
            return Ok(LocalCliConfig { command: command_for_provider(provider)?, workspace_dir });
        }
    }
    bail!(
        "LOCAL_CLI_UNAVAILABLE: memory selected local_cli, but no supported coding CLI was found \
         on PATH. Install/authenticate claude, codex, or gemini on the agent host, or set \
         GOSH_LOCAL_CLI_BACKEND to one of: claude, codex, gemini."
    )
}

fn normalize_requested_provider(requested: Option<&str>) -> Option<String> {
    let raw = requested?.trim();
    let lowered = raw.to_ascii_lowercase();
    if lowered.is_empty() || lowered == "local_cli" || lowered == "local-cli" {
        return None;
    }
    Some(
        lowered
            .strip_prefix("local-cli/")
            .or_else(|| lowered.strip_prefix("local_cli/"))
            .unwrap_or(&lowered)
            .to_string(),
    )
}

fn command_for_provider(provider: &str) -> Result<LocalCliCommand> {
    match provider {
        "claude" | "claude-code" | "claude_code" => Ok(LocalCliCommand::Claude),
        "codex" | "codex-cli" | "codex_cli" => Ok(LocalCliCommand::Codex),
        "gemini" | "gemini-cli" | "gemini_cli" => Ok(LocalCliCommand::Gemini),
        other => bail!(
            "LOCAL_CLI_UNSUPPORTED: unsupported local_cli provider '{other}'; expected one of \
             claude, codex, gemini"
        ),
    }
}

fn command_exists(bin: &str) -> bool {
    if bin.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(bin).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

pub fn render_local_cli_prompt(system: &str, messages: &[Message]) -> String {
    let mut blocks = vec![format!("SYSTEM:\n{system}")];
    for message in messages {
        blocks.push(format!("{}:\n{}", message.role.to_uppercase(), message.content));
    }
    blocks.join("\n\n")
}

async fn run_local_cli(prompt: &str, config: &LocalCliConfig) -> Result<String> {
    let invocation = configure_command(config)?;
    let mut command = Command::new(&invocation.cli_bin);
    if let Some(workspace_dir) = resolve_workspace_dir(config)? {
        command.current_dir(&workspace_dir);
    }
    command.args(&invocation.args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    if invocation.writes_stdin {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn local_cli {}", invocation.cli_bin))?;
    if invocation.writes_stdin {
        let Some(mut stdin) = child.stdin.take() else {
            bail!("failed to open local_cli stdin");
        };
        match stdin.write_all(prompt.as_bytes()).await {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                // The CLI exited before reading stdin. Continue to
                // wait_with_output so operators see the real
                // exit status and stderr instead of EPIPE.
            }
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context("failed to write prompt to local_cli stdin")
                );
            }
        }
        drop(stdin);
    }

    let output =
        child.wait_with_output().await.context("failed waiting for local_cli subprocess")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        bail!(
            "local_cli subprocess failed (exit_code={}, stdout={:?}, stderr={:?})",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }
    if !stderr.trim().is_empty() {
        debug!(stderr = stderr.trim(), "local_cli subprocess wrote stderr on success");
    }
    Ok(stdout)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalCliInvocation {
    cli_bin: String,
    args: Vec<String>,
    writes_stdin: bool,
}

impl LocalCliInvocation {
    fn stdin(
        cli_bin: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            cli_bin: cli_bin.into(),
            args: args.into_iter().map(Into::into).collect(),
            writes_stdin: true,
        }
    }
}

fn configure_command(config: &LocalCliConfig) -> Result<LocalCliInvocation> {
    match &config.command {
        LocalCliCommand::Stdin { cli_bin, .. } if cli_bin.trim().is_empty() => {
            bail!("local_cli cli_bin is empty")
        }
        LocalCliCommand::Stdin { cli_bin, cli_args_prefix } => {
            Ok(LocalCliInvocation::stdin(cli_bin.clone(), cli_args_prefix.clone()))
        }
        LocalCliCommand::Claude => Ok(LocalCliInvocation::stdin(
            "claude",
            ["-p", "--dangerously-skip-permissions", CLAUDE_STDIN_PROMPT],
        )),
        LocalCliCommand::Codex => Ok(LocalCliInvocation::stdin(
            "codex",
            ["exec", "--dangerously-bypass-approvals-and-sandbox", "--ephemeral", "-"],
        )),
        LocalCliCommand::Gemini => Ok(LocalCliInvocation::stdin("gemini", ["--yolo"])),
    }
}

fn resolve_workspace_dir(config: &LocalCliConfig) -> Result<Option<PathBuf>> {
    let Some(raw_workspace_dir) = config.workspace_dir.as_deref() else {
        return Ok(None);
    };
    let workspace_dir = raw_workspace_dir.trim();
    if workspace_dir.is_empty() {
        bail!("LOCAL_CLI_WORKSPACE_INVALID: workspace_dir is empty");
    }
    let path = PathBuf::from(workspace_dir);
    if !path.exists() {
        bail!("LOCAL_CLI_WORKSPACE_INVALID: workspace_dir does not exist: {}", path.display());
    }
    if !path.is_dir() {
        bail!("LOCAL_CLI_WORKSPACE_INVALID: workspace_dir is not a directory: {}", path.display());
    }
    path.canonicalize()
        .map(Some)
        .with_context(|| format!("LOCAL_CLI_WORKSPACE_INVALID: cannot resolve {}", path.display()))
}

#[async_trait]
impl LlmProvider for LocalCliProvider {
    async fn chat(
        &self,
        _model: &str,
        system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
        _max_tokens: u32,
    ) -> Result<LlmResponse> {
        let prompt = render_local_cli_prompt(system, messages);
        let text = run_local_cli(&prompt, &self.config).await?;
        Ok(LlmResponse {
            text: Some(text),
            tool_calls: Vec::<ToolCall>::new(),
            usage: Usage::default(),
            stop_reason: "stop".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn render_local_cli_prompt_matches_memory_shape() {
        let messages = vec![Message { role: "user".to_string(), content: "hello".to_string() }];
        assert_eq!(render_local_cli_prompt("sys", &messages), "SYSTEM:\nsys\n\nUSER:\nhello");
    }

    #[test]
    fn resolve_local_cli_config_accepts_case_insensitive_requested_provider() {
        let config =
            resolve_local_cli_config(Some("LOCAL-CLI/CoDeX"), Some("/tmp/workspace".to_string()))
                .unwrap();

        assert_eq!(
            config,
            LocalCliConfig {
                command: LocalCliCommand::Codex,
                workspace_dir: Some("/tmp/workspace".to_string()),
            }
        );
    }

    #[test]
    fn built_in_local_cli_invocations_do_not_put_prompt_in_argv() {
        let prompt = "PRIVATE MEMORY CONTEXT";
        for command in [LocalCliCommand::Claude, LocalCliCommand::Codex, LocalCliCommand::Gemini] {
            let invocation =
                configure_command(&LocalCliConfig { command, workspace_dir: None }).unwrap();

            assert!(invocation.writes_stdin);
            assert!(
                !invocation.args.iter().any(|arg| arg.contains(prompt)),
                "prompt leaked into argv: {:?}",
                invocation.args
            );
        }
    }

    #[test]
    fn built_in_local_cli_invocations_use_expected_private_prompt_transport() {
        assert_eq!(
            configure_command(&LocalCliConfig {
                command: LocalCliCommand::Claude,
                workspace_dir: None
            })
            .unwrap(),
            LocalCliInvocation::stdin(
                "claude",
                ["-p", "--dangerously-skip-permissions", CLAUDE_STDIN_PROMPT]
            )
        );
        assert_eq!(
            configure_command(&LocalCliConfig {
                command: LocalCliCommand::Codex,
                workspace_dir: None
            })
            .unwrap(),
            LocalCliInvocation::stdin(
                "codex",
                ["exec", "--dangerously-bypass-approvals-and-sandbox", "--ephemeral", "-",]
            )
        );
        assert_eq!(
            configure_command(&LocalCliConfig {
                command: LocalCliCommand::Gemini,
                workspace_dir: None
            })
            .unwrap(),
            LocalCliInvocation::stdin("gemini", ["--yolo"])
        );
    }

    #[tokio::test]
    async fn local_cli_provider_sends_prompt_to_stdin_and_returns_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo_prompt.sh");
        fs::write(&script, "#!/bin/sh\ncat\n").unwrap();

        let provider = LocalCliProvider::new(LocalCliConfig::legacy_stdin(
            "/bin/sh",
            vec![script.to_string_lossy().to_string()],
            None,
        ));
        let messages = vec![Message { role: "user".to_string(), content: "task".to_string() }];
        let response = provider.chat("gpt-5.4", "system", &messages, &[], 1024).await.unwrap();
        let text = response.text.unwrap();
        assert!(text.contains("SYSTEM:\nsystem"));
        assert!(text.contains("USER:\ntask"));
        assert!(response.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn local_cli_provider_does_not_apply_configured_wall_clock_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("delayed.sh");
        fs::write(&script, "#!/bin/sh\ncat >/dev/null\nsleep 0.2\nprintf done\n").unwrap();

        std::env::set_var("GOSH_LOCAL_CLI_TIMEOUT_SECS", "0.001");
        let provider = LocalCliProvider::new(LocalCliConfig::legacy_stdin(
            "/bin/sh",
            vec![script.to_string_lossy().to_string()],
            None,
        ));
        let messages = vec![Message { role: "user".to_string(), content: "task".to_string() }];
        let response = provider.chat("gpt-5.4", "system", &messages, &[], 1024).await.unwrap();
        std::env::remove_var("GOSH_LOCAL_CLI_TIMEOUT_SECS");

        assert_eq!(response.text.unwrap(), "done");
    }

    #[tokio::test]
    async fn local_cli_provider_runs_in_configured_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let script = dir.path().join("pwd.sh");
        fs::write(&script, "#!/bin/sh\ncat >/dev/null\npwd\n").unwrap();

        let provider = LocalCliProvider::new(LocalCliConfig::legacy_stdin(
            "/bin/sh",
            vec![script.to_string_lossy().to_string()],
            Some(workspace.path().to_string_lossy().to_string()),
        ));
        let response = provider.chat("gpt-5.4", "system", &[], &[], 1024).await.unwrap();

        assert_eq!(
            response.text.unwrap().trim(),
            workspace.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[tokio::test]
    async fn local_cli_provider_surfaces_exit_status_when_subprocess_exits_before_reading_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fail_fast.sh");
        fs::write(&script, "#!/bin/sh\necho 'codex: unknown flag --foo' >&2\nexit 7\n").unwrap();

        let provider = LocalCliProvider::new(LocalCliConfig::legacy_stdin(
            "/bin/sh",
            vec![script.to_string_lossy().to_string()],
            None,
        ));
        let messages =
            vec![Message { role: "user".to_string(), content: "task".repeat(1024 * 1024) }];
        let err =
            provider.chat("gpt-5.4", "system", &messages, &[], 1024).await.unwrap_err().to_string();

        assert!(err.contains("local_cli subprocess failed"), "got: {err}");
        assert!(err.contains("exit status: 7"), "got: {err}");
        assert!(err.contains("unknown flag"), "stderr should be surfaced: {err}");
        assert!(!err.contains("Broken pipe"), "EPIPE must not mask subprocess stderr: {err}");
    }

    #[tokio::test]
    async fn local_cli_provider_fails_clearly_on_invalid_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("noop.sh");
        let missing_workspace = dir.path().join("missing");
        fs::write(&script, "#!/bin/sh\nprintf ok\n").unwrap();

        let provider = LocalCliProvider::new(LocalCliConfig::legacy_stdin(
            "/bin/sh",
            vec![script.to_string_lossy().to_string()],
            Some(missing_workspace.to_string_lossy().to_string()),
        ));
        let err = provider.chat("gpt-5.4", "system", &[], &[], 1024).await.unwrap_err().to_string();

        assert!(err.contains("LOCAL_CLI_WORKSPACE_INVALID"));
        assert!(err.contains("does not exist"));
    }
}
