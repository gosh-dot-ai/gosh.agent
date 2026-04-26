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

pub(crate) fn test_app_state_with_config_and_delays(
    config: AgentConfig,
    responses: Vec<Value>,
    delays: HashMap<String, Duration>,
) -> (Arc<AppState>, Arc<SyncMutex<MockState>>) {
    let (transport, mock_state) = MockTransport::new_with_delays(responses, delays);
    let memory = Arc::new(MemoryMcpClient::new(transport));
    let agent = Agent::new(config, memory.clone(), None);
    let state = Arc::new(AppState {
        agent,
        memory,
        courier: Mutex::new(CourierListener::new("http://127.0.0.1:1", None, None)),
        agent_id: "default".to_string(),
        default_context_key: None,
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(crate::server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
    });

    (state, mock_state)
}
