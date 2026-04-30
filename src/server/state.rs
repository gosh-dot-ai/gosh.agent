// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::Agent;
use crate::client::memory::MemoryMcpClient;
use crate::courier::CourierListener;
use crate::oauth::clients::ClientStore;
use crate::oauth::sessions::SessionStore;
use crate::oauth::tokens::TokenStore;

pub(crate) const DISPATCH_TRACKER_CAPACITY: usize = 10_000;

#[derive(Default)]
pub struct DispatchedTracker {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DispatchedTracker {
    fn claim(&mut self, task_fact_id: &str) -> bool {
        if self.seen.contains(task_fact_id) {
            return false;
        }

        if self.order.len() >= DISPATCH_TRACKER_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }

        let owned = task_fact_id.to_string();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

/// Shared state across all request handlers.
pub struct AppState {
    pub agent: Agent,
    pub memory: Arc<MemoryMcpClient>,
    pub courier: Mutex<CourierListener>,
    /// Agent ID derived from principal_id (e.g., "agent:myagent" → "myagent").
    /// Used as default agent_id for memory MCP calls.
    pub agent_id: String,
    pub default_context_key: Option<String>,
    /// Daemon's fallback `key` for `memory_*` tool calls forwarded from
    /// the MCP gateway. Sourced from the per-instance `GlobalConfig.key`
    /// written by `gosh-agent setup` — the agent's bound namespace,
    /// distinct from the watcher's `--watch-key` (which is what the
    /// watcher loop subscribes to for task discovery; the two
    /// legitimately differ when an agent watches one namespace and
    /// answers in another). Per-call values from the inbound request
    /// always win — the default is only injected when the caller
    /// omits `key` entirely. `None` means no fallback; calls without
    /// an explicit `key` will fail downstream at memory.
    pub default_key: Option<String>,
    /// Daemon's fallback `swarm_id` for `memory_*` tool calls forwarded
    /// from the MCP gateway. Sourced from `GlobalConfig.swarm_id`. Same
    /// per-call semantics as `default_key`.
    pub default_swarm_id: Option<String>,
    pub session_counter: Mutex<u64>,
    pub dispatched_tasks: Mutex<DispatchedTracker>,
    pub in_flight_tasks: Mutex<HashSet<String>>,
    pub in_flight_by_agent: Mutex<HashMap<String, usize>>,

    /// Whether the OAuth `/oauth/register` endpoint accepts unauth'd
    /// Dynamic Client Registration requests. Mirrors
    /// `GlobalConfig.oauth_dcr_enabled` at startup; never mutated at
    /// runtime — to flip, operator re-runs `gosh agent setup
    /// [--oauth-dcr|--no-oauth-dcr]` and restarts the daemon.
    pub oauth_dcr_enabled: bool,
    /// Persistent OAuth client registry. Wrapped in `Mutex` because
    /// `register` / `revoke` flush to disk and we don't want
    /// concurrent writers stepping on each other's `clients.toml`.
    pub oauth_clients: Mutex<ClientStore>,
    /// In-memory store of pending OAuth `/authorize` sessions. Holds
    /// short-lived state (~10 min TTL) — PIN material, authorisation
    /// codes, PKCE challenges. A daemon restart drops everything;
    /// Claude.ai's flow either completes within a single daemon
    /// process lifetime or starts over. The background sweep task
    /// in `serve()` evicts expired entries on a 60-second interval.
    pub oauth_sessions: Mutex<SessionStore>,
    /// Issued access (in-memory) + refresh (persisted) tokens.
    /// Mutex-wrapped because mint/rotate/revoke flush refresh state
    /// to `~/.gosh/agent/state/<name>/oauth/tokens.toml` and the
    /// access-token sweep mutates the in-memory map. Daemon restart
    /// drops every active access token by design (forces a single
    /// `/oauth/token` refresh round-trip from each remote client);
    /// refresh tokens persist so Claude.ai doesn't have to redo the
    /// PIN dance after a daemon restart.
    pub oauth_tokens: Mutex<TokenStore>,
    /// Ephemeral admin token, regenerated on every daemon start and
    /// written to `~/.gosh/agent/state/<name>/admin.token` (mode
    /// 0600). The CLI reads that file when calling
    /// `/admin/oauth/*`; the bearer must match this in-memory value.
    /// Daemon restart invalidates outstanding admin tokens by design.
    pub admin_token: String,
}

impl AppState {
    pub async fn claim_dispatch(&self, task_fact_id: &str) -> bool {
        let mut dispatched = self.dispatched_tasks.lock().await;
        dispatched.claim(task_fact_id)
    }
}

#[cfg(test)]
mod tests {
    use super::DISPATCH_TRACKER_CAPACITY;
    use crate::test_support::test_app_state;

    #[tokio::test]
    async fn claim_dispatch_deduplicates_task_ids_across_watch_paths() {
        let (state, _) = test_app_state(vec![]);

        assert!(state.claim_dispatch("task-123").await);
        assert!(!state.claim_dispatch("task-123").await);
        assert!(state.claim_dispatch("task-456").await);
    }

    #[tokio::test]
    async fn claim_dispatch_evicts_oldest_entries_when_capacity_is_exceeded() {
        let (state, _) = test_app_state(vec![]);

        for i in 0..(DISPATCH_TRACKER_CAPACITY + 1) {
            assert!(state.claim_dispatch(&format!("task-{i}")).await);
        }

        let dispatched = state.dispatched_tasks.lock().await;
        assert_eq!(dispatched.len(), DISPATCH_TRACKER_CAPACITY);
        drop(dispatched);

        assert!(state.claim_dispatch("task-0").await);
    }
}
