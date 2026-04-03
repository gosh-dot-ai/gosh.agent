// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use serde_json::json;
use serde_json::Value;

use crate::client::memory::MemoryQueryParams;
use crate::server::AppState;

pub async fn handle(state: &AppState, args: &Value) -> Value {
    let task_ref = match args.get("task_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return json!({"error": "task_id is required", "code": "MISSING_PARAM"}),
    };
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("default");
    let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("default");
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default");

    // Resolve task reference to authoritative task fact (fail fast)
    let resolved =
        match crate::agent::resolve::resolve_task(&state.memory, task_ref, agent_id, key, swarm_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return json!({
                    "error": format!("task {} not found: {}", task_ref, e),
                    "code": "TASK_NOT_FOUND",
                });
            }
        };

    let task_fact_id = &resolved.task_fact_id;

    // Canonical: latest result via metadata.task_fact_id
    let result_fact =
        query_latest_fact(state, key, agent_id, swarm_id, "task_result", task_fact_id).await;

    // Canonical: latest session via metadata.task_fact_id
    let session_fact =
        query_latest_fact(state, key, agent_id, swarm_id, "task_session", task_fact_id).await;

    // Legacy fallback if canonical returned nothing
    let result_fact = match result_fact {
        Some(fact) => Some(fact),
        None => {
            let ext_id = resolved.external_task_id.as_deref().unwrap_or(task_ref);
            query_legacy_fact(state, key, agent_id, swarm_id, &format!("result_{ext_id}"))
                .await
                .map(|text| json!({"kind": "task_result", "fact": text}))
        }
    };

    let session_fact = match session_fact {
        Some(fact) => Some(fact),
        None => {
            let ext_id = resolved.external_task_id.as_deref().unwrap_or(task_ref);
            query_legacy_fact(state, key, agent_id, swarm_id, &format!("session_{ext_id}"))
                .await
                .map(|text| json!({"kind": "task_session", "fact": text}))
        }
    };

    if result_fact.is_none() && session_fact.is_none() {
        return json!({
            "task_id": task_ref,
            "task_fact_id": task_fact_id,
            "status": "pending",
            "telemetry_version": 1,
        });
    }

    build_status_response(task_ref, task_fact_id, result_fact.as_ref(), session_fact.as_ref())
}

/// Query latest artifact by kind + metadata.task_fact_id (canonical).
async fn query_latest_fact(
    state: &AppState,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    kind: &str,
    task_fact_id: &str,
) -> Option<Value> {
    let result = state
        .memory
        .memory_query(MemoryQueryParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            filter: json!({
                "kind": kind,
                "metadata.task_fact_id": task_fact_id,
            }),
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(1),
        })
        .await
        .ok()?;

    result.get("facts").and_then(|v| v.as_array()).and_then(|arr| arr.first()).cloned()
}

