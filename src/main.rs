// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

mod agent;
mod auth;
mod client;
mod courier;
mod crypto;
mod join;
mod llm;
mod plugin;
mod sandbox;
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
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

use crate::agent::config::AgentConfig;
use crate::agent::pricing::PricingCatalog;
use crate::agent::Agent;
use crate::auth::MemoryAuthState;
use crate::client::memory::MemoryMcpClient;
use crate::client::transport::HttpTransport;
use crate::courier::CourierListener;
use crate::server::build_router;
use crate::server::AppState;
use crate::watcher::WatchConfig;

#[derive(Parser)]
#[command(name = "gosh-agent", about = "GOSH AI Agent — MCP server, capture plugin, MCP proxy")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Run as MCP server (default agent mode)
    Serve {
        /// JSON file with join_token + secret_key (base64), deleted by CLI
        /// after start
        #[arg(long)]
        bootstrap_file: Option<String>,
        #[arg(long)]
        join: Option<String>,
        #[arg(long)]
        allow_insecure_inline_join: bool,
        #[arg(long)]
        memory_url: Option<String>,
        #[arg(long)]
        memory_token: Option<String>,
        #[arg(long, env = "GOSH_MEMORY_AUTH_TOKEN")]
        memory_auth_token: Option<String>,
        #[arg(long, env = "GOSH_AGENT_MEMORY_PRINCIPAL_ID")]
        memory_principal_id: Option<String>,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value = "8767")]
        port: u16,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value = "default")]
        watch_key: String,
        #[arg(long = "watch-context-key")]
        watch_context_key: Option<String>,
        #[arg(long, default_value = "default")]
        watch_agent_id: String,
        #[arg(long, default_value = "default")]
        watch_swarm_id: String,
        #[arg(long, default_value = "30")]
        poll_interval: u64,
        #[arg(long, default_value = "10.0")]
        watch_budget: f64,
    },
    /// Capture prompt or response from a coding CLI hook
    Capture {
        /// Agent instance name (for per-instance state isolation)
        #[arg(long)]
        name: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        event: String,
    },
    /// Run as MCP proxy (stdio), injecting auth and forwarding to authority
    McpProxy {
        /// Agent instance name
        #[arg(long)]
        name: String,
        #[arg(long)]
        default_key: Option<String>,
        /// Swarm ID injected into all memory tool calls (e.g., memory_recall,
        /// memory_store). Without this, calls default to swarm_id="default" on
        /// the server, missing facts written under a named swarm.
        #[arg(long)]
        default_swarm: Option<String>,
        #[arg(long, default_value_t = false)]
        full_memory_surface: bool,
    },
    /// Detect CLIs, write configs, register hooks
    Setup {
        /// Agent instance name
        #[arg(long)]
        name: String,
        #[arg(long)]
        authority: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long, env = "GOSH_MEMORY_AUTH_TOKEN")]
        auth_token: Option<String>,
        /// Memory namespace key (overrides git-based auto-detection).
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        key: Option<String>,
        /// Swarm ID for captured data (enables swarm-shared scope).
        #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
        swarm: Option<String>,
        /// Limit to specific platforms (repeatable: claude, codex, gemini).
        /// If omitted, all detected CLIs are configured.
        #[arg(long)]
        platform: Vec<String>,
        /// Where the agent's hooks AND MCP server registration land.
        ///
        /// `project` (default) — writes hooks and MCP config under
        /// `<cwd>/.<platform>/...` so they only fire when the coding CLI is
        /// launched from this directory. This is the privacy-safe default:
        /// prompts captured here never leak into other projects' agents.
        /// Each project where you want capture must run `gosh agent setup`
        /// from its own root.
        ///
        /// `user` — writes hooks and MCP config to the user-global path
        /// (`~/.<platform>/...`), so capture fires for **every** session of
        /// that coding CLI on this machine, regardless of working directory.
        /// Use only when you explicitly want one agent capturing across all
        /// your projects (rare; risk of cross-project prompt leakage).
        ///
        /// Codex MCP registration is always user-global (the upstream
        /// `codex mcp add` has no per-project mode); only Codex hooks honor
        /// this flag. A warning is printed when scope=project is asked for
        /// codex MCP.
        #[arg(long, default_value = "project", value_parser = ["project", "user"])]
        scope: String,
    },
    /// Replay buffered writes to authority
    ReplayBuffer {
        /// Agent instance name (for per-instance state isolation)
        #[arg(long)]
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { .. } => {
            tracing_subscriber::fmt::init();
            serve(cli.command).await
        }
        Command::Capture { name, platform, event } => {
            init_plugin_tracing();
            plugin::capture::run(&name, &platform, &event).await
        }
        Command::McpProxy { name, default_key, default_swarm, full_memory_surface } => {
            init_plugin_tracing();
            plugin::proxy::run(
                &name,
                default_key.as_deref(),
                default_swarm.as_deref(),
                full_memory_surface,
            )
            .await
        }
        Command::Setup { name, authority, token, auth_token, key, swarm, platform, scope } => {
            init_plugin_tracing();
            plugin::setup::run(
                &name,
                authority.as_deref(),
                token.as_deref(),
                auth_token.as_deref(),
                key.as_deref(),
                swarm.as_deref(),
                &platform,
                &scope,
            )
            .await
        }
        Command::ReplayBuffer { name } => {
            init_plugin_tracing();
            let config = plugin::config::GlobalConfig::load(&name)?;
            let url = config.authority_url.trim_end_matches('/');
            let transport =
                HttpTransport::new(url, config.token.clone(), config.principal_auth_token.clone());
            let mcp_client = client::McpClient::new(transport, "gosh-agent-plugin");
            plugin::buffer::replay(&name, |args| async {
                mcp_client.call_tool("memory_write", args).await
            })
            .await
        }
    }
}

