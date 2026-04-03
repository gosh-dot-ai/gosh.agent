// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;

use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use crate::client::memory::MemoryGetParams;
use crate::client::memory::MemoryMcpClient;
use crate::client::memory::MemoryQueryParams;

fn unwrap_memory_get_fact(value: &Value) -> &Value {
    value.get("fact").unwrap_or(value)
}

/// Resolution outcome.
#[derive(Debug, Clone)]
pub struct ResolvedTask {
    /// The persisted top-level memory fact id (task_fact_id).
    pub task_fact_id: String,
    /// The external user-facing task id (metadata.task_id), if present.
    pub external_task_id: Option<String>,
    /// The task fact text.
    pub fact: String,
    /// The full fact value for further inspection.
    pub raw: Value,
}

/// Resolve a task reference to an authoritative task fact.
///
/// Algorithm:
/// 1. Try `memory_get(fact_id=<ref>)` -- if found and `kind == "task"`, verify
///    target contains `agent:<agent_id>` for agent-scoped flows, reject on
///    mismatch.
/// 2. Else query by external task_id:
///    `filter={"kind":"task","target":"agent:<agent_id>","metadata.task_id":"
///    <ref>"}`, `sort_by="created_at"`, `sort_order="desc"`, `limit=2`
///    - 0 results = NOT_FOUND
///    - 2 results = AMBIGUOUS
///    - 1 result  = success
pub async fn resolve_task(
    memory: &Arc<MemoryMcpClient>,
    task_ref: &str,
    agent_id: &str,
    key: &str,
    swarm_id: &str,
) -> Result<ResolvedTask> {
    let agent_target = format!("agent:{agent_id}");

    // Step 1: Try direct fact_id lookup
    let get_result = memory
        .memory_get(MemoryGetParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            fact_id: task_ref.to_string(),
        })
        .await;

    if let Ok(fact) = get_result {
        let fact = unwrap_memory_get_fact(&fact);
        if !fact.is_null() && fact.get("kind").and_then(|v| v.as_str()) == Some("task") {
            // Verify target contains agent:<agent_id>
            if !target_contains(fact, &agent_target) {
                bail!(
                    "TARGET_MISMATCH: task {} exists but is not targeted at {}",
                    task_ref,
                    agent_target
                );
            }

            let fact_text = fact.get("fact").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let external_task_id = fact
                .get("metadata")
                .and_then(|m| m.get("task_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let fact_id = fact.get("id").and_then(|v| v.as_str()).unwrap_or(task_ref).to_string();

            return Ok(ResolvedTask {
                task_fact_id: fact_id,
                external_task_id,
                fact: fact_text,
                raw: fact.clone(),
            });
        }
    }

    // Step 2: Query by external task_id + target
    let query_result = memory
        .memory_query(MemoryQueryParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            filter: json!({
                "kind": "task",
                "target": agent_target,
                "metadata.task_id": task_ref,
            }),
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(2),
        })
        .await?;

    let facts = query_result.get("facts").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    match facts.len() {
        0 => bail!("NOT_FOUND: no task found for reference '{}'", task_ref),
        1 => {
            let fact = &facts[0];
            let fact_id = fact.get("id").and_then(|v| v.as_str()).unwrap_or(task_ref).to_string();
            let fact_text = fact.get("fact").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let external_task_id = fact
                .get("metadata")
                .and_then(|m| m.get("task_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            Ok(ResolvedTask {
                task_fact_id: fact_id,
                external_task_id,
                fact: fact_text,
                raw: fact.clone(),
            })
        }
        _ => bail!(
            "AMBIGUOUS: multiple tasks found for reference '{}' targeted at {}",
            task_ref,
            agent_target
        ),
    }
}

