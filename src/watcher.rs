// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;

use crate::client::memory::MemoryMcpClient;
use crate::client::memory::MemoryQueryParams;
use crate::server::AppState;

const COURIER_RETRY_DELAY: Duration = Duration::from_secs(10);
// Courier retries forever by design. To add a limit, define a finite
// constant and uncomment the guard in the courier loop below.

/// Configuration for watch mode.
pub struct WatchConfig {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub poll_interval: Duration,
    pub budget_shell: f64,
}

/// Start watch mode: courier subscription with auto-reconnect + poll fallback.
/// Blocks until cancel signal is received.
pub async fn run(
    config: WatchConfig,
    app_state: Arc<AppState>,
    memory: Arc<MemoryMcpClient>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    // Run courier subscriber and poll loop concurrently
    tokio::select! {
        _ = cancel_rx.changed() => {
            info!("watcher cancelled");
        }
        _ = courier_loop(&config, app_state.clone(), &memory) => {
            warn!("courier loop exited unexpectedly");
        }
        _ = poll_loop(&config, app_state.clone(), &memory) => {
            warn!("poll loop exited unexpectedly");
        }
    }
}

/// Maintain courier subscription. Re-subscribes on failure with backoff.
async fn courier_loop(
    config: &WatchConfig,
    app_state: Arc<AppState>,
    memory: &Arc<MemoryMcpClient>,
) {
    let mut retries = 0u32;

    loop {
        info!(
            key = %config.key,
            agent_id = %config.agent_id,
            "subscribing to courier"
        );

        let result = {
            let mut courier = app_state.courier.lock().await;
            courier
                .subscribe(
                    memory,
                    &config.key,
                    &config.agent_id,
                    &config.swarm_id,
                    app_state.clone(),
                )
                .await
        };

        match result {
            Ok(sub_id) => {
                info!(sub_id = %sub_id, "courier subscription active");
                retries = 0;

                // Wait until subscription drops (courier listener will log the error).
                // We detect this by polling is_subscribed — when the SSE stream
                // reconnect loop inside courier.rs gives up or is cancelled, the
                // subscription is still technically active. We just keep it alive.
                //
                // In practice this future never resolves — the courier SSE loop
                // handles its own reconnect. We park here so select! keeps both
                // branches alive.
                tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
            }
            Err(e) => {
                retries = retries.saturating_add(1);
                let delay = COURIER_RETRY_DELAY * retries.min(6);
                warn!(
                    error = %e,
                    retry = retries,
                    delay_secs = delay.as_secs(),
                    "courier subscription failed, retrying"
                );

                // COURIER_MAX_RETRIES = u32::MAX means retry forever.
                // If changed to a finite value, uncomment:
                // if retries > COURIER_MAX_RETRIES {
                //     warn!("courier max retries exceeded, giving up");
                //     return;
                // }

                tokio::time::sleep(delay).await;

                // Ensure we cleaned up before retrying
                let mut courier = app_state.courier.lock().await;
                let _ = courier.unsubscribe(memory).await;
            }
        }
    }
}

