// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::collections::HashMap;

use anyhow::bail;
use anyhow::Result;

/// Routing tiers selected from complexity hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoutingTier {
    Fast,
    Balanced,
    Strong,
}

impl RoutingTier {
    pub fn escalate(self) -> Option<Self> {
        match self {
            Self::Fast => Some(Self::Balanced),
            Self::Balanced => Some(Self::Strong),
            Self::Strong => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelBackend {
    AnthropicApi,
    OpenAiApi,
    GroqApi,
    InceptionApi,
    ClaudeCli,
    CodexCli,
    GeminiCli,
}

impl ModelBackend {
    pub fn supports_tools(self) -> bool {
        matches!(self, Self::AnthropicApi | Self::OpenAiApi | Self::GroqApi | Self::InceptionApi)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    pub id: &'static str,
    pub model_id: &'static str,
    pub backend: ModelBackend,
    pub cost_per_1k: f64,
    pub cli_bin: Option<&'static str>,
    pub cli_args_prefix: &'static [&'static str],
    pub cooldown_secs: u64,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone)]
pub struct ResolvedCliCommand {
    pub bin: String,
    pub args_prefix: Vec<String>,
    pub cooldown_secs: u64,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileRuntimePolicy {
    pub cooldown_secs: Option<u64>,
    pub max_concurrency: Option<usize>,
}

pub const BUILTIN_PROFILES: &[ModelProfile] = &[
    ModelProfile {
        id: "anthropic_haiku_api",
        model_id: "claude-haiku-4-5-20251001",
        backend: ModelBackend::AnthropicApi,
        cost_per_1k: 0.03,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "anthropic_sonnet_api",
        model_id: "claude-sonnet-4-6",
        backend: ModelBackend::AnthropicApi,
        cost_per_1k: 0.30,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "anthropic_opus_api",
        model_id: "claude-opus-4-6",
        backend: ModelBackend::AnthropicApi,
        cost_per_1k: 1.50,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "qwen_fast",
        model_id: "qwen/qwen3-32b",
        backend: ModelBackend::GroqApi,
        cost_per_1k: 0.03,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "sonnet_balanced",
        model_id: "anthropic/claude-sonnet-4-6",
        backend: ModelBackend::AnthropicApi,
        cost_per_1k: 0.30,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "mercury_strong",
        model_id: "inception/mercury-2",
        backend: ModelBackend::InceptionApi,
        cost_per_1k: 1.00,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "gpt41_max",
        model_id: "gpt-4.1",
        backend: ModelBackend::OpenAiApi,
        cost_per_1k: 1.50,
        cli_bin: None,
        cli_args_prefix: &[],
        cooldown_secs: 0,
        max_concurrency: usize::MAX,
    },
    ModelProfile {
        id: "claude_code_cli",
        model_id: "claude-code",
        backend: ModelBackend::ClaudeCli,
        cost_per_1k: 0.30,
        cli_bin: Some("claude"),
        cli_args_prefix: &["-p"],
        cooldown_secs: 600,
        max_concurrency: 1,
    },
    ModelProfile {
        id: "codex_cli",
        model_id: "codex-cli",
        backend: ModelBackend::CodexCli,
        cost_per_1k: 0.30,
        cli_bin: Some("codex"),
        cli_args_prefix: &["exec"],
        cooldown_secs: 600,
        max_concurrency: 1,
    },
    ModelProfile {
        id: "gemini_cli",
        model_id: "gemini-cli",
        backend: ModelBackend::GeminiCli,
        cost_per_1k: 0.20,
        cli_bin: Some("gemini"),
        cli_args_prefix: &["-p"],
        cooldown_secs: 600,
        max_concurrency: 1,
    },
];

pub fn profile_by_id(id: &str) -> Option<&'static ModelProfile> {
    BUILTIN_PROFILES.iter().find(|profile| profile.id == id)
}

fn model_ids_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }

