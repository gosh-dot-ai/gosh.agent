// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use serde_json::json;
use serde_json::Value;

use crate::agent::run::AgentRunRequest;
use crate::agent::task::TaskProgressEvent;
use crate::agent::task::TaskProgressReporter;
use crate::server::AppState;

pub async fn handle(state: &AppState, args: &Value) -> Value {
    handle_with_session(state, args, None).await
}

pub async fn handle_with_session(
    state: &AppState,
    args: &Value,
    mcp_session_id: Option<&str>,
) -> Value {
    let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or(&state.agent_id);
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default");
    let work_key = args.get("key").and_then(|v| v.as_str()).unwrap_or("default");
    let context_key = args
        .get("context_key")
        .and_then(|v| v.as_str())
        .or(state.default_context_key.as_deref())
        .unwrap_or(work_key);
    let task_ref = args.get("task_id").and_then(|v| v.as_str());
    let budget = args.get("budget_shell").and_then(|v| v.as_f64()).unwrap_or(10.0);

    let task_ref = match task_ref {
        Some(id) => id.to_string(),
        None => return json!({"error": "task_id is required", "code": "MISSING_PARAM"}),
    };

    if budget < 1.0 {
        return json!({"error": "budget_shell must be >= 1.0", "code": "INVALID_BUDGET"});
    }

    if !state.agent.config.enabled {
        return json!({
            "task_id": task_ref,
            "status": "failed",
            "shell_spent": 0.0,
            "artifacts_written": [],
            "error": "AGENT_DISABLED",
        });
    }

    // Resolve task reference to authoritative task fact (fail fast on error).
    // Watch/courier dispatch ignores this handler's return value, so resolve
    // timeouts must persist a terminal failed task result before returning.
    let resolved = match tokio::time::timeout(
        state.agent.config.bootstrap_memory_timeout,
        crate::agent::resolve::resolve_task(&state.memory, &task_ref, agent_id, work_key, swarm_id),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return json!({
                "task_id": task_ref,
                "status": "failed",
                "shell_spent": 0.0,
                "artifacts_written": [],
                "error": e.to_string(),
            });
        }
        Err(_) => {
            let task_result = state
                .agent
                .finish_bootstrap_resolve_timeout(
                    agent_id,
                    swarm_id,
                    &task_ref,
                    work_key,
                    context_key,
                    budget,
                )
                .await;
            return serde_json::to_value(&task_result)
                .unwrap_or(json!({"error": "serialization failed"}));
        }
    };
    if let Some(session_id) = mcp_session_id {
        state.mcp_events.bind_task_session(&resolved.task_fact_id, session_id).await;
    }

    let already_running = {
        let mut in_flight = state.in_flight_tasks.lock().await;
        if in_flight.contains(&resolved.task_fact_id) {
            true
        } else {
            in_flight.insert(resolved.task_fact_id.clone());
            false
        }
    };
    if already_running {
        emit_session_terminal(state, &resolved, "already_running", "task is already running").await;
        return json!({
            "task_id": task_ref,
            "task_fact_id": resolved.task_fact_id,
            "status": "already_running",
            "shell_spent": 0.0,
            "artifacts_written": [],
        });
    }

    let busy = {
        let mut counts = state.in_flight_by_agent.lock().await;
        let current = counts.get(agent_id).copied().unwrap_or(0);
        if current >= state.agent.config.max_parallel_tasks {
            true
        } else {
            counts.insert(agent_id.to_string(), current + 1);
            false
        }
    };
    if busy {
        let mut in_flight = state.in_flight_tasks.lock().await;
        in_flight.remove(&resolved.task_fact_id);
        drop(in_flight);
        emit_session_terminal(state, &resolved, "busy", "agent concurrency limit reached").await;
        return json!({
            "task_id": task_ref,
            "task_fact_id": resolved.task_fact_id,
            "status": "busy",
            "shell_spent": 0.0,
            "artifacts_written": [],
            "error": "AGENT_CONCURRENCY_LIMIT",
        });
    }

    let (reporter, mut progress_rx) = TaskProgressReporter::channel();
    let mcp_events = state.mcp_events.clone();
    tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            mcp_events.emit_task_progress(event).await;
        }
    });

    let run_result = state
        .agent
        .run_with_progress(AgentRunRequest {
            agent_id,
            swarm_id,
            task_id: &resolved.task_fact_id,
            work_key,
            default_context_key: context_key,
            budget_shell: budget,
            progress: Some(reporter),
        })
        .await;

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
        Err(e) => finish_run_error_after_binding(state, &resolved, &task_ref, e).await,
    }
}