/// Poll memory for pending tasks as a fallback. Catches tasks that courier
/// might have missed (e.g., stored while SSE was reconnecting).
async fn poll_loop(config: &WatchConfig, app_state: Arc<AppState>, memory: &Arc<MemoryMcpClient>) {
    // Initial delay — give courier a chance to connect first
    tokio::time::sleep(config.poll_interval).await;

    loop {
        match poll_once(config, &app_state, memory).await {
            Ok(n) => {
                if n > 0 {
                    info!(new_tasks = n, "poll found tasks");
                }
            }
            Err(e) => {
                // Transient memory errors: log and retry on next tick,
                // do not kill the watch loop.
                warn!(error = %e, "poll failed (transient), will retry next tick");
            }
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

/// Single poll iteration. Returns number of new tasks dispatched.
async fn poll_once(
    config: &WatchConfig,
    app_state: &Arc<AppState>,
    memory: &Arc<MemoryMcpClient>,
) -> Result<usize> {
    let agent_target = format!("agent:{}", config.agent_id);

    // Use memory_query with the same structured filter as courier subscribe
    let result = memory
        .memory_query(MemoryQueryParams {
            key: config.key.clone(),
            agent_id: config.agent_id.clone(),
            swarm_id: config.swarm_id.clone(),
            filter: json!({
                "kind": "task",
                "target": agent_target,
            }),
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(50),
        })
        .await?;

    let facts = result.get("facts").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut dispatched = 0;

    for fact in &facts {
        let task_fact_id = match fact.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => continue,
        };

        // Check if already executed via metadata-based linkage (canonical)
        let has_result = check_has_result(memory, config, task_fact_id).await;
        if has_result {
            continue;
        }

        if !app_state.claim_dispatch(task_fact_id).await {
            info!(task_fact_id = task_fact_id, "poll: skipping already-dispatched task");
            continue;
        }

        info!(task_fact_id = task_fact_id, "poll: dispatching task");

        let state = app_state.clone();
        let args = json!({
            "agent_id": config.agent_id,
            "swarm_id": config.swarm_id,
            "key": config.key,
            "task_id": task_fact_id,
            "budget_shell": config.budget_shell,
        });

        tokio::spawn(async move {
            let _ = crate::server::handle_agent_start_pub(&state, &args).await;
        });

        dispatched += 1;
    }

    Ok(dispatched)
}

/// Check if a task already has a result via canonical metadata linkage.
/// Falls back to legacy `result_<task_id>` check for compatibility.
async fn check_has_result(
    memory: &Arc<MemoryMcpClient>,
    config: &WatchConfig,
    task_fact_id: &str,
) -> bool {
    // Canonical: query by metadata.task_fact_id
    if let Ok(result) = memory
        .memory_query(MemoryQueryParams {
            key: config.key.clone(),
            agent_id: config.agent_id.clone(),
            swarm_id: config.swarm_id.clone(),
            filter: json!({
                "kind": "task_result",
                "metadata.task_fact_id": task_fact_id,
            }),
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(1),
        })
        .await
    {
        let count = result.get("facts").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        if count > 0 {
            return true;
        }
    }

    // Legacy fallback: check for result_<id> fact
    let external_id = task_fact_id; // may be fact_id or external
    if let Ok(result) = memory
        .memory_query(MemoryQueryParams {
            key: config.key.clone(),
            agent_id: config.agent_id.clone(),
            swarm_id: config.swarm_id.clone(),
            filter: json!({
                "kind": "fact",
                "metadata.task_id": external_id,
            }),
            sort_by: None,
            sort_order: None,
            limit: Some(1),
        })
        .await
    {
        let count = result.get("facts").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        if count > 0 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::check_has_result;
    use super::WatchConfig;
    use crate::client::memory::MemoryMcpClient;
    use crate::test_support::wrap_mcp_response;
    use crate::test_support::MockTransport;

    #[tokio::test]
    async fn check_has_result_uses_canonical_metadata_linkage() {
        let canonical_result = wrap_mcp_response(&json!({
            "facts": [{
                "id": "result-1",
                "kind": "task_result",
                "fact": "done",
                "metadata": {"task_fact_id": "fact-123"}
            }]
        }));
        let (transport, _) = MockTransport::new(vec![canonical_result]);
        let memory = Arc::new(MemoryMcpClient::new(transport));
        let config = WatchConfig {
            key: "proj-a".to_string(),
            agent_id: "planner".to_string(),
            swarm_id: "swarm-alpha".to_string(),
            poll_interval: Duration::from_secs(30),
            budget_shell: 10.0,
        };

        let has_result = check_has_result(&memory, &config, "fact-123").await;

        assert!(has_result);
    }

    #[tokio::test]
    async fn check_has_result_falls_back_to_legacy_lookup() {
        let canonical_empty = wrap_mcp_response(&json!({"facts": []}));
        let legacy_result = wrap_mcp_response(&json!({
            "facts": [{
                "id": "legacy-result",
                "kind": "fact",
                "metadata": {"task_id": "fact-123"}
            }]
        }));
        let (transport, _) = MockTransport::new(vec![canonical_empty, legacy_result]);
        let memory = Arc::new(MemoryMcpClient::new(transport));
        let config = WatchConfig {
            key: "proj-a".to_string(),
            agent_id: "planner".to_string(),
            swarm_id: "swarm-alpha".to_string(),
            poll_interval: Duration::from_secs(30),
            budget_shell: 10.0,
        };

        let has_result = check_has_result(&memory, &config, "fact-123").await;

        assert!(has_result);
    }
}