/// Check if a fact's `target` list contains a given target string.
fn target_contains(fact: &Value, target: &str) -> bool {
    match fact.get("target") {
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(target)),
        Some(Value::String(s)) => s == target,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::McpTransport;

    /// Mock transport that records calls and returns pre-configured responses.
    struct MockTransport {
        responses: Mutex<Vec<Value>>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Value>) -> Self {
            Self { responses: Mutex::new(responses), calls: Mutex::new(Vec::new()) }
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
            self.calls.lock().unwrap().push((tool_name, args));

            let resp = {
                let mut resps = self.responses.lock().unwrap();
                if resps.is_empty() {
                    json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"null"}]}})
                } else {
                    resps.remove(0)
                }
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

    #[tokio::test]
    async fn resolver_exact_fact_id_works() {
        let task_fact = json!({
            "id": "fact-abc-123",
            "kind": "task",
            "fact": "Do the thing",
            "target": ["agent:worker-1"],
            "metadata": {"task_id": "ext-001"}
        });

        let transport = MockTransport::new(vec![wrap_mcp_response(&json!({ "fact": task_fact }))]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "fact-abc-123", "worker-1", "default", "default").await;

        let resolved = result.unwrap();
        assert_eq!(resolved.task_fact_id, "fact-abc-123");
        assert_eq!(resolved.external_task_id.as_deref(), Some("ext-001"));
        assert_eq!(resolved.fact, "Do the thing");
    }

    #[tokio::test]
    async fn resolver_exact_fact_id_unwraps_memory_get_payload() {
        let task_fact = json!({
            "id": "fact-wrap-1",
            "kind": "task",
            "fact": "Wrapped task",
            "target": ["agent:worker-1"],
            "metadata": {"task_id": "wrapped-001"}
        });

        let transport = MockTransport::new(vec![wrap_mcp_response(&json!({ "fact": task_fact }))]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "fact-wrap-1", "worker-1", "default", "default").await;

        let resolved = result.unwrap();
        assert_eq!(resolved.task_fact_id, "fact-wrap-1");
        assert_eq!(resolved.external_task_id.as_deref(), Some("wrapped-001"));
        assert_eq!(resolved.fact, "Wrapped task");
    }

    #[tokio::test]
    async fn resolver_external_task_id_resolves() {
        // memory_get returns null (not found by fact_id)
        let get_resp = wrap_mcp_response(&json!(null));
        // memory_query returns one matching fact
        let query_resp = wrap_mcp_response(&json!({
            "facts": [{
                "id": "fact-xyz",
                "kind": "task",
                "fact": "Do it",
                "target": ["agent:worker-1"],
                "metadata": {"task_id": "my-task"}
            }]
        }));

        let transport = MockTransport::new(vec![get_resp, query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "my-task", "worker-1", "default", "default").await;

        let resolved = result.unwrap();
        assert_eq!(resolved.task_fact_id, "fact-xyz");
        assert_eq!(resolved.external_task_id.as_deref(), Some("my-task"));
    }

    #[tokio::test]
    async fn resolver_wrong_target_rejects() {
        let task_fact = json!({
            "id": "fact-abc",
            "kind": "task",
            "fact": "Do the thing",
            "target": ["agent:other-agent"],
            "metadata": {"task_id": "ext-001"}
        });

        let transport = MockTransport::new(vec![wrap_mcp_response(&json!({ "fact": task_fact }))]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "fact-abc", "worker-1", "default", "default").await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("TARGET_MISMATCH"), "expected TARGET_MISMATCH, got: {err}");
    }

    #[tokio::test]
    async fn resolver_ambiguous_errors() {
        let get_resp = wrap_mcp_response(&json!(null));
        let query_resp = wrap_mcp_response(&json!({
            "facts": [
                {"id": "fact-1", "kind": "task", "fact": "A", "target": ["agent:w"], "metadata": {"task_id": "dup"}},
                {"id": "fact-2", "kind": "task", "fact": "B", "target": ["agent:w"], "metadata": {"task_id": "dup"}}
            ]
        }));

        let transport = MockTransport::new(vec![get_resp, query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "dup", "w", "default", "default").await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("AMBIGUOUS"), "expected AMBIGUOUS, got: {err}");
    }

    #[tokio::test]
    async fn resolver_missing_errors() {
        let get_resp = wrap_mcp_response(&json!(null));
        let query_resp = wrap_mcp_response(&json!({"facts": []}));

        let transport = MockTransport::new(vec![get_resp, query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = resolve_task(&memory, "nonexistent", "w", "default", "default").await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("NOT_FOUND"), "expected NOT_FOUND, got: {err}");
    }
}
