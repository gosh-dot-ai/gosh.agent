// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

#[cfg(test)]
mod routing_contract {
    use std::sync::Arc;

    use serde_json::json;
    use serde_json::Value;

    use crate::client::memory::CourierSubscribeParams;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::memory::MemoryQueryParams;
    use crate::client::memory::MemoryStoreParams;
    use crate::test_support::take_calls;
    use crate::test_support::wrap_mcp_response;
    use crate::test_support::MockTransport;

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
            "schema_version": 2,
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
}