    let left_suffix = left.split_once('/').map(|(_, suffix)| suffix).unwrap_or(left);
    let right_suffix = right.split_once('/').map(|(_, suffix)| suffix).unwrap_or(right);
    left_suffix == right_suffix
}

pub fn profile_by_model_id(model_id: &str) -> Option<&'static ModelProfile> {
    BUILTIN_PROFILES.iter().find(|profile| model_ids_match(profile.model_id, model_id))
}

/// Agent-specific configuration (budget, retries, complexity, profile routing).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Whether this agent config is enabled.
    pub enabled: bool,
    /// Fraction of budget reserved for review phase.
    pub review_budget_reserve: f64,
    /// If estimated effort > budget × this, reject as too_complex.
    pub too_complex_threshold: f64,
    /// Max retries after failed review.
    pub max_retries: u32,
    /// Profile used for fact extraction.
    pub extraction_profile: String,
    /// Profiles used for execution routing.
    pub fast_profile: String,
    pub balanced_profile: String,
    pub strong_profile: String,
    /// Profile used for review.
    pub review_profile: String,
    /// Maximum number of tasks allowed to run concurrently for this agent.
    pub max_parallel_tasks: usize,
    /// Optional global cooldown applied to CLI-backed executions.
    pub global_cli_cooldown_secs: Option<u64>,
    /// Optional per-profile runtime policy overrides loaded from persisted
    /// agent config.
    pub profile_runtime: HashMap<String, ProfileRuntimePolicy>,
    /// Optional runtime overrides for official CLI binary paths.
    pub claude_cli_bin: Option<String>,
    pub codex_cli_bin: Option<String>,
    pub gemini_cli_bin: Option<String>,
    /// Optional runtime overrides for CLI cooldowns.
    pub claude_cli_cooldown_secs: Option<u64>,
    pub codex_cli_cooldown_secs: Option<u64>,
    pub gemini_cli_cooldown_secs: Option<u64>,
}

impl AgentConfig {
    pub fn execution_profile_id(&self, tier: RoutingTier) -> &str {
        match tier {
            RoutingTier::Fast => &self.fast_profile,
            RoutingTier::Balanced => &self.balanced_profile,
            RoutingTier::Strong => &self.strong_profile,
        }
    }

