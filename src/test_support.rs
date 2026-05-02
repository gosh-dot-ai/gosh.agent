// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use serde_json::json;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::agent::config::AgentConfig;
use crate::agent::Agent;
use crate::client::memory::MemoryMcpClient;
use crate::client::McpTransport;
use crate::courier::CourierListener;
use crate::server::AppState;

#[derive(Default)]
pub(crate) struct MockState {
    pub responses: VecDeque<Value>,
    pub calls: Vec<(String, Value)>,
    pub delays: HashMap<String, Duration>,
}

pub(crate) struct MockTransport {
    state: Arc<SyncMutex<MockState>>,
}

impl MockTransport {
    pub(crate) fn new(responses: Vec<Value>) -> (Self, Arc<SyncMutex<MockState>>) {
        Self::new_with_delays(responses, HashMap::new())
    }

    pub(crate) fn new_with_delays(
        responses: Vec<Value>,
        delays: HashMap<String, Duration>,
    ) -> (Self, Arc<SyncMutex<MockState>>) {
        let state = Arc::new(SyncMutex::new(MockState {
            responses: responses.into(),
            calls: Vec::new(),
            delays,
        }));
        (Self { state: state.clone() }, state)
    }
}

#[async_trait]
impl McpTransport for MockTransport {
    async fn send(
        &self,
        body: &Value,
        _session_id: Option<&str>,
    ) -> anyhow::Result<(Value, Option<String>)> {
        let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");

        if method == "initialize" {
            return Ok((
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": { "name": "mock", "version": "0.1.0" }
                    }
                }),
                Some("mock-session".to_string()),
            ));
        }

        if method == "notifications/initialized" {
            return Ok((json!({}), Some("mock-session".to_string())));
        }

        let tool_name =
            body.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let args = body.pointer("/params/arguments").cloned().unwrap_or(json!({}));
        let delay = self.state.lock().delays.get(&tool_name).cloned();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        let mut state = self.state.lock();
        state.calls.push((tool_name, args));

        let response = if state.responses.is_empty() {
            json!({
                "jsonrpc":"2.0",
                "id":1,
                "result":{"content":[{"type":"text","text":"null"}]}
            })
        } else {
            state.responses.pop_front().unwrap()
        };

        Ok((response, Some("mock-session".to_string())))
    }
}

pub(crate) fn wrap_mcp_response(payload: &Value) -> Value {
    let text = serde_json::to_string(payload).unwrap();
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": false
        }
    })
}

pub(crate) fn take_calls(state: &Arc<SyncMutex<MockState>>) -> Vec<(String, Value)> {
    state.lock().calls.drain(..).collect()
}

pub(crate) fn test_app_state(responses: Vec<Value>) -> (Arc<AppState>, Arc<SyncMutex<MockState>>) {
    test_app_state_with_delays(responses, HashMap::new())
}

pub(crate) fn test_app_state_with_delays(
    responses: Vec<Value>,
    delays: HashMap<String, Duration>,
) -> (Arc<AppState>, Arc<SyncMutex<MockState>>) {
    test_app_state_with_config_and_delays(AgentConfig::default(), responses, delays)
}