async fn finish_run_error_after_binding(
    state: &AppState,
    resolved: &crate::agent::resolve::ResolvedTask,
    task_ref: &str,
    error: anyhow::Error,
) -> Value {
    let error = error.to_string();
    emit_session_terminal(
        state,
        resolved,
        "failed",
        &format!("task failed before terminal result: {error}"),
    )
    .await;
    json!({
        "task_id": task_ref,
        "task_fact_id": resolved.task_fact_id,
        "status": "failed",
        "shell_spent": 0.0,
        "artifacts_written": [],
        "error": error,
    })
}

async fn emit_session_terminal(
    state: &AppState,
    resolved: &crate::agent::resolve::ResolvedTask,
    stage: &str,
    message: &str,
) {
    state
        .mcp_events
        .emit_task_progress(
            TaskProgressEvent::new(
                resolved.external_task_id.as_deref().unwrap_or(&resolved.task_fact_id),
                Some(&resolved.task_fact_id),
                resolved.external_task_id.as_deref(),
                stage,
                9,
                9,
                message,
            )
            .terminal(),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use anyhow::anyhow;
    use serde_json::json;

    use super::finish_run_error_after_binding;
    use super::handle;
    use crate::agent::config::AgentConfig;
    use crate::agent::resolve::ResolvedTask;
    use crate::test_support::test_app_state;
    use crate::test_support::test_app_state_with_config_and_delays;
    use crate::test_support::wrap_mcp_response;

    fn task_get_response(task_fact: serde_json::Value) -> serde_json::Value {
        wrap_mcp_response(&json!({ "fact": task_fact }))
    }

    fn stored_response() -> serde_json::Value {
        wrap_mcp_response(&json!({"stored": true}))
    }

    fn visible_fact_response(
        kind: &str,
        id: &str,
        task_fact_id: &str,
        status: Option<&str>,
    ) -> serde_json::Value {
        wrap_mcp_response(&json!({
            "facts": [{
                "id": id,
                "kind": kind,
                "metadata": {
                    "task_fact_id": task_fact_id,
                    "status": status,
                }
            }]
        }))
    }

    #[tokio::test]
    async fn start_handle_rejects_target_mismatch() {
        let responses = vec![task_get_response(json!({
            "id": "fact-123",
            "kind": "task",
            "fact": "Implement feature X",
            "target": ["agent:other-agent"],
            "metadata": {"task_id": "ext-task-1"}
        }))];
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

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failed"));
        assert!(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("TARGET_MISMATCH"));
    }

    #[tokio::test]
    async fn start_handle_rejects_duplicate_in_flight_task() {
        let responses = vec![task_get_response(json!({
            "id": "fact-dup",
            "kind": "task",
            "fact": "Implement feature Y",
            "target": ["agent:planner"],
            "metadata": {"task_id": "ext-task-2"}
        }))];
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
    }

    #[tokio::test]
    async fn start_handle_returns_failure_when_task_is_missing() {
        let responses =
            vec![wrap_mcp_response(&json!(null)), wrap_mcp_response(&json!({"facts": []}))];
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

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failed"));
        assert!(result.get("error").and_then(|v| v.as_str()).unwrap_or("").contains("NOT_FOUND"));
    }

    #[tokio::test]
    async fn start_handle_persists_terminal_failure_when_prerun_resolve_times_out() {
        let responses = vec![
            stored_response(),
            visible_fact_response(
                "task_result",
                "task_result_fact-timeout",
                "fact-timeout",
                Some("failed"),
            ),
            stored_response(),
            visible_fact_response(
                "task_session",
                "task_session_fact-timeout",
                "fact-timeout",
                Some("failed"),
            ),
        ];
        let mut delays = HashMap::new();
        delays.insert("memory_get".to_string(), Duration::from_secs(5));
        let failure_dir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            bootstrap_memory_timeout: Duration::from_millis(50),
            local_failure_artifact_dir: Some(failure_dir.path().to_path_buf()),
            ..Default::default()
        };
        let (state, mock_state) = test_app_state_with_config_and_delays(config, responses, delays);

        let result = handle(
            &state,
            &json!({
                "agent_id": "planner",
                "swarm_id": "swarm-alpha",
                "key": "proj-a",
                "context_key": "ctx-a",
                "task_id": "fact-timeout",
                "budget_shell": 10.0,
            }),
        )
        .await;

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failed"));
        assert!(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("BOOTSTRAP_RESOLVE_TIMEOUT"));
        assert_eq!(
            result.get("artifacts_written").and_then(|v| v.as_array()).map(|v| v.len()),
            Some(3)
        );
        let artifact_path = failure_dir.path().join("task_failure_fact-timeout.json");
        let artifact: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();
        assert_eq!(
            artifact.get("source").and_then(|v| v.as_str()),
            Some("local_task_failure_fallback")
        );
        assert_eq!(artifact.get("schema_version").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(artifact.get("task_id").and_then(|v| v.as_str()), Some("fact-timeout"));
        assert_eq!(artifact.get("phase").and_then(|v| v.as_str()), Some("bootstrap_resolve"));
        assert_eq!(artifact.get("memory_persisted").and_then(|v| v.as_bool()), Some(true));

        let calls = mock_state.lock().calls.clone();
        let result_call = calls
            .iter()
            .find(|(name, args)| {
                name == "memory_ingest_asserted_facts"
                    && args
                        .get("facts")
                        .and_then(|v| v.as_array())
                        .and_then(|facts| facts.first())
                        .and_then(|fact| fact.get("kind"))
                        .and_then(|v| v.as_str())
                        == Some("task_result")
            })
            .expect("start resolve timeout should persist canonical task_result");
        let metadata = result_call
            .1
            .get("facts")
            .and_then(|v| v.as_array())
            .and_then(|facts| facts.first())
            .and_then(|fact| fact.get("metadata"))
            .unwrap();
        assert_eq!(metadata.get("task_fact_id").and_then(|v| v.as_str()), Some("fact-timeout"));
        assert_eq!(metadata.get("task_id").and_then(|v| v.as_str()), Some("fact-timeout"));
        assert_eq!(metadata.get("status").and_then(|v| v.as_str()), Some("failed"));
        assert_eq!(metadata.get("phase").and_then(|v| v.as_str()), Some("bootstrap_resolve"));
        assert_eq!(metadata.get("complete").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn run_error_after_binding_emits_terminal_progress_and_unbinds_session() {
        let (state, _) = test_app_state(vec![]);
        let mut rx = state.mcp_events.subscribe("session_1").await;
        let resolved = ResolvedTask {
            task_fact_id: "fact-run-error".to_string(),
            external_task_id: Some("external-run-error".to_string()),
            fact: "trigger post-bind error".to_string(),
            raw: json!({"id": "fact-run-error", "kind": "task"}),
        };
        state.mcp_events.bind_task_session(&resolved.task_fact_id, "session_1").await;

        let result = finish_run_error_after_binding(
            &state,
            &resolved,
            "external-run-error",
            anyhow!("boom"),
        )
        .await;

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("failed"));
        assert_eq!(result.get("task_fact_id").and_then(|v| v.as_str()), Some("fact-run-error"));
        let event = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("terminal progress timeout")
            .expect("terminal progress event");
        assert_eq!(event.data["method"], "notifications/progress");
        assert_eq!(event.data["params"]["progressToken"], "fact-run-error");
        assert_eq!(event.data["params"]["_meta"]["stage"], "failed");
        assert_eq!(event.data["params"]["_meta"]["terminal"], true);
        assert!(event.data["params"]["message"].as_str().unwrap_or("").contains("boom"));
        assert!(state.mcp_events.bound_sessions_for_task("fact-run-error").await.is_empty());
    }
}