    pub fn resolve_profile(&self, profile_id: &str) -> Result<&'static ModelProfile> {
        profile_by_id(profile_id)
            .ok_or_else(|| anyhow::anyhow!("unknown model profile: {profile_id}"))
    }

    pub fn execution_profile(&self, tier: RoutingTier) -> Result<&'static ModelProfile> {
        self.resolve_profile(self.execution_profile_id(tier))
    }

    pub fn review_profile(&self) -> Result<&'static ModelProfile> {
        self.resolve_profile(&self.review_profile)
    }

    pub fn extraction_profile(&self) -> Result<&'static ModelProfile> {
        self.resolve_profile(&self.extraction_profile)
    }

    pub fn allowed_profile_ids(&self) -> Vec<&str> {
        vec![
            self.extraction_profile.as_str(),
            self.fast_profile.as_str(),
            self.balanced_profile.as_str(),
            self.strong_profile.as_str(),
            self.review_profile.as_str(),
        ]
    }

    pub fn resolve_cli_command(&self, profile: &ModelProfile) -> Result<ResolvedCliCommand> {
        let default_bin = profile.cli_bin.ok_or_else(|| {
            anyhow::anyhow!("profile {} has no CLI binary configured", profile.id)
        })?;
        let policy = self.profile_runtime.get(profile.id);

        let (bin_override, cooldown_override) = match profile.backend {
            ModelBackend::ClaudeCli => {
                (self.claude_cli_bin.as_deref(), self.claude_cli_cooldown_secs)
            }
            ModelBackend::CodexCli => (self.codex_cli_bin.as_deref(), self.codex_cli_cooldown_secs),
            ModelBackend::GeminiCli => {
                (self.gemini_cli_bin.as_deref(), self.gemini_cli_cooldown_secs)
            }
            ModelBackend::AnthropicApi
            | ModelBackend::OpenAiApi
            | ModelBackend::GroqApi
            | ModelBackend::InceptionApi => {
                bail!("profile {} is not a CLI backend", profile.id);
            }
        };

        let requested_cooldown = policy
            .and_then(|cfg| cfg.cooldown_secs)
            .or(cooldown_override)
            .or(self.global_cli_cooldown_secs);
        let effective_cooldown =
            requested_cooldown.unwrap_or(profile.cooldown_secs).max(profile.cooldown_secs);

        let requested_concurrency =
            policy.and_then(|cfg| cfg.max_concurrency).unwrap_or(profile.max_concurrency);
        let effective_concurrency = requested_concurrency.min(profile.max_concurrency);
        if effective_concurrency == 0 {
            bail!("profile {} max_concurrency must be >= 1", profile.id);
        }

        Ok(ResolvedCliCommand {
            bin: bin_override.unwrap_or(default_bin).to_string(),
            args_prefix: profile.cli_args_prefix.iter().map(|arg| (*arg).to_string()).collect(),
            cooldown_secs: effective_cooldown,
            max_concurrency: effective_concurrency,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_parallel_tasks == 0 {
            bail!("max_parallel_tasks must be >= 1");
        }
        if !(0.0..1.0).contains(&self.review_budget_reserve) {
            bail!("review_budget_reserve must be >= 0.0 and < 1.0");
        }
        if !(0.0..=1.0).contains(&self.too_complex_threshold) || self.too_complex_threshold == 0.0 {
            bail!("too_complex_threshold must be > 0.0 and <= 1.0");
        }
        for profile_id in [
            &self.extraction_profile,
            &self.fast_profile,
            &self.balanced_profile,
            &self.strong_profile,
            &self.review_profile,
        ] {
            if profile_by_id(profile_id).is_none() {
                bail!("unknown model profile: {profile_id}");
            }
        }

        for profile in [
            self.extraction_profile()?,
            self.execution_profile(RoutingTier::Fast)?,
            self.execution_profile(RoutingTier::Balanced)?,
            self.execution_profile(RoutingTier::Strong)?,
            self.review_profile()?,
        ] {
            if matches!(
                profile.backend,
                ModelBackend::ClaudeCli | ModelBackend::CodexCli | ModelBackend::GeminiCli
            ) {
                let command = self.resolve_cli_command(profile)?;
                if command.max_concurrency != 1 {
                    bail!(
                        "profile {} must keep CLI max_concurrency=1, got {}",
                        profile.id,
                        command.max_concurrency
                    );
                }
                if command.cooldown_secs < profile.cooldown_secs {
                    bail!(
                        "profile {} must keep CLI cooldown >= {}, got {}",
                        profile.id,
                        profile.cooldown_secs,
                        command.cooldown_secs
                    );
                }
            }
        }
        Ok(())
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            review_budget_reserve: 0.2,
            too_complex_threshold: 0.8,
            max_retries: 2,
            extraction_profile: "anthropic_haiku_api".to_string(),
            fast_profile: "anthropic_haiku_api".to_string(),
            balanced_profile: "anthropic_sonnet_api".to_string(),
            strong_profile: "anthropic_opus_api".to_string(),
            review_profile: "anthropic_sonnet_api".to_string(),
            max_parallel_tasks: usize::MAX,
            global_cli_cooldown_secs: None,
            profile_runtime: HashMap::new(),
            claude_cli_bin: None,
            codex_cli_bin: None,
            gemini_cli_bin: None,
            claude_cli_cooldown_secs: None,
            codex_cli_cooldown_secs: None,
            gemini_cli_cooldown_secs: None,
        }
    }
}
