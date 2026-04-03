// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::info;

use super::config::AgentConfig;
use super::config::ModelBackend;
use super::config::ModelProfile;
use super::config::ResolvedCliCommand;
use crate::llm::Message;

struct CliGate {
    run_lock: Mutex<()>,
    last_finished_at: Mutex<Option<Instant>>,
}

impl CliGate {
    fn new() -> Self {
        Self { run_lock: Mutex::new(()), last_finished_at: Mutex::new(None) }
    }
}

pub struct CliExecutorManager {
    global: CliGate,
}

impl CliExecutorManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { global: CliGate::new() })
    }

    pub async fn run_prompt(
        &self,
        config: &AgentConfig,
        profile: &ModelProfile,
        prompt: &str,
        workdir: Option<&str>,
    ) -> Result<String> {
        match profile.backend {
            ModelBackend::ClaudeCli | ModelBackend::CodexCli | ModelBackend::GeminiCli => {}
            ModelBackend::AnthropicApi
            | ModelBackend::OpenAiApi
            | ModelBackend::GroqApi
            | ModelBackend::InceptionApi => bail!("profile {} is not a CLI backend", profile.id),
        }

        let _run_guard = self.global.run_lock.lock().await;

        if profile.max_concurrency != 1 {
            bail!(
                "unsupported CLI profile {}: max_concurrency={} (expected 1)",
                profile.id,
                profile.max_concurrency
            );
        }

        let command = config.resolve_cli_command(profile)?;
        wait_cooldown(profile.id, &self.global, command.cooldown_secs).await;
        let result = run_profile_command(profile.backend, &command, prompt, workdir).await;
        *self.global.last_finished_at.lock().await = Some(Instant::now());
        result
    }
}

async fn wait_cooldown(profile_id: &str, gate: &CliGate, cooldown_secs: u64) {
    if cooldown_secs == 0 {
        return;
    }

    let last_finished = *gate.last_finished_at.lock().await;
    let Some(last_finished) = last_finished else {
        return;
    };

    let cooldown = Duration::from_secs(cooldown_secs);
    let ready_at = last_finished + cooldown;
    let now = Instant::now();
    if ready_at > now {
        let wait = ready_at.duration_since(now);
        info!(profile = profile_id, wait_secs = wait.as_secs(), "waiting for global CLI cooldown");
        tokio::time::sleep_until(ready_at).await;
    }
}

async fn run_profile_command(
    backend: ModelBackend,
    command_spec: &ResolvedCliCommand,
    prompt: &str,
    workdir: Option<&str>,
) -> Result<String> {
    let mut command = Command::new(&command_spec.bin);
    command.args(&command_spec.args_prefix);
    command.arg(prompt);

    if let Some(dir) = workdir {
        command.current_dir(dir);
    }

    let output = command
        .output()
        .await
        .with_context(|| format!("failed to spawn CLI backend {}", command_spec.bin))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("CLI backend {:?} exited with {}: {}", backend, output.status, detail);
    }

    if !stdout.is_empty() {
        return Ok(stdout);
    }

    if !stderr.is_empty() {
        return Ok(stderr);
    }

    Ok(String::new())
}

pub fn render_cli_prompt(backend: ModelBackend, system: &str, messages: &[Message]) -> String {
    match backend {
        ModelBackend::ClaudeCli => render_role_prompt(system, messages),
        ModelBackend::CodexCli => render_role_prompt(system, messages),
        ModelBackend::GeminiCli => render_role_prompt(system, messages),
        ModelBackend::AnthropicApi
        | ModelBackend::OpenAiApi
        | ModelBackend::GroqApi
        | ModelBackend::InceptionApi => render_role_prompt(system, messages),
    }
}

fn render_role_prompt(system: &str, messages: &[Message]) -> String {
    let mut prompt = String::new();
    prompt.push_str(system);
    prompt.push_str("\n\n");

    for message in messages {
        prompt.push_str(&message.role.to_uppercase());
        prompt.push_str(":\n");
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }

    prompt
}