fn init_plugin_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("gosh_agent=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Contents of the bootstrap file written by CLI and deleted after agent start.
#[derive(Deserialize)]
struct BootstrapData {
    join_token: String,
    /// Base64-encoded 32-byte X25519 private key.
    secret_key: String,
}

#[allow(clippy::too_many_arguments)]
async fn serve(cmd: Command) -> anyhow::Result<()> {
    let Command::Serve {
        bootstrap_file,
        join,
        allow_insecure_inline_join,
        memory_url,
        memory_token,
        memory_auth_token,
        memory_principal_id,
        host,
        port,
        watch,
        watch_key,
        watch_context_key,
        watch_agent_id,
        watch_swarm_id,
        poll_interval,
        watch_budget,
    } = cmd
    else {
        unreachable!()
    };

    // Parse bootstrap file (contains join_token + secret_key), then delete it
    // to prevent secret key material from lingering on disk.
    let bootstrap: Option<BootstrapData> = match bootstrap_file {
        Some(ref path) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("reading bootstrap file: {path}"))?;
            let data = serde_json::from_str(&content).context("parsing bootstrap file")?;
            if let Err(e) = std::fs::remove_file(path) {
                tracing::warn!("failed to delete bootstrap file {path}: {e}");
            }
            Some(data)
        }
        None => None,
    };
    let join_from_bootstrap = bootstrap.as_ref().map(|b| b.join_token.clone());
    let secret_key_bytes = bootstrap
        .as_ref()
        .map(|b| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&b.secret_key)
                .context("decoding secret_key from bootstrap file")
        })
        .transpose()?;

    let effective_join = join.as_deref().or(join_from_bootstrap.as_deref());
    let resolved_memory = resolve_memory_connection(
        effective_join,
        allow_insecure_inline_join,
        memory_url.as_deref(),
        memory_token,
        memory_auth_token,
        memory_principal_id,
    )?;
    let mem_url = resolved_memory.memory_url.clone();
    let mem_token = resolved_memory.transport_token.clone();
    let principal_auth_token = resolved_memory.principal_token.clone();
    let pinned_client = resolved_memory.pinned_client.clone();
    let transport = if let Some(http_client) = resolved_memory.pinned_client {
        HttpTransport::with_client(&mem_url, mem_token.clone(), principal_auth_token, http_client)
    } else {
        HttpTransport::new(&mem_url, mem_token.clone(), principal_auth_token)
    };
    let memory = Arc::new(MemoryMcpClient::new(transport));

    let config = AgentConfig::default();
    config.validate()?;
    let pricing = Arc::new(PricingCatalog::load_default()?);

    // Build secret context for per-task key resolution (sealed-box encrypted
    // delivery).
    let secret_ctx = match secret_key_bytes {
        Some(bytes) => {
            if bytes.len() != 32 {
                bail!("secret_key in bootstrap file must be exactly 32 bytes, got {}", bytes.len());
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Some(crate::agent::run::SecretContext {
                memory_url: mem_url.clone(),
                transport_token: mem_token.clone(),
                principal_token: resolved_memory.principal_token.clone().unwrap_or_default(),
                private_key: Arc::new(x25519_dalek::StaticSecret::from(arr)),
                http: reqwest::Client::new(),
            })
        }
        None => bail!("--bootstrap-file is required (contains join_token and secret_key)"),
    };

    let agent = Agent::with_pricing(config.clone(), memory.clone(), secret_ctx, pricing);

    // Derive agent_id from principal_id: "agent:myagent" → "myagent"
    let agent_id = resolved_memory
        .principal_id
        .as_deref()
        .and_then(|pid| pid.strip_prefix("agent:"))
        .unwrap_or("default")
        .to_string();

    let app_state = Arc::new(AppState {
        agent,
        memory: memory.clone(),
        courier: Mutex::new(CourierListener::new(&mem_url, mem_token, pinned_client)),
        agent_id,
        default_context_key: watch_context_key.clone(),
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
    });

    sandbox::apply_agent_sandbox();

    let app = build_router(app_state.clone());
    let addr = format!("{host}:{port}");

    println!("GOSH Agent MCP Server");
    println!("  Listening on http://{addr}");
    println!("  Memory:      {mem_url}");
    println!("  Secrets:     per-task (resolved from memory at execution time)");
    println!("  Watch mode:  {}", if watch { "ON" } else { "off" });
    println!("  POST /mcp    → agent_start, agent_status");
    println!("  GET  /health → health check");

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if watch {
        // Use the real agent_id from principal when watch params are defaults
        let effective_watch_agent =
            if watch_agent_id == "default" { app_state.agent_id.clone() } else { watch_agent_id };
        let effective_watch_context_key = watch_context_key.unwrap_or_else(|| watch_key.clone());
        println!(
            "    watch: work_key={watch_key} context_key={effective_watch_context_key} agent={effective_watch_agent} swarm={watch_swarm_id} poll={poll_interval}s budget={watch_budget}"
        );
        let watch_config = WatchConfig {
            key: watch_key,
            context_key: effective_watch_context_key,
            agent_id: effective_watch_agent,
            swarm_id: watch_swarm_id,
            poll_interval: Duration::from_secs(poll_interval),
            budget_shell: watch_budget,
        };

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let watcher_state = app_state.clone();
        let watcher_memory = memory.clone();

        tokio::spawn(async move {
            watcher::run(watch_config, watcher_state, watcher_memory, cancel_rx).await;
        });

        info!("watch mode started");

        let result = axum::serve(listener, app).await;
        let _ = cancel_tx.send(true);
        result?;
    } else {
        axum::serve(listener, app).await?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ResolvedMemoryConnection {
    memory_url: String,
    transport_token: Option<String>,
    principal_token: Option<String>,
    principal_id: Option<String>,
    pinned_client: Option<reqwest::Client>,
}

fn resolve_memory_connection(
    join: Option<&str>,
    allow_insecure_inline_join: bool,
    memory_url_override: Option<&str>,
    memory_token_override: Option<String>,
    memory_principal_token_override: Option<String>,
    memory_principal_id_override: Option<String>,
) -> anyhow::Result<ResolvedMemoryConnection> {
    const DEFAULT_MEMORY_URL: &str = "http://127.0.0.1:8765";

    let mut state = if let Some(token) = join {
        if !allow_insecure_inline_join && !token.starts_with("gosh_join_") {
            bail!(
                "--join is insecure for raw tokens; use --bootstrap-file or add --allow-insecure-inline-join"
            );
        }
        let decoded = join::JoinToken::decode(token)?;
        MemoryAuthState::from_join_token(&decoded)
    } else {
        MemoryAuthState {
            memory_url: memory_url_override
                .unwrap_or(DEFAULT_MEMORY_URL)
                .trim_end_matches("/mcp")
                .to_string(),
            transport_token: memory_token_override.clone(),
            principal_id: memory_principal_id_override.clone(),
            principal_token: memory_principal_token_override.clone(),
            tls_fingerprint: None,
            tls_ca: None,
        }
    };

    // CLI overrides take precedence over join token values
    if let Some(url) = memory_url_override {
        state.memory_url = url.trim_end_matches("/mcp").to_string();
    }
    if let Some(token) = memory_token_override {
        state.transport_token = Some(token);
    }
    if let Some(token) = memory_principal_token_override {
        state.principal_token = Some(token);
    }
    if let Some(id) = memory_principal_id_override {
        state.principal_id = Some(id);
    }

    if state.tls_fingerprint.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        bail!("tls_fingerprint pinning is not supported yet; use tls_ca instead");
    }

    let pinned_client = match &state.tls_ca {
        Some(ca) if !ca.is_empty() => {
            let cert = reqwest::Certificate::from_pem(ca.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid memory CA: {e}"))?;
            Some(reqwest::Client::builder().tls_certs_only([cert]).build()?)
        }
        _ => None,
    };

    Ok(ResolvedMemoryConnection {
        memory_url: state.memory_url.trim_end_matches("/mcp").to_string(),
        transport_token: state.transport_token,
        principal_token: state.principal_token,
        principal_id: state.principal_id,
        pinned_client,
    })
}

#[cfg(test)]
mod auth_resolution_tests {
    use super::resolve_memory_connection;
    use crate::join::JoinToken;

    #[test]
    fn resolve_memory_connection_parses_join_bundle() {
        let join = JoinToken {
            url: "http://127.0.0.1:8765".into(),
            transport_token: Some("server-xyz".into()),
            principal_id: Some("agent:planner".into()),
            principal_token: Some("principal-abc".into()),
            fingerprint: None,
            ca: None,
        };
        let join_encoded = join.encode().unwrap();

        let resolved =
            resolve_memory_connection(Some(&join_encoded), true, None, None, None, None).unwrap();
        assert_eq!(resolved.memory_url, "http://127.0.0.1:8765");
        assert_eq!(resolved.transport_token.as_deref(), Some("server-xyz"));
        assert_eq!(resolved.principal_token.as_deref(), Some("principal-abc"));
        assert_eq!(resolved.principal_id.as_deref(), Some("agent:planner"));
    }

    #[test]
    fn resolve_memory_connection_honors_explicit_memory_url_override() {
        let join = JoinToken {
            url: "http://127.0.0.1:8765".into(),
            transport_token: Some("server-xyz".into()),
            principal_id: None,
            principal_token: Some("principal-abc".into()),
            fingerprint: None,
            ca: None,
        };
        let join_encoded = join.encode().unwrap();

        let resolved = resolve_memory_connection(
            Some(&join_encoded),
            true,
            Some("http://memory.example:9999/mcp"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(resolved.memory_url, "http://memory.example:9999");
    }
}
