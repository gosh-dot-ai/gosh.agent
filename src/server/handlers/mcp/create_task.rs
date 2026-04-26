// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use serde_json::json;
use serde_json::Value;

use crate::client::memory::IngestFactsParams;
use crate::client::memory::MemoryQueryParams;
use crate::client::memory::StoreParams;
use crate::server::AppState;

#[derive(Debug, Clone)]
pub(crate) struct CreateTaskRequest {
    agent_id: String,
    swarm_id: String,
    work_key: String,
    description: String,
    external_task_id: String,
    target_list: Vec<String>,
    scope: String,
    metadata: Value,
}

pub(crate) fn parse_request(
    args: &Value,
    default_agent_id: &str,
) -> Result<CreateTaskRequest, Value> {
    let agent_id =
        args.get("agent_id").and_then(|v| v.as_str()).unwrap_or(default_agent_id).to_string();
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default").to_string();
    let work_key = args
        .get("work_key")
        .or_else(|| args.get("key"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let context_key =
        args.get("context_key").and_then(|v| v.as_str()).map(|value| value.to_string());
    let description = match args.get("description").and_then(|v| v.as_str()) {
        Some(d) => d.to_string(),
        None => return Err(json!({"error": "description is required", "code": "MISSING_PARAM"})),
    };
    let external_task_id =
        args.get("task_id").and_then(|v| v.as_str()).unwrap_or("auto").to_string();

    let target_list = normalize_target_list(args.get("target"), &agent_id);
    let scope = match args.get("scope").and_then(|v| v.as_str()) {
        Some(scope) => scope.to_string(),
        None => return Err(json!({"error": "scope is required", "code": "MISSING_PARAM"})),
    };

    let mut metadata = match args.get("metadata") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({}),
    };
    metadata["task_id"] = json!(external_task_id);
    if let Some(workflow_id) = args.get("workflow_id").and_then(|v| v.as_str()) {
        metadata["workflow_id"] = json!(workflow_id);
    }
    if let Some(route) = args.get("route").and_then(|v| v.as_str()) {
        metadata["route"] = json!(route);
    }
    if let Some(priority) = args.get("priority") {
        metadata["priority"] = priority.clone();
    }
    metadata["work_key"] = json!(work_key);
    if let Some(context_key) = context_key.as_deref() {
        metadata["context_key"] = json!(context_key);
    }

    Ok(CreateTaskRequest {
        agent_id,
        swarm_id,
        work_key,
        description,
        external_task_id,
        target_list,
        scope,
        metadata,
    })
}

fn normalize_target_list(target: Option<&Value>, agent_id: &str) -> Vec<String> {
    match target {
        Some(Value::String(s)) => vec![normalize_target(s)],
        Some(Value::Array(arr)) => {
            let normalized: Vec<String> =
                arr.iter().filter_map(|v| v.as_str()).map(normalize_target).collect();
            if normalized.is_empty() {
                vec![format!("agent:{agent_id}")]
            } else {
                normalized
            }
        }
        _ => vec![format!("agent:{agent_id}")],
    }
}

fn normalize_target(target: &str) -> String {
    if target.starts_with("agent:") {
        target.to_string()
    } else {
        format!("agent:{target}")
    }
}

fn build_authoritative_task_fact(request: &CreateTaskRequest) -> Value {
    json!({
        "id": request.external_task_id,
        "kind": "task",
        "fact": request.description,
        "target": request.target_list,
        "metadata": request.metadata,
        "tags": ["task", format!("task:{}", request.external_task_id)],
        "scope": request.scope,
    })
}

async fn resolve_task_fact_id(
    state: &AppState,
    request: &CreateTaskRequest,
) -> Result<String, Value> {
    let mut filter = json!({
        "kind": "task",
        "metadata.task_id": request.external_task_id,
    });
    if request.target_list.len() == 1 {
        filter["target"] = json!(request.target_list[0]);
    }

    let result = state
        .agent
        .memory
        .memory_query(MemoryQueryParams {
            key: request.work_key.clone(),
            agent_id: request.agent_id.clone(),
            swarm_id: request.swarm_id.clone(),
            filter,
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(1),
        })
        .await
        .map_err(|e| json!({"error": e.to_string(), "code": "QUERY_ERROR"}))?;

    result
        .get("facts")
        .and_then(|v| v.as_array())
        .and_then(|facts| facts.first())
        .and_then(|fact| fact.get("id"))
        .and_then(|id| id.as_str())
        .map(|id| id.to_string())
        .ok_or_else(|| json!({"error": "authoritative task fact not found after ingest", "code": "QUERY_ERROR"}))
}

pub(crate) async fn execute_create_task(state: &AppState, request: &CreateTaskRequest) -> Value {
    let task_fact_result = state
        .agent
        .memory
        .ingest_asserted_facts(IngestFactsParams {
            key: request.work_key.clone(),
            agent_id: request.agent_id.clone(),
            swarm_id: request.swarm_id.clone(),
            scope: request.scope.clone(),
            facts: json!([build_authoritative_task_fact(request)]),
            enrich_l0: None,
        })
        .await;

    let task_fact_id = match task_fact_result {
        Ok(_) => match resolve_task_fact_id(state, request).await {
            Ok(task_fact_id) => task_fact_id,
            Err(err) => return err,
        },
        Err(e) => return json!({"error": e.to_string(), "code": "STORE_ERROR"}),
    };

    let memory = state.agent.memory.clone();
    let store_params = StoreParams {
        key: request.work_key.clone(),
        agent_id: request.agent_id.clone(),
        swarm_id: request.swarm_id.clone(),
        content: request.description.clone(),
        scope: request.scope.clone(),
        content_type: "default".to_string(),
        session_num: 1,
        session_date: chrono::Utc::now().date_naive().to_string(),
        speakers: "User".to_string(),
        metadata: Some(request.metadata.clone()),
        target: Some(request.target_list.clone()),
    };
    let task_id = request.external_task_id.clone();
    // The authoritative task fact above is the reliable create-task contract.
    // This semantic store is indexing-only best effort and intentionally
    // detached so slow extraction cannot hold the request open.
    tokio::spawn(async move {
        if let Err(e) = memory.store(store_params).await {
            tracing::warn!(task_id = %task_id, error = %e, "semantic task store failed (non-fatal)");
        }
    });

    // Fact extraction is handled by memory (via librarian_profile) when
    // the task description is stored asynchronously above. Agent does not
    // extract facts, and task creation is authoritative once the task fact is
    // accepted.

    json!({
        "task_id": request.external_task_id,
        "task_fact_id": task_fact_id,
        "target": request.target_list,
    })
}

pub async fn handle(state: &AppState, args: &Value) -> Value {
    match parse_request(args, &state.agent_id) {
        Ok(request) => execute_create_task(state, &request).await,
        Err(err) => err,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use serde_json::json;

    use super::handle;
    use super::normalize_target_list;
    use crate::test_support::take_calls;
    use crate::test_support::test_app_state;
    use crate::test_support::test_app_state_with_delays;
    use crate::test_support::wrap_mcp_response;

    #[test]
    fn normalize_target_list_defaults_to_request_agent() {
        let normalized = normalize_target_list(None, "planner");
        assert_eq!(normalized, vec!["agent:planner"]);
    }

    #[tokio::test]
    async fn create_task_handle_rejects_missing_description() {
        let (state, _) = test_app_state(vec![]);

        let result = handle(&state, &json!({"agent_id": "planner"})).await;

        assert_eq!(result.get("code").and_then(|v| v.as_str()), Some("MISSING_PARAM"));
    }

    #[tokio::test]
    async fn create_task_handle_writes_authoritative_fact_and_normalizes_target() {
        let responses = vec![
            wrap_mcp_response(&json!({"granular_added": 1})),
            wrap_mcp_response(&json!({"facts": [{"id": "fact-new-1"}]})),
            wrap_mcp_response(&json!({"facts_extracted": 0})),
        ];
        let (state, mock_state) = test_app_state(responses);

        let result = handle(
            &state,
            &json!({
                "agent_id": "planner",
                "swarm_id": "swarm-alpha",
                "key": "proj-a",
                "scope": "swarm-shared",
                "task_id": "ext-task-42",
                "description": "Implement feature X",
                "target": "worker-1",
                "metadata": {"route": "secondary"},
                "workflow_id": "wf-123",
                "priority": 5,
            }),
        )
        .await;

        assert_eq!(result.get("task_fact_id").and_then(|v| v.as_str()), Some("fact-new-1"));
        assert_eq!(
            result.get("target").and_then(|v| v.as_array()).unwrap(),
            &vec![json!("agent:worker-1")]
        );

        let mut calls = Vec::new();
        for _ in 0..20 {
            calls = mock_state.lock().calls.clone();
            if calls.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(calls.len(), 3);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "memory_ingest_asserted_facts");
        assert_eq!(args.get("scope").unwrap(), "swarm-shared");
        let fact = args
            .get("facts")
            .and_then(|v| v.as_array())
            .and_then(|facts| facts.first())
            .expect("authoritative fact");
        assert_eq!(fact.get("kind").unwrap(), "task");
        assert_eq!(fact.get("fact").unwrap(), "Implement feature X");
        assert_eq!(fact.get("target").unwrap(), &json!(["agent:worker-1"]));
        assert_eq!(fact.get("scope").unwrap(), "swarm-shared");
        let metadata = fact.get("metadata").unwrap();
        assert_eq!(metadata.get("task_id").unwrap(), "ext-task-42");
        assert_eq!(metadata.get("workflow_id").unwrap(), "wf-123");
        assert_eq!(metadata.get("route").unwrap(), "secondary");
        assert_eq!(metadata.get("priority").unwrap(), 5);
        assert_eq!(metadata.get("work_key").unwrap(), "proj-a");
        assert!(metadata.get("context_key").is_none());
        let (tool_name, args) = &calls[1];
        assert_eq!(tool_name, "memory_query");
        assert_eq!(args.get("filter").unwrap().get("metadata.task_id").unwrap(), "ext-task-42");
        let (tool_name, args) = &calls[2];
        assert_eq!(tool_name, "memory_store");
        assert_eq!(args.get("scope").unwrap(), "swarm-shared");
        assert_eq!(args.get("content").unwrap(), "Implement feature X");
        assert_eq!(args.get("target").unwrap(), &json!(["agent:worker-1"]));
        let metadata = args.get("metadata").unwrap();
        assert_eq!(metadata.get("task_id").unwrap(), "ext-task-42");
        assert_eq!(metadata.get("workflow_id").unwrap(), "wf-123");
        assert_eq!(metadata.get("route").unwrap(), "secondary");
        assert_eq!(metadata.get("priority").unwrap(), 5);
        assert_eq!(metadata.get("work_key").unwrap(), "proj-a");
    }

    #[tokio::test]
    async fn create_task_returns_before_slow_semantic_store() {
        let responses = vec![
            wrap_mcp_response(&json!({"granular_added": 1})),
            wrap_mcp_response(&json!({"facts": [{"id": "fact-new-1"}]})),
            wrap_mcp_response(&json!({"facts_extracted": 0})),
        ];
        let mut delays = HashMap::new();
        delays.insert("memory_store".to_string(), Duration::from_secs(5));
        let (state, mock_state) = test_app_state_with_delays(responses, delays);

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            handle(
                &state,
                &json!({
                    "agent_id": "planner",
                    "swarm_id": "swarm-alpha",
                    "key": "proj-a",
                    "scope": "swarm-shared",
                    "task_id": "ext-task-42",
                    "description": "Implement feature X",
                    "target": "worker-1",
                }),
            ),
        )
        .await
        .expect("create-task should not wait for semantic memory_store");

        assert_eq!(result.get("task_fact_id").and_then(|v| v.as_str()), Some("fact-new-1"));
        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "memory_ingest_asserted_facts");
        assert_eq!(calls[1].0, "memory_query");
    }
}
