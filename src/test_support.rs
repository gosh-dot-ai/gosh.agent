// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

#![cfg(test)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::agent::config::AgentConfig;
use crate::agent::Agent;
use crate::client::memory::MemoryMcpClient;
use crate::client::McpTransport;
use crate::courier::CourierListener;
use crate::llm::LlmProvider;
use crate::llm::LlmResponse;
use crate::llm::Message;
use crate::llm::ToolDef;
use crate::llm::Usage;
use crate::server::AppState;

#[derive(Default)]
pub(crate) struct MockState {
    pub responses: Vec<Value>,
    pub calls: Vec<(String, Value)>,
}

pub(crate) struct MockTransport {
    state: Arc<StdMutex<MockState>>,
}

impl MockTransport {
    pub(crate) fn new(responses: Vec<Value>) -> (Self, Arc<StdMutex<MockState>>) {
        let state = Arc::new(StdMutex::new(MockState { responses, calls: Vec::new() }));
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

        let mut state = self.state.lock().unwrap();
        state.calls.push((tool_name, args));

        let response = if state.responses.is_empty() {
            json!({
                "jsonrpc":"2.0",
                "id":1,
                "result":{"content":[{"type":"text","text":"null"}]}
            })
        } else {
            state.responses.remove(0)
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

pub(crate) fn take_calls(state: &Arc<StdMutex<MockState>>) -> Vec<(String, Value)> {
    state.lock().unwrap().calls.drain(..).collect()
}

pub(crate) struct StaticLlmProvider {
    text: String,
}

impl StaticLlmProvider {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[async_trait]
impl LlmProvider for StaticLlmProvider {
    async fn chat(
        &self,
        _model: &str,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
        _max_tokens: u32,
    ) -> anyhow::Result<LlmResponse> {
        Ok(LlmResponse {
            text: Some(self.text.clone()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            stop_reason: "stop".to_string(),
        })
    }
}

pub(crate) fn test_app_state(responses: Vec<Value>) -> (Arc<AppState>, Arc<StdMutex<MockState>>) {
    test_app_state_with_llm(responses, None)
}

pub(crate) fn test_app_state_with_llm(
    responses: Vec<Value>,
    llm: Option<Arc<dyn LlmProvider>>,
) -> (Arc<AppState>, Arc<StdMutex<MockState>>) {
    let (transport, mock_state) = MockTransport::new(responses);
    let memory = Arc::new(MemoryMcpClient::new(transport));
    let agent = Agent::new(AgentConfig::default(), memory.clone(), llm);
    let state = Arc::new(AppState {
        agent,
        memory,
        courier: Mutex::new(CourierListener::new("http://127.0.0.1:1", None, None)),
        session_counter: Mutex::new(0),
        dispatched_tasks: Mutex::new(crate::server::DispatchedTracker::default()),
        in_flight_tasks: Mutex::new(HashSet::new()),
        in_flight_by_agent: Mutex::new(HashMap::new()),
    });

    (state, mock_state)
}
