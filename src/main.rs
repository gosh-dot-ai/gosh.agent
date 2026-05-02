// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

mod agent;
mod auth;
mod client;
mod courier;
mod crypto;
mod join;
mod keychain;
mod llm;
mod oauth;
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
use tokio::sync::Mutex;
use tracing::info;
use tracing_subscriber::EnvFilter;

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
#[command(
    name = "gosh-agent",
    about = "GOSH AI Agent — MCP server, capture plugin, MCP proxy",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Run as MCP server (default agent mode).
    ///
    /// Reads configuration from `~/.gosh/agent/state/<name>/config.toml`
    /// (the `GlobalConfig` written by `gosh-agent setup`). Every flag
    /// below is optional and overrides the corresponding config value
    /// when present — `gosh agent start` invokes this with just `--name`,
    /// the rest is for dev iteration and ad-hoc debugging.
    Serve {
        /// Agent instance name. Drives both the GlobalConfig load
        /// (`~/.gosh/agent/state/<name>/config.toml`) and the keychain
        /// lookup (service "gosh", account "agent/<name>"). Required.
        #[arg(long)]
        name: String,
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
        /// Override `GlobalConfig.host`. Falls back to `127.0.0.1`.
        #[arg(long)]
        host: Option<String>,
        /// Override `GlobalConfig.port`. Falls back to `8767`.
        #[arg(long)]
        port: Option<u16>,
        /// Force the watcher loop on. Mutually exclusive with `--no-watch`.
        /// Without either flag, falls back to `GlobalConfig.watch`.
        #[arg(long, conflicts_with = "no_watch")]
        watch: bool,
        /// Force the watcher loop off. Mutually exclusive with `--watch`.
        #[arg(long, conflicts_with = "watch")]
        no_watch: bool,
        #[arg(long)]
        watch_key: Option<String>,
        #[arg(long = "watch-context-key")]
        watch_context_key: Option<String>,
        #[arg(long)]
        watch_agent_id: Option<String>,
        #[arg(long)]
        watch_swarm_id: Option<String>,
        #[arg(long)]
        poll_interval: Option<u64>,
        #[arg(long)]
        watch_budget: Option<f64>,
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
    /// Run as MCP proxy (stdio): a thin transport bridge between the
    /// coding-CLI process and the agent daemon's HTTP `/mcp`. The daemon
    /// owns the memory MCP relay, key/swarm scoping, tools/list filtering,
    /// and tool-name allowlist; this binary just forwards JSON-RPC.
    McpProxy {
        /// Agent instance name (used for log identification only — the
        /// daemon reads its own per-instance config).
        #[arg(long)]
        name: String,
        /// Daemon host. When absent, the proxy reads the bind host
        /// from the per-instance `GlobalConfig` for `--name` and
        /// rewrites bind placeholders (`0.0.0.0` / `::`) to loopback.
        /// Pass explicitly only when the proxy and daemon run in
        /// different network namespaces.
        #[arg(long)]
        daemon_host: Option<String>,
        /// Daemon port. When absent, the proxy reads it from the
        /// per-instance `GlobalConfig` for `--name`. Pass explicitly
        /// only to override (rare — `gosh-agent setup` writes the
        /// configured port back into the generated MCP args, so a
        /// freshly-`setup`-ped instance never needs an override).
        #[arg(long)]
        daemon_port: Option<u16>,
        /// Deprecated, kept for backwards compatibility with `.mcp.json`
        /// files written by older `gosh-agent setup` runs. The daemon now
        /// applies key injection itself based on the per-instance
        /// `GlobalConfig`. The proxy ignores any value passed here.
        #[arg(long, hide = true)]
        default_key: Option<String>,
        /// Deprecated. See `--default-key` — same story for swarm scope.
        #[arg(long, hide = true)]
        default_swarm: Option<String>,
        /// Deprecated. The daemon owns the tools/list filter now.
        #[arg(long, hide = true, default_value_t = false)]
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
        /// Without this flag, setup preserves the existing swarm setting.
        #[arg(long, conflicts_with = "no_swarm", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        swarm: Option<String>,
        /// Clear any previously configured swarm and capture as agent-private.
        #[arg(long)]
        no_swarm: bool,
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

        // ── Daemon-spawn config (canonical source of truth) ─────────────
        //
        // After the MCP unification work
        // (<gosh.cli>/specs/agent_mcp_unification.md) `gosh agent setup`
        // is the single place where agent-instance settings live. The
        // daemon and the autostart artifact (launchd / systemd) read all
        // of these from `GlobalConfig` at startup; `gosh agent start` and
        // `gosh agent stop` are pure process-lifecycle and don't take
        // these flags themselves. Re-running setup with a subset of flags
        // patches just those (semantics documented per-flag).
        /// Daemon HTTP bind host. Defaults to `127.0.0.1` when unset.
        #[arg(long)]
        host: Option<String>,

        /// Daemon HTTP bind port. Defaults to `8767` when unset.
        #[arg(long)]
        port: Option<u16>,

        /// Enable the watcher loop. Mutually exclusive with `--no-watch`.
        #[arg(long, conflicts_with = "no_watch")]
        watch: bool,

        /// Disable the watcher loop. Mutually exclusive with `--watch`.
        #[arg(long, conflicts_with = "watch")]
        no_watch: bool,

        /// Namespace key the watcher subscribes to for task discovery.
        #[arg(long)]
        watch_key: Option<String>,

        /// Swarm filter for the watcher's courier subscription.
        #[arg(long)]
        watch_swarm_id: Option<String>,

        /// Agent-id filter for the watcher (default: derived from
        /// principal_id).
        #[arg(long)]
        watch_agent_id: Option<String>,

        /// Context retrieval namespace, distinct from `--watch-key` when an
        /// agent watches one namespace and recalls context from another.
        #[arg(long)]
        watch_context_key: Option<String>,

        /// USD budget cap for autonomous task execution.
        #[arg(long)]
        watch_budget: Option<f64>,

        /// Polling interval (seconds) for the watcher loop fallback when
        /// courier SSE is unavailable.
        #[arg(long)]
        poll_interval: Option<u64>,

        /// Daemon log level persisted into per-instance config. `RUST_LOG`
        /// still wins when set for one-off diagnostics.
        #[arg(long)]
        log_level: Option<plugin::config::LogLevel>,

        /// Disable Dynamic Client Registration on the daemon's
        /// `/oauth/register` endpoint. By default the daemon accepts
        /// unauthenticated DCR per RFC 7591 (the standard MCP-spec
        /// path); pass `--no-oauth-dcr` to require explicit
        /// per-client registration via `gosh agent oauth clients
        /// register --name <X>` instead.
        ///
        /// Same shape as `--no-autostart`: setup declares the
        /// desired state on every run. Absence ⇒ DCR on, presence ⇒
        /// DCR off. Re-running without the flag re-enables DCR.
        #[arg(long)]
        no_oauth_dcr: bool,

        /// Skip writing the launchd / systemd autostart artifact. The
        /// operator supervises the daemon themselves (docker-compose,
        /// runit, supervisord, etc.).
        #[arg(long)]
        no_autostart: bool,
    },
    /// Tear down an agent instance: stop the daemon, remove autostart
    /// artifact, hooks/MCP entries, and per-instance state.
    Uninstall {
        /// Agent instance name.
        #[arg(long)]
        name: String,
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
        Command::Serve { .. } => serve(cli.command).await,
        Command::Capture { name, platform, event } => {
            init_plugin_tracing();
            plugin::capture::run(&name, &platform, &event).await
        }
        Command::McpProxy {
            name,
            daemon_host,
            daemon_port,
            default_key: _,
            default_swarm: _,
            full_memory_surface: _,
        } => {
            init_plugin_tracing();
            plugin::proxy::run(&name, daemon_host.as_deref(), daemon_port).await
        }
        Command::Setup {
            name,
            authority,
            token,
            auth_token,
            key,
            swarm,
            no_swarm,
            platform,
            scope,
            host,
            port,
            watch,
            no_watch,
            watch_key,
            watch_swarm_id,
            watch_agent_id,
            watch_context_key,
            watch_budget,
            poll_interval,
            log_level,
            no_oauth_dcr,
            no_autostart,
        } => {
            init_plugin_tracing();
            // Three-state mapping: explicit `--watch` → Some(true),
            // explicit `--no-watch` → Some(false), neither → None
            // (preserve existing config). clap enforces mutual exclusion.
            let watch_arg = if watch {
                Some(true)
            } else if no_watch {
                Some(false)
            } else {
                None
            };
            // Two-state for DCR (matches `--no-autostart` style):
            // absence ⇒ DCR on, presence ⇒ DCR off. Always
            // overwrites GlobalConfig — re-running setup without
            // `--no-oauth-dcr` re-enables DCR by design.
            let oauth_dcr_arg = !no_oauth_dcr;
            plugin::setup::run(plugin::setup::SetupArgs {
                agent_name: &name,
                authority_url: authority.as_deref(),
                token: token.as_deref(),
                principal_auth_token: auth_token.as_deref(),
                key: key.as_deref(),
                swarm_id: swarm.as_deref(),
                no_swarm,
                platforms: &platform,
                scope: &scope,
                host: host.as_deref(),
                port,
                watch: watch_arg,
                watch_key: watch_key.as_deref(),
                watch_swarm_id: watch_swarm_id.as_deref(),
                watch_agent_id: watch_agent_id.as_deref(),
                watch_context_key: watch_context_key.as_deref(),
                watch_budget,
                poll_interval,
                log_level,
                oauth_dcr_enabled: oauth_dcr_arg,
                no_autostart,
            })
            .await
        }
        Command::Uninstall { name } => {
            init_plugin_tracing();
            plugin::uninstall::run(&name).await
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

fn init_daemon_tracing(log_level: plugin::config::LogLevel) {
    let filter = match std::env::var_os("RUST_LOG") {
        Some(_) => EnvFilter::from_default_env(),
        None => EnvFilter::new(format!(
            "gosh_agent={level},gosh_agent::http={level},hyper=warn,h2=warn,tower=warn,tower_http=warn,reqwest=warn",
            level = log_level.as_str(),
        )),
    };

    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

#[allow(clippy::too_many_arguments)]
async fn serve(cmd: Command) -> anyhow::Result<()> {
    let Command::Serve {
        name,
        join,
        allow_insecure_inline_join,
        memory_url,
        memory_token,
        memory_auth_token,
        memory_principal_id,
        host: host_arg,
        port: port_arg,
        watch: watch_flag,
        no_watch: no_watch_flag,
        watch_key: watch_key_arg,
        watch_context_key: watch_context_key_arg,
        watch_agent_id: watch_agent_id_arg,
        watch_swarm_id: watch_swarm_id_arg,
        poll_interval: poll_interval_arg,
        watch_budget: watch_budget_arg,
    } = cmd
    else {
        unreachable!()
    };

    // Load MCP-forwarding defaults and credentials from the per-instance
    // state the CLI provisioned. The CLI is the canonical writer (during
    // `gosh agent create` / `gosh agent import` / `gosh agent setup`); the
    // daemon is read-only.
    //
    // `key` / `swarm_id` come from `GlobalConfig` (the agent's bound scope —
    // distinct from `--watch-key` / `--watch-swarm-id`, which subscribe the
    // watcher loop to a task-discovery namespace; agents legitimately watch
    // one namespace and forward MCP calls in another).
    //
    // `principal_token` / `join_token` / `secret_key` come from the OS
    // keychain entry the CLI wrote at provisioning time. There is no
    // bootstrap-file path anymore — the daemon used to receive an ephemeral
    // file from the CLI on each spawn, but with the daemon now able to
    // read keychain directly that intermediate channel is unnecessary
    // (and its existence forced the CLI into a write-temp-secret /
    // delete dance that was its own attack surface).
    let global_config = plugin::config::GlobalConfig::load(&name).with_context(|| {
        format!(
            "could not load per-instance config for agent '{name}' \
             (~/.gosh/agent/state/{name}/config.toml). Run `gosh agent setup --instance {name}` \
             to provision it"
        )
    })?;
    init_daemon_tracing(global_config.log_level);

    let forwarding_default_key = global_config.key.clone().filter(|s| !s.is_empty());
    let forwarding_default_swarm_id = global_config.swarm_id.clone().filter(|s| !s.is_empty());

    // CLI flags override the corresponding `GlobalConfig` value; in their
    // absence we fall back to config, then to baked-in defaults. Setup is
    // the canonical writer, so the common case is "no flags, all values
    // from config".
    let host =
        host_arg.or_else(|| global_config.host.clone()).unwrap_or_else(|| "127.0.0.1".to_string());
    let port = port_arg.or(global_config.port).unwrap_or(8767);
    let watch = if watch_flag {
        true
    } else if no_watch_flag {
        false
    } else {
        global_config.watch
    };
    let watch_key = watch_key_arg
        .or_else(|| global_config.watch_key.clone())
        .unwrap_or_else(|| "default".to_string());
    let watch_context_key =
        watch_context_key_arg.or_else(|| global_config.watch_context_key.clone());
    let watch_agent_id = watch_agent_id_arg
        .or_else(|| global_config.watch_agent_id.clone())
        .unwrap_or_else(|| "default".to_string());
    let watch_swarm_id = watch_swarm_id_arg
        .or_else(|| global_config.watch_swarm_id.clone())
        .unwrap_or_else(|| "default".to_string());
    let poll_interval = poll_interval_arg.or(global_config.poll_interval).unwrap_or(30);
    let watch_budget = watch_budget_arg.or(global_config.watch_budget).unwrap_or(10.0);

    let agent_secrets = keychain::AgentSecrets::load(&name)
        .with_context(|| format!("could not read keychain for agent '{name}'"))?
        .with_context(|| {
            format!(
                "no keychain entry found for agent '{name}'. \
                 Run `gosh agent create --instance {name}` or \
                 `gosh agent import` to provision credentials"
            )
        })?;

    let join_from_keychain = agent_secrets.join_token.clone();
    let secret_key_bytes = match agent_secrets.secret_key.as_deref() {
        Some(b64) => {
            use base64::Engine;
            Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .context("decoding secret_key from keychain")?,
            )
        }
        None => None,
    };

    let effective_join = join.as_deref().or(join_from_keychain.as_deref());
    // CLI/env-provided memory auth token wins; otherwise fall back to the
    // keychain-stored agent principal token (the one the CLI provisioned at
    // create/import time).
    let effective_memory_auth = memory_auth_token.or_else(|| agent_secrets.principal_token.clone());
    let resolved_memory = resolve_memory_connection(
        effective_join,
        allow_insecure_inline_join,
        memory_url.as_deref(),
        memory_token,
        effective_memory_auth,
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
                bail!("secret_key from keychain must be exactly 32 bytes, got {}", bytes.len());
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
        None => bail!(
            "no `secret_key` found in keychain for agent '{name}'. \
             Run `gosh agent create --instance {name}` or `gosh agent import` to provision it"
        ),
    };

    let agent = Agent::with_pricing(config.clone(), memory.clone(), secret_ctx, pricing);

    // Derive agent_id from principal_id: "agent:myagent" → "myagent"
    let agent_id = resolved_memory
        .principal_id
        .as_deref()
        .and_then(|pid| pid.strip_prefix("agent:"))
        .unwrap_or("default")
        .to_string();

    // OAuth surface: load the persistent client store and mint a
    // fresh per-process admin token written to the state dir for
    // the CLI to find. Failures here are non-fatal — daemon still
    // serves /mcp and /health, but `/admin/*` and `/oauth/*` will
    // refuse callers (admin token mismatch / lock contention).
    let oauth_clients_store = oauth::clients::ClientStore::load(&name)
        .with_context(|| format!("loading OAuth client store for agent '{name}'"))?;
    let admin_token = oauth::admin_token::write_fresh_token(&name)
        .with_context(|| format!("provisioning admin token for agent '{name}'"))?;
    let oauth_sessions_store = oauth::sessions::SessionStore::new();
    let oauth_tokens_store = oauth::tokens::TokenStore::load(&name)
        .with_context(|| format!("loading OAuth token store for agent '{name}'"))?;

    let app_state = Arc::new(AppState {
        agent,
        memory: memory.clone(),
        courier: Mutex::new(CourierListener::new(&mem_url, mem_token, pinned_client)),
        agent_id,
        default_context_key: watch_context_key.clone(),
        default_key: forwarding_default_key,
        default_swarm_id: forwarding_default_swarm_id,
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
        mcp_events: Default::default(),
        oauth_dcr_enabled: global_config.oauth_dcr_enabled,
        oauth_clients: Mutex::new(oauth_clients_store),
        oauth_sessions: Mutex::new(oauth_sessions_store),
        oauth_tokens: Mutex::new(oauth_tokens_store),
        admin_token,
    });

    // Background sweep: every 60s, evict expired `/oauth/authorize`
    // sessions. The TTL is 10 minutes so a slower interval is fine —
    // we just don't want stale entries piling up indefinitely under
    // a noisy traffic pattern. Failures (mutex poisoning, etc.)
    // would log via tracing; the daemon keeps running.
    {
        let sweeper_state = app_state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                let removed = sweeper_state.oauth_sessions.lock().await.sweep();
                if removed > 0 {
                    tracing::debug!(removed, "oauth: swept expired authorize sessions");
                }
            }
        });
    }

    // Background sweep for expired access tokens. Refresh tokens
    // have no TTL — they live until explicitly revoked or the
    // backing client is removed — so this only touches in-memory
    // access state. Same 60s cadence as the session sweep; access
    // TTL is 1h so a few extra seconds of dead state is fine.
    {
        let sweeper_state = app_state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                let removed = sweeper_state.oauth_tokens.lock().await.sweep_access();
                if removed > 0 {
                    tracing::debug!(removed, "oauth: swept expired access tokens");
                }
            }
        });
    }

    sandbox::apply_agent_sandbox();

    let app = build_router(app_state.clone());
    let addr = format!("{host}:{port}");

    println!("GOSH Agent MCP Server");
    println!("  Listening on http://{addr}");
    println!("  Memory:      {mem_url}");
    println!("  Secrets:     per-task (resolved from memory at execution time)");
    println!("  Watch mode:  {}", if watch { "ON" } else { "off" });
    println!("  POST /mcp    → agent_start, agent_status");
    println!("  GET  /mcp    → MCP SSE progress stream");
    println!("  GET  /health → health check");

    // Surface non-loopback binds prominently — operator should know
    // the daemon is exposed beyond the host. The Bearer middleware on
    // `/mcp` plus the OAuth surface mean the wire is gated even
    // without TLS, but the daemon itself does NOT terminate TLS:
    // anything more than a curl-driven smoke test needs a TLS
    // terminator (Caddy / cloudflared / Tailscale Funnel). See the
    // operator runbook in `<gosh.cli>/docs/cli.md`.
    let bind_is_public =
        !host.starts_with("127.") && host != "localhost" && host != "::1" && host != "[::1]";
    if bind_is_public {
        println!();
        println!("  ⚠  Daemon is binding to a NON-LOOPBACK address ({host}).");
        println!("     The OAuth + Bearer surface protects /mcp, but the daemon");
        println!("     does NOT terminate TLS. Put Caddy / cloudflared / Tailscale");
        println!("     Funnel in front before pointing Claude.ai at this URL.");
        println!("     Runbook: <gosh.cli>/docs/cli.md#exposing-the-agent-to-the-internet");
    }

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

        // `into_make_service_with_connect_info` is what makes
        // `ConnectInfo<SocketAddr>` available to the admin
        // middleware so it can enforce loopback-only access.
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
        let _ = cancel_tx.send(true);
        result?;
    } else {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await?;
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
                "--join is insecure for raw tokens; rely on the keychain-provisioned join_token \
                 (CLI handles this) or add --allow-insecure-inline-join"
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
