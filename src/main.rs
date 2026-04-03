// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

mod agent;
mod client;
mod courier;
mod join;
mod llm;
mod server;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod watcher;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use clap::Parser;
use tokio::sync::Mutex;
use tracing::info;

use crate::agent::config::AgentConfig;
use crate::agent::config::ModelBackend;
use crate::agent::config::RoutingTier;
use crate::agent::Agent;
use crate::client::memory::MemoryMcpClient;
use crate::client::transport::HttpTransport;
use crate::courier::CourierListener;
use crate::llm::multi::MultiProvider;
use crate::server::build_router;
use crate::server::AppState;
use crate::watcher::WatchConfig;

#[derive(Parser)]
#[command(name = "gosh-agent", about = "GOSH AI Agent MCP Server")]
struct Args {
    /// Join token from memory server (includes URL, auth, TLS cert).
    /// Overrides --memory-url and --memory-token.
    #[arg(long)]
    join: Option<String>,
    /// Memory service MCP URL (ignored if --join is set)
    #[arg(long, default_value = "http://127.0.0.1:8765/mcp")]
    memory_url: String,
    /// Memory service auth token (ignored if --join is set)
    #[arg(long)]
    memory_token: Option<String>,
    /// Listen host
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Listen port
    #[arg(long, default_value = "8767")]
    port: u16,
    /// Anthropic API key
    #[arg(long, env = "ANTHROPIC_API_KEY")]
    anthropic_api_key: Option<String>,
    /// OpenAI API key
    #[arg(long, env = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    /// Groq API key
    #[arg(long, env = "GROQ_API_KEY")]
    groq_api_key: Option<String>,
    /// Inception API key (falls back to MERCURY_API_KEY env if unset)
    #[arg(long, env = "INCEPTION_API_KEY")]
    inception_api_key: Option<String>,
    /// Profile used for extraction
    #[arg(long)]
    extraction_profile: Option<String>,
    /// Profile used for fast tasks
    #[arg(long)]
    fast_profile: Option<String>,
    /// Profile used for balanced tasks
    #[arg(long)]
    balanced_profile: Option<String>,
    /// Profile used for strong tasks
    #[arg(long)]
    strong_profile: Option<String>,
    /// Profile used for review
    #[arg(long)]
    review_profile: Option<String>,
    /// Override path to official Claude Code CLI binary
    #[arg(long, env = "GOSH_AGENT_CLAUDE_CLI_BIN")]
    claude_cli_bin: Option<String>,
    /// Override path to official Codex CLI binary
    #[arg(long, env = "GOSH_AGENT_CODEX_CLI_BIN")]
    codex_cli_bin: Option<String>,
    /// Override path to official Gemini CLI binary
    #[arg(long, env = "GOSH_AGENT_GEMINI_CLI_BIN")]
    gemini_cli_bin: Option<String>,
    /// Override Claude CLI cooldown in seconds
    #[arg(long, env = "GOSH_AGENT_CLAUDE_CLI_COOLDOWN_SECS")]
    claude_cli_cooldown_secs: Option<u64>,
    /// Override Codex CLI cooldown in seconds
    #[arg(long, env = "GOSH_AGENT_CODEX_CLI_COOLDOWN_SECS")]
    codex_cli_cooldown_secs: Option<u64>,
    /// Override Gemini CLI cooldown in seconds
    #[arg(long, env = "GOSH_AGENT_GEMINI_CLI_COOLDOWN_SECS")]
    gemini_cli_cooldown_secs: Option<u64>,

    /// Enable watch mode: auto-subscribe to courier + poll for tasks
    #[arg(long)]
    watch: bool,
    /// Memory key for watch mode
    #[arg(long, default_value = "default")]
    watch_key: String,
    /// Agent ID for watch mode
    #[arg(long, default_value = "default")]
    watch_agent_id: String,
    /// Swarm ID for watch mode
    #[arg(long, default_value = "default")]
    watch_swarm_id: String,
    /// Poll interval in seconds (fallback alongside courier)
    #[arg(long, default_value = "30")]
    poll_interval: u64,
    /// Default SHELL budget per task
    #[arg(long, default_value = "10.0")]
    watch_budget: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let (memory_url, memory_token, memory, pinned_client) = if let Some(ref join_str) = args.join {
        let jt = join::JoinToken::decode(join_str)?;
        let url = jt.url.clone();
        let token = jt.token.clone();
        let http_client = jt.build_http_client()?;
        let transport = HttpTransport::with_client(&url, Some(token), http_client.clone());
        info!(url = %url, fingerprint = %jt.fingerprint, "connected via join token (TLS pinned)");
        (url, Some(jt.token), Arc::new(MemoryMcpClient::new(transport)), Some(http_client))
    } else {
        let url = args.memory_url.trim_end_matches("/mcp").to_string();
        let token = args.memory_token.clone();
        let transport = HttpTransport::new(&url, token.clone());
        (url, token, Arc::new(MemoryMcpClient::new(transport)), None)
    };

