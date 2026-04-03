// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use serde_json::json;
use serde_json::Value;

use crate::agent::config_loader;
use crate::server::AppState;

pub async fn handle(state: &AppState, args: &Value) -> Value {
    let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("default");
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default");
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("default");
    let task_ref = args.get("task_id").and_then(|v| v.as_str());
    let budget = args.get("budget_shell").and_then(|v| v.as_f64()).unwrap_or(10.0);

    let task_ref = match task_ref {
        Some(id) => id.to_string(),
        None => return json!({"error": "task_id is required", "code": "MISSING_PARAM"}),
    };

    if budget < 1.0 {
        return json!({"error": "budget_shell must be >= 1.0", "code": "INVALID_BUDGET"});
    }

    let effective_config = match config_loader::load_agent_config(
        &state.memory,
        &state.agent.config,
        key,
        agent_id,
        swarm_id,
    )
    .await
    {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({
                "task_id": task_ref,
                "status": "failure",
                "shell_spent": 0.0,
                "artifacts_written": [],
                "error": format!("CONFIG_LOAD_FAILED: {e}"),
            });
        }
    };

    if !effective_config.enabled {
        return json!({
            "task_id": task_ref,
            "status": "failure",
            "shell_spent": 0.0,
            "artifacts_written": [],
            "error": "AGENT_DISABLED",
        });
    }

    // Resolve task reference to authoritative task fact (fail fast on error)
    let resolved = match crate::agent::resolve::resolve_task(
        &state.memory,
        &task_ref,
        agent_id,
        key,
        swarm_id,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "task_id": task_ref,
                "status": "failure",
                "shell_spent": 0.0,
                "artifacts_written": [],
                "error": e.to_string(),
            });
        }
    };

    {
        let mut in_flight = state.in_flight_tasks.lock().await;
        if in_flight.contains(&resolved.task_fact_id) {
            return json!({
                "task_id": task_ref,
                "task_fact_id": resolved.task_fact_id,
                "status": "already_running",
                "shell_spent": 0.0,
                "artifacts_written": [],
            });
        }
        in_flight.insert(resolved.task_fact_id.clone());
    }
    {
        let mut counts = state.in_flight_by_agent.lock().await;
        let current = counts.get(agent_id).copied().unwrap_or(0);
        if current >= effective_config.max_parallel_tasks {
            let mut in_flight = state.in_flight_tasks.lock().await;
            in_flight.remove(&resolved.task_fact_id);
            return json!({
                "task_id": task_ref,
                "task_fact_id": resolved.task_fact_id,
                "status": "busy",
                "shell_spent": 0.0,
                "artifacts_written": [],
                "error": "AGENT_CONCURRENCY_LIMIT",
            });
        }
        counts.insert(agent_id.to_string(), current + 1);
    }

    let agent = state.agent.with_config(effective_config);

    // Execute using the resolved task_fact_id (blocking) — results persisted by
    // agent
    let run_result = agent.run(agent_id, swarm_id, &resolved.task_fact_id, key, budget).await;

    {
        let mut in_flight = state.in_flight_tasks.lock().await;
        in_flight.remove(&resolved.task_fact_id);
    }
    {
        let mut counts = state.in_flight_by_agent.lock().await;
        if let Some(current) = counts.get_mut(agent_id) {
            *current = current.saturating_sub(1);
            if *current == 0 {
                counts.remove(agent_id);
            }
        }
    }

    match run_result {
        Ok(task_result) => {
            serde_json::to_value(&task_result).unwrap_or(json!({"error": "serialization failed"}))
        }
        Err(e) => {
            json!({
                "task_id": task_ref,
                "status": "failure",
                "shell_spent": 0.0,
                "artifacts_written": [],
                "error": e.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::handle;
    use crate::test_support::test_app_state;
    use crate::test_support::wrap_mcp_response;

    fn empty_agent_config_query() -> serde_json::Value {
        wrap_mcp_response(&json!({"facts": []}))
    }

    fn task_get_response(task_fact: serde_json::Value) -> serde_json::Value {
        wrap_mcp_response(&json!({ "fact": task_fact }))
    }

    #[tokio::test]
    async fn start_handle_rejects_target_mismatch() {
        let responses = vec![
            empty_agent_config_query(),
            empty_agent_config_query(),
            task_get_response(json!({
                "id": "fact-123",
                "kind": "task",
                "fact": "Implement feature X",
                "target": ["agent:other-agent"],
                "metadata": {"task_id": "ext-task-1"}
            })),
        ];
        let (state, _) = test_app_state(responses);

        let result = handle(
            &state,
            &json!({
                "agent_id": "planner",
                "swarm_id": "swarm-alpha",
                "key": "proj-a",
                "task_id": "fact-123",
                "budget_shell": 10.0,
            }),
        )
        .await;

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failure"));
        assert!(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("TARGET_MISMATCH"));
    }

    #[tokio::test]
    async fn start_handle_rejects_duplicate_in_flight_task() {
        let responses = vec![
            empty_agent_config_query(),
            empty_agent_config_query(),
            task_get_response(json!({
                "id": "fact-dup",
                "kind": "task",
                "fact": "Implement feature Y",
                "target": ["agent:planner"],
                "metadata": {"task_id": "ext-task-2"}
            })),
        ];
        let (state, _) = test_app_state(responses);
        state.in_flight_tasks.lock().await.insert("fact-dup".to_string());

        let result = handle(
            &state,
            &json!({
                "agent_id": "planner",
                "swarm_id": "swarm-alpha",
                "key": "proj-a",
                "task_id": "fact-dup",
                "budget_shell": 10.0,
            }),
        )
        .await;

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("already_running"));
        assert_eq!(result.get("task_fact_id").and_then(|v| v.as_str()), Some("fact-dup"));
    }

    #[tokio::test]
    async fn start_handle_returns_failure_when_task_is_missing() {
        let responses = vec![
            empty_agent_config_query(),
            empty_agent_config_query(),
            wrap_mcp_response(&json!(null)),
            wrap_mcp_response(&json!({"facts": []})),
        ];
        let (state, _) = test_app_state(responses);

        let result = handle(
            &state,
            &json!({
                "agent_id": "planner",
                "swarm_id": "swarm-alpha",
                "key": "proj-a",
                "task_id": "missing-task",
                "budget_shell": 10.0,
            }),
        )
        .await;

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failure"));
        assert!(result.get("error").and_then(|v| v.as_str()).unwrap_or("").contains("NOT_FOUND"));
    }
}