/// Legacy fallback: find fact by id suffix match.
async fn query_legacy_fact(
    state: &AppState,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    legacy_id: &str,
) -> Option<String> {
    // Try memory_get directly with the legacy id
    let result = state
        .memory
        .memory_get(crate::client::memory::MemoryGetParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            fact_id: legacy_id.to_string(),
        })
        .await
        .ok()?;

    if result.is_null() {
        return None;
    }

    result.get("fact").and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn fact_text(fact: Option<&Value>) -> Option<String> {
    fact.and_then(|f| f.get("fact").or_else(|| f.get("text")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn fact_metadata(fact: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    fact.and_then(|f| f.get("metadata")).and_then(|v| v.as_object())
}

fn normalize_status(status: &str) -> Option<&'static str> {
    match status {
        "done" => Some("done"),
        "failed" | "failure" => Some("failed"),
        "pending" => Some("pending"),
        "running" | "active" => Some("active"),
        "partial_budget_overdraw" => Some("partial_budget_overdraw"),
        "too_complex" => Some("too_complex"),
        _ => None,
    }
}

fn derive_effective_status(
    base_status: &str,
    latest_result: Option<&serde_json::Value>,
    latest_session: Option<&serde_json::Value>,
) -> String {
    let session_status = fact_metadata(latest_session)
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .and_then(normalize_status);
    let result_status = fact_metadata(latest_result)
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .and_then(normalize_status);

    if let Some(status) = result_status {
        return status.to_string();
    }

    if let Some(status) = session_status {
        return status.to_string();
    }

    if let Some(text) = latest_session
        .and_then(|sf| sf.get("fact").or_else(|| sf.get("text")))
        .and_then(|v| v.as_str())
    {
        if text.contains("status failed") {
            return "failed".to_string();
        }
        if text.contains("status done") {
            return "done".to_string();
        }
    }

    base_status.to_string()
}

fn build_status_response(
    task_ref: &str,
    task_fact_id: &str,
    latest_result: Option<&Value>,
    latest_session: Option<&Value>,
) -> Value {
    let base_status = fact_metadata(latest_session)
        .and_then(|m| m.get("status"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            fact_metadata(latest_result).and_then(|m| m.get("status")).and_then(|v| v.as_str())
        })
        .unwrap_or("active");
    let runtime_meta = fact_metadata(latest_session).or_else(|| fact_metadata(latest_result));
    let effective_status = derive_effective_status(base_status, latest_result, latest_session);

    json!({
        "telemetry_version": 1,
        "task_id": task_ref,
        "task_fact_id": task_fact_id,
        "status": effective_status,
        "session": fact_text(latest_session),
        "result": fact_text(latest_result),
        "session_fact": latest_session.cloned(),
        "result_fact": latest_result.cloned(),
        "phase": runtime_meta.and_then(|m| m.get("phase")).and_then(|v| v.as_str()),
        "iteration": runtime_meta.and_then(|m| m.get("iteration")).and_then(|v| v.as_u64()),
        "shell_spent": runtime_meta.and_then(|m| m.get("shell_spent")).and_then(|v| v.as_f64()),
        "profile_used": runtime_meta.and_then(|m| m.get("profile_used")).and_then(|v| v.as_str()),
        "backend_used": runtime_meta.and_then(|m| m.get("backend_used")).and_then(|v| v.as_str()),
        "started_at": runtime_meta.and_then(|m| m.get("started_at")).and_then(|v| v.as_str()),
        "finished_at": runtime_meta.and_then(|m| m.get("finished_at")).and_then(|v| v.as_str()),
        "error": runtime_meta.and_then(|m| m.get("error")).and_then(|v| v.as_str()),
        "tool_trace": runtime_meta.and_then(|m| m.get("tool_trace")).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::build_status_response;
    use super::derive_effective_status;
    use super::normalize_status;

    #[test]
    fn effective_status_prefers_latest_result() {
        let status = derive_effective_status(
            "active",
            Some(&json!({"kind":"task_result","fact":"ok","metadata":{"status":"done"}})),
            None,
        );
        assert_eq!(status, "done");
    }

    #[test]
    fn effective_status_maps_failure_metadata_to_failed() {
        let status = derive_effective_status(
            "active",
            None,
            Some(&json!({
                "kind":"task_session",
                "fact":"Agent planner completed task abc with status failure.",
                "metadata":{"status":"failure"}
            })),
        );
        assert_eq!(status, "failed");
    }

    #[test]
    fn effective_status_uses_latest_session_when_result_absent() {
        let status = derive_effective_status(
            "active",
            None,
            Some(&json!({"fact":"Agent planner completed task abc with status failed."})),
        );
        assert_eq!(status, "failed");
    }

    #[test]
    fn effective_status_falls_back_to_base_status() {
        let status = derive_effective_status("active", None, None);
        assert_eq!(status, "active");
    }

    #[test]
    fn normalize_status_handles_failure_aliases() {
        assert_eq!(normalize_status("failure"), Some("failed"));
        assert_eq!(normalize_status("failed"), Some("failed"));
        assert_eq!(normalize_status("running"), Some("active"));
    }

    #[test]
    fn build_status_response_exposes_structured_runtime_fields() {
        let result = build_status_response(
            "task-1",
            "fact-1",
            Some(&json!({
                "fact": "done",
                "metadata": {
                    "status": "done",
                    "profile_used": "qwen",
                    "backend_used": "groq",
                    "shell_spent": 1.5,
                    "error": "MODEL_TIMEOUT",
                    "tool_trace": ["memory_recall:ok"],
                }
            })),
            Some(&json!({
                "fact": "session text",
                "metadata": {
                    "status": "done",
                    "phase": "review",
                    "iteration": 3,
                    "profile_used": "qwen",
                    "backend_used": "groq",
                    "shell_spent": 1.5,
                    "started_at": "2026-03-28T00:00:00Z",
                    "finished_at": "2026-03-28T00:01:00Z",
                    "error": "MODEL_TIMEOUT",
                    "tool_trace": ["memory_recall:ok"],
                }
            })),
        );

        assert_eq!(result.get("telemetry_version").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("done"));
        assert_eq!(result.get("phase").and_then(|v| v.as_str()), Some("review"));
        assert_eq!(result.get("iteration").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(result.get("profile_used").and_then(|v| v.as_str()), Some("qwen"));
        assert_eq!(result.get("backend_used").and_then(|v| v.as_str()), Some("groq"));
        assert_eq!(result.get("error").and_then(|v| v.as_str()), Some("MODEL_TIMEOUT"));
        assert_eq!(result.get("tool_trace").and_then(|v| v.as_array()).map(|v| v.len()), Some(1));
        assert!(result.get("session_fact").is_some());
        assert!(result.get("result_fact").is_some());
    }
}