    let mut config = AgentConfig::default();
    if let Some(profile) = args.extraction_profile {
        config.extraction_profile = profile;
    }
    if let Some(profile) = args.fast_profile {
        config.fast_profile = profile;
    }
    if let Some(profile) = args.balanced_profile {
        config.balanced_profile = profile;
    }
    if let Some(profile) = args.strong_profile {
        config.strong_profile = profile;
    }
    if let Some(profile) = args.review_profile {
        config.review_profile = profile;
    }
    config.claude_cli_bin = args.claude_cli_bin;
    config.codex_cli_bin = args.codex_cli_bin;
    config.gemini_cli_bin = args.gemini_cli_bin;
    config.claude_cli_cooldown_secs = args.claude_cli_cooldown_secs;
    config.codex_cli_cooldown_secs = args.codex_cli_cooldown_secs;
    config.gemini_cli_cooldown_secs = args.gemini_cli_cooldown_secs;
    config.validate()?;

    let anthropic_api_key = args.anthropic_api_key.clone();
    let openai_api_key = args.openai_api_key.clone();
    let groq_api_key = args.groq_api_key.clone();
    let inception_api_key =
        args.inception_api_key.clone().or_else(|| std::env::var("MERCURY_API_KEY").ok());

    let configured_profiles = [
        config.extraction_profile()?.backend,
        config.execution_profile(RoutingTier::Fast)?.backend,
        config.execution_profile(RoutingTier::Balanced)?.backend,
        config.execution_profile(RoutingTier::Strong)?.backend,
        config.review_profile()?.backend,
    ];

    let needs_anthropic_api =
        configured_profiles.iter().any(|backend| matches!(backend, ModelBackend::AnthropicApi));
    let needs_openai_api =
        configured_profiles.iter().any(|backend| matches!(backend, ModelBackend::OpenAiApi));
    let needs_groq_api =
        configured_profiles.iter().any(|backend| matches!(backend, ModelBackend::GroqApi));
    let needs_inception_api =
        configured_profiles.iter().any(|backend| matches!(backend, ModelBackend::InceptionApi));
    if needs_anthropic_api && anthropic_api_key.is_none() {
        bail!("ANTHROPIC_API_KEY is required for the configured API-backed model profiles");
    }
    if needs_openai_api && openai_api_key.is_none() {
        bail!("OPENAI_API_KEY is required for the configured API-backed model profiles");
    }
    if needs_groq_api && groq_api_key.is_none() {
        bail!("GROQ_API_KEY is required for the configured API-backed model profiles");
    }
    if needs_inception_api && inception_api_key.is_none() {
        bail!("INCEPTION_API_KEY or MERCURY_API_KEY is required for the configured API-backed model profiles");
    }

    let agent = {
        let llm = if anthropic_api_key.is_some()
            || openai_api_key.is_some()
            || groq_api_key.is_some()
            || inception_api_key.is_some()
        {
            Some(Arc::new(MultiProvider::new(
                anthropic_api_key,
                openai_api_key,
                groq_api_key,
                inception_api_key,
            )) as Arc<dyn crate::llm::LlmProvider>)
        } else {
            None
        };
        Agent::new(config.clone(), memory.clone(), llm)
    };

    let app_state = Arc::new(AppState {
        agent,
        memory: memory.clone(),
        courier: Mutex::new(CourierListener::new(&memory_url, memory_token, pinned_client)),
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
    });

    let app = build_router(app_state.clone());
    let addr = format!("{}:{}", args.host, args.port);

    println!("GOSH Agent MCP Server");
    println!("  Listening on http://{addr}");
    println!("  Memory:      {memory_url}");
    println!("  Watch mode:  {}", if args.watch { "ON" } else { "off" });
    println!("  Profiles:");
    println!("    extraction={}", config.extraction_profile);
    println!("    fast={}", config.fast_profile);
    println!("    balanced={}", config.balanced_profile);
    println!("    strong={}", config.strong_profile);
    println!("    review={}", config.review_profile);
    println!(
        "  CLI overrides: claude_bin={:?} codex_bin={:?} gemini_bin={:?}",
        config.claude_cli_bin, config.codex_cli_bin, config.gemini_cli_bin
    );
    println!(
        "  CLI cooldown overrides: claude={:?} codex={:?} gemini={:?}",
        config.claude_cli_cooldown_secs,
        config.codex_cli_cooldown_secs,
        config.gemini_cli_cooldown_secs
    );
    if args.watch {
        println!(
            "    key={} agent={} swarm={} poll={}s budget={}",
            args.watch_key,
            args.watch_agent_id,
            args.watch_swarm_id,
            args.poll_interval,
            args.watch_budget
        );
    }
    println!("  POST /mcp    → agent_start, agent_status");
    println!("  GET  /health → health check");

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if args.watch {
        let watch_config = WatchConfig {
            key: args.watch_key,
            agent_id: args.watch_agent_id,
            swarm_id: args.watch_swarm_id,
            poll_interval: Duration::from_secs(args.poll_interval),
            budget_shell: args.watch_budget,
        };

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let watcher_state = app_state.clone();
        let watcher_memory = memory.clone();

        tokio::spawn(async move {
            watcher::run(watch_config, watcher_state, watcher_memory, cancel_rx).await;
        });

        info!("watch mode started");

        // Server runs until shutdown; cancel watcher on exit
        let result = axum::serve(listener, app).await;
        let _ = cancel_tx.send(true);
        result?;
    } else {
        axum::serve(listener, app).await?;
    }

    Ok(())
}