/// Build an `AppState` with explicit overrides for the OAuth fixture
/// fields. Used by the OAuth integration tests that need a real
/// (tempdir-backed) `ClientStore` so save/reload is exercised, plus
/// a known admin token so the admin-bearer middleware can be hit.
pub(crate) fn test_app_state_with_oauth(
    oauth_clients_path: std::path::PathBuf,
    oauth_dcr_enabled: bool,
    admin_token: &str,
) -> Arc<AppState> {
    let (transport, _) = MockTransport::new(Vec::new());
    let memory = Arc::new(MemoryMcpClient::new(transport));
    let agent = Agent::new(AgentConfig::default(), memory.clone(), None);
    let oauth_clients_store = crate::oauth::clients::ClientStore::empty_at(oauth_clients_path);
    // Tests that exercise the token surface override this via
    // `with_token_store` — default fixture uses a `/dev/null`-rooted
    // path so any accidental mint that flushes panics noisily.
    let oauth_tokens_store = crate::oauth::tokens::TokenStore::empty_at(std::path::PathBuf::from(
        "/dev/null/oauth-test-fixture-tokens",
    ));
    Arc::new(AppState {
        agent,
        memory,
        courier: Mutex::new(CourierListener::new("http://127.0.0.1:1", None, None)),
        agent_id: "default".to_string(),
        default_context_key: None,
        default_key: None,
        default_swarm_id: None,
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(crate::server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
        mcp_events: Default::default(),
        oauth_dcr_enabled,
        oauth_clients: Mutex::new(oauth_clients_store),
        oauth_sessions: Mutex::new(crate::oauth::sessions::SessionStore::new()),
        oauth_tokens: Mutex::new(oauth_tokens_store),
        admin_token: admin_token.to_string(),
    })
}

/// Build an `AppState` with explicit overrides for both OAuth client
/// and token stores. Used by `/oauth/token` and `/oauth/revoke`
/// integration tests where the token store needs a real (tempdir-
/// backed) persistence path so mint/rotate/revoke flush behaviour is
/// covered end-to-end.
#[allow(dead_code)] // wired in upcoming 7c integration tests
pub(crate) fn test_app_state_with_oauth_full(
    oauth_clients_path: std::path::PathBuf,
    oauth_tokens_path: std::path::PathBuf,
    oauth_dcr_enabled: bool,
    admin_token: &str,
) -> Arc<AppState> {
    let (transport, _) = MockTransport::new(Vec::new());
    let memory = Arc::new(MemoryMcpClient::new(transport));
    let agent = Agent::new(AgentConfig::default(), memory.clone(), None);
    let oauth_clients_store = crate::oauth::clients::ClientStore::empty_at(oauth_clients_path);
    let oauth_tokens_store = crate::oauth::tokens::TokenStore::empty_at(oauth_tokens_path);
    Arc::new(AppState {
        agent,
        memory,
        courier: Mutex::new(CourierListener::new("http://127.0.0.1:1", None, None)),
        agent_id: "default".to_string(),
        default_context_key: None,
        default_key: None,
        default_swarm_id: None,
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(crate::server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
        mcp_events: Default::default(),
        oauth_dcr_enabled,
        oauth_clients: Mutex::new(oauth_clients_store),
        oauth_sessions: Mutex::new(crate::oauth::sessions::SessionStore::new()),
        oauth_tokens: Mutex::new(oauth_tokens_store),
        admin_token: admin_token.to_string(),
    })
}

pub(crate) fn test_app_state_with_config_and_delays(
    config: AgentConfig,
    responses: Vec<Value>,
    delays: HashMap<String, Duration>,
) -> (Arc<AppState>, Arc<SyncMutex<MockState>>) {
    let (transport, mock_state) = MockTransport::new_with_delays(responses, delays);
    let memory = Arc::new(MemoryMcpClient::new(transport));
    let agent = Agent::new(config, memory.clone(), None);
    // Test fixture: empty in-memory client store, DCR on (default
    // production posture), random admin token. None of the
    // /admin/* or /oauth/* endpoints are exercised by the existing
    // mock-driven tests; the OAuth surface is covered separately by
    // its own integration tests against a real router.
    let oauth_clients_store = crate::oauth::clients::ClientStore::empty_at(
        std::path::PathBuf::from("/dev/null/oauth-test-fixture"),
    );
    let oauth_tokens_store = crate::oauth::tokens::TokenStore::empty_at(std::path::PathBuf::from(
        "/dev/null/oauth-test-fixture-tokens",
    ));
    let state = Arc::new(AppState {
        agent,
        memory,
        courier: Mutex::new(CourierListener::new("http://127.0.0.1:1", None, None)),
        agent_id: "default".to_string(),
        default_context_key: None,
        default_key: None,
        default_swarm_id: None,
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(crate::server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
        mcp_events: Default::default(),
        oauth_dcr_enabled: true,
        oauth_clients: Mutex::new(oauth_clients_store),
        oauth_sessions: Mutex::new(crate::oauth::sessions::SessionStore::new()),
        oauth_tokens: Mutex::new(oauth_tokens_store),
        admin_token: "test-admin-token".to_string(),
    });

    (state, mock_state)
}
