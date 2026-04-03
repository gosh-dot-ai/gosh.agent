// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

#[cfg(test)]
mod routing_contract {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;
    use serde_json::Value;

    use crate::agent::config::profile_by_id;
    use crate::agent::config::AgentConfig;
    use crate::agent::config::RoutingTier;
    use crate::client::memory::CourierSubscribeParams;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::memory::MemoryQueryParams;
    use crate::client::memory::MemoryStoreParams;
    use crate::client::McpTransport;

    /// Shared state between mock transport and test assertions.
    #[derive(Default)]
    struct MockState {
        responses: Vec<Value>,
        calls: Vec<(String, Value)>,
    }

    /// Mock transport that records tool calls and returns pre-configured
    /// responses.
    struct MockTransport {
        state: Arc<Mutex<MockState>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Value>) -> (Self, Arc<Mutex<MockState>>) {
            let state = Arc::new(Mutex::new(MockState { responses, calls: Vec::new() }));
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

            let mut st = self.state.lock().unwrap();
            st.calls.push((tool_name, args));

            let resp = if st.responses.is_empty() {
                json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"null"}]}})
            } else {
                st.responses.remove(0)
            };

            Ok((resp, Some("mock-session".to_string())))
        }
    }

    fn wrap_mcp_response(payload: &Value) -> Value {
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

    fn take_calls(state: &Arc<Mutex<MockState>>) -> Vec<(String, Value)> {
        state.lock().unwrap().calls.drain(..).collect()
    }

    // ---------------------------------------------------------------
    // Test: watcher/courier subscribes with correct structured filter
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn courier_subscribe_uses_target_filter() {
        let sub_resp = wrap_mcp_response(&json!({"sub_id": "sub-123"}));
        let (transport, mock_state) = MockTransport::new(vec![sub_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let agent_id = "test-agent";
        let agent_target = format!("agent:{agent_id}");
        let filter = json!({"kind": "task", "target": agent_target});

        let result = memory
            .courier_subscribe(CourierSubscribeParams {
                key: "default".to_string(),
                agent_id: agent_id.to_string(),
                swarm_id: "default".to_string(),
                connection_id: "conn-1".to_string(),
                filter: Some(filter.clone()),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "courier_subscribe");
        let sent_filter = args.get("filter").unwrap();
        assert_eq!(sent_filter.get("kind").unwrap(), "task");
        assert_eq!(sent_filter.get("target").unwrap().as_str().unwrap(), "agent:test-agent");
    }

    // ---------------------------------------------------------------
    // Test: poll fallback uses memory_query with same structured filter
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn poll_fallback_uses_memory_query_with_filter() {
        let query_resp = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let agent_id = "poll-agent";
        let agent_target = format!("agent:{agent_id}");

        // Simulate what poll_once does
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: agent_id.to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task",
                    "target": agent_target,
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(50),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "memory_query");
        let filter = args.get("filter").unwrap();
        assert_eq!(filter.get("kind").unwrap(), "task");
        assert_eq!(filter.get("target").unwrap().as_str().unwrap(), "agent:poll-agent");
        assert_eq!(args.get("sort_by").unwrap(), "created_at");
        assert_eq!(args.get("sort_order").unwrap(), "desc");
    }

    // ---------------------------------------------------------------
    // Test: result/session artifacts carry canonical metadata
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn result_session_carry_canonical_metadata() {
        let store_resp_1 = wrap_mcp_response(&json!({"fact_id": "result-fact-1"}));
        let store_resp_2 = wrap_mcp_response(&json!({"fact_id": "session-fact-1"}));
        let (transport, mock_state) = MockTransport::new(vec![store_resp_1, store_resp_2]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        // Store result with canonical metadata
        let result = memory
            .memory_store(MemoryStoreParams {
                key: "default".to_string(),
                agent_id: "agent-1".to_string(),
                swarm_id: "default".to_string(),
                fact: "The answer is 42".to_string(),
                kind: "task_result".to_string(),
                target: None,
                metadata: Some(json!({
                    "task_fact_id": "fact-abc",
                    "task_id": "ext-001",
                })),
            })
            .await;
        assert!(result.is_ok());

        // Store session with canonical metadata
        let result = memory
            .memory_store(MemoryStoreParams {
                key: "default".to_string(),
                agent_id: "agent-1".to_string(),
                swarm_id: "default".to_string(),
                fact: "Agent completed task".to_string(),
                kind: "task_session".to_string(),
                target: None,
                metadata: Some(json!({
                    "task_fact_id": "fact-abc",
                    "task_id": "ext-001",
                })),
            })
            .await;
        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 2);

        let (name1, args1) = &calls[0];
        assert_eq!(name1, "memory_store");
        assert_eq!(args1.get("kind").unwrap(), "task_result");
        let meta1 = args1.get("metadata").unwrap();
        assert_eq!(meta1.get("task_fact_id").unwrap(), "fact-abc");
        assert_eq!(meta1.get("task_id").unwrap(), "ext-001");

        let (name2, args2) = &calls[1];
        assert_eq!(name2, "memory_store");
        assert_eq!(args2.get("kind").unwrap(), "task_session");
        let meta2 = args2.get("metadata").unwrap();
        assert_eq!(meta2.get("task_fact_id").unwrap(), "fact-abc");
        assert_eq!(meta2.get("task_id").unwrap(), "ext-001");
    }

    // ---------------------------------------------------------------
    // Test: status uses latest-only queries (canonical metadata linkage)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn status_queries_use_latest_only() {
        let result_resp = wrap_mcp_response(&json!({
            "facts": [{
                "id": "r1",
                "fact": "Latest result",
                "kind": "task_result",
                "metadata": {"task_fact_id": "fact-x", "task_id": "ext-1"}
            }]
        }));
        let session_resp = wrap_mcp_response(&json!({
            "facts": [{
                "id": "s1",
                "fact": "Latest session",
                "kind": "task_session",
                "metadata": {"task_fact_id": "fact-x", "task_id": "ext-1"}
            }]
        }));

        let (transport, mock_state) = MockTransport::new(vec![result_resp, session_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        // Query latest result
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task_result",
                    "metadata.task_fact_id": "fact-x",
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(1),
            })
            .await
            .unwrap();

        let fact = result.get("facts").unwrap().as_array().unwrap().first().unwrap();
        assert_eq!(fact.get("fact").unwrap(), "Latest result");

        // Query latest session
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task_session",
                    "metadata.task_fact_id": "fact-x",
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(1),
            })
            .await
            .unwrap();

        let fact = result.get("facts").unwrap().as_array().unwrap().first().unwrap();
        assert_eq!(fact.get("fact").unwrap(), "Latest session");

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 2);

        // Both queries must use sort_by=created_at, sort_order=desc, limit=1
        for (name, args) in &calls {
            assert_eq!(name, "memory_query");
            assert_eq!(args.get("sort_by").unwrap(), "created_at");
            assert_eq!(args.get("sort_order").unwrap(), "desc");
            assert_eq!(args.get("limit").unwrap(), 1);
        }
    }

    // ---------------------------------------------------------------
    // Test: memory_get wrapper serializes params correctly
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn memory_get_wrapper_serializes_params() {
        let get_resp = wrap_mcp_response(&json!({
            "id": "fact-1",
            "kind": "task",
            "fact": "Do stuff",
        }));
        let (transport, mock_state) = MockTransport::new(vec![get_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_get(crate::client::memory::MemoryGetParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
                fact_id: "fact-1".to_string(),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_get");
        assert_eq!(args.get("fact_id").unwrap(), "fact-1");
        assert_eq!(args.get("key").unwrap(), "ns");
    }

    // ---------------------------------------------------------------
    // Test: memory_query wrapper serializes params correctly
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn memory_query_wrapper_serializes_params() {
        let query_resp = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_query(MemoryQueryParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
                filter: json!({"kind": "task", "target": "agent:x"}),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(5),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_query");
        assert_eq!(args.get("filter").unwrap().get("kind").unwrap(), "task");
        assert_eq!(args.get("sort_by").unwrap(), "created_at");
        assert_eq!(args.get("limit").unwrap(), 5);
    }

    #[tokio::test]
    async fn memory_get_config_wrapper_serializes_params() {
        let get_resp = wrap_mcp_response(&json!({
            "schema_version": 1,
            "embedding_model": "openai/text-embedding-3-large",
        }));
        let (transport, mock_state) = MockTransport::new(vec![get_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_get_config(crate::client::memory::MemoryGetConfigParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_get_config");
        assert_eq!(args.get("key").unwrap(), "ns");
        assert_eq!(args.get("agent_id").unwrap(), "a");
    }

    #[test]
    fn cli_profiles_enforce_single_process_and_cooldown() {
        for profile_id in ["claude_code_cli", "codex_cli", "gemini_cli"] {
            let profile = profile_by_id(profile_id).expect("profile must exist");
            assert_eq!(profile.max_concurrency, 1, "{profile_id} must stay serialized");
            assert!(
                profile.cooldown_secs >= 600,
                "{profile_id} cooldown must stay at least 10 minutes"
            );
        }
    }

    #[test]
    fn cli_runtime_overrides_resolve_without_rebuild() {
        let mut config = AgentConfig::default();
        config.fast_profile = "claude_code_cli".to_string();
        config.claude_cli_bin = Some("/opt/bin/claude".to_string());
        config.claude_cli_cooldown_secs = Some(1200);

        let profile = config.execution_profile(RoutingTier::Fast).unwrap();
        let resolved = config.resolve_cli_command(profile).unwrap();

        assert_eq!(resolved.bin, "/opt/bin/claude");
        assert_eq!(resolved.cooldown_secs, 1200);
        assert_eq!(resolved.max_concurrency, 1);
        assert_eq!(resolved.args_prefix, vec!["-p".to_string()]);
    }

    #[test]
    fn persisted_profile_runtime_policy_can_tighten_cli_limits() {
        let mut config = AgentConfig::default();
        config.fast_profile = "claude_code_cli".to_string();
        config.max_parallel_tasks = 2;
        config.profile_runtime.entry("claude_code_cli".to_string()).or_default().cooldown_secs =
            Some(1800);

        let profile = config.execution_profile(RoutingTier::Fast).unwrap();
        let resolved = config.resolve_cli_command(profile).unwrap();

        assert_eq!(resolved.cooldown_secs, 1800);
        assert_eq!(resolved.max_concurrency, 1);
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter rejects string target that is a
    // different agent (mirrors dispatch_sse_event string-target branch)
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_rejects_string_target_mismatch() {
        let agent_id = "my-agent";
        let agent_target = format!("agent:{agent_id}");

        // String-form target pointing to a different agent
        let payload = json!({
            "id": "task-1",
            "kind": "task",
            "target": "agent:other-agent",
        });
        let target_ok = match payload.get("target") {
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(&agent_target)),
            Some(Value::String(s)) => s == &agent_target,
            _ => false,
        };
        assert!(!target_ok, "string target for wrong agent must be filtered");
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter accepts string target matching
    // this agent (mirrors dispatch_sse_event string-target branch)
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_accepts_string_target_match() {
        let agent_id = "my-agent";
        let agent_target = format!("agent:{agent_id}");

        let payload = json!({
            "id": "task-1",
            "kind": "task",
            "target": "agent:my-agent",
        });
        let target_ok = match payload.get("target") {
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(&agent_target)),
            Some(Value::String(s)) => s == &agent_target,
            _ => false,
        };
        assert!(target_ok, "string target for this agent must be accepted");
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter rejects non-"task" kind even when
    // target matches (e.g. kind="note")
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_rejects_non_task_kind_with_valid_target() {
        let payload = json!({
            "id": "note-1",
            "kind": "note",
            "target": ["agent:my-agent"],
        });
        let kind_ok = payload.get("kind").and_then(|v| v.as_str()) == Some("task");
        assert!(!kind_ok, "kind=note must be filtered even when target matches");
    }

    // ---------------------------------------------------------------
    // Negative-path: config_loader rejects bad schema_version via
    // production parse_agent_config (covers tests.rs coverage gap)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn config_loader_rejects_unsupported_schema_version() {
        use crate::agent::config_loader::load_agent_config;

        let fact = json!({
            "facts": [{
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 99,
                    "agent_id": "planner",
                    "swarm_id": "s"
                }
            }]
        });
        let wrap_resp = wrap_mcp_response(&fact);
        let (transport, _) = MockTransport::new(vec![wrap_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let err = load_agent_config(&memory, &AgentConfig::default(), "default", "planner", "s")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("UNSUPPORTED_AGENT_CONFIG_SCHEMA_VERSION"),
            "expected schema version rejection, got: {err}"
        );
    }

    // ---------------------------------------------------------------
    // Negative-path: config_loader rejects target mismatch via
    // production parse_agent_config
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn config_loader_rejects_target_mismatch() {
        use crate::agent::config_loader::load_agent_config;

        let fact = json!({
            "facts": [{
                "target": ["agent:someone-else"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "planner",
                    "swarm_id": "s"
                }
            }]
        });
        let wrap_resp = wrap_mcp_response(&fact);
        let (transport, _) = MockTransport::new(vec![wrap_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let err = load_agent_config(&memory, &AgentConfig::default(), "default", "planner", "s")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("AGENT_CONFIG_TARGET_MISMATCH"),
            "expected target mismatch rejection, got: {err}"
        );
    }
}
