// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tracing::info;
use tracing::warn;

use crate::client::memory::CourierSubscribeParams;
use crate::client::memory::MemoryMcpClient;
use crate::client::transport::canonical_mcp_url;
use crate::server::AppState;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Manages courier subscription lifecycle.
pub struct CourierListener {
    /// Base URL of the memory service (for SSE endpoint).
    memory_url: String,
    /// Memory server token for authenticated SSE access.
    memory_token: Option<String>,
    /// Optional TLS-pinned HTTP client (from join token).
    http_client: Option<reqwest::Client>,
    /// Subscription ID returned by courier_subscribe.
    sub_id: Option<String>,
    /// Signal to stop the listener loop.
    cancel_tx: Option<watch::Sender<bool>>,
}

impl CourierListener {
    pub fn new(
        memory_url: &str,
        memory_token: Option<String>,
        http_client: Option<reqwest::Client>,
    ) -> Self {
        Self {
            memory_url: memory_url.to_string(),
            memory_token,
            http_client,
            sub_id: None,
            cancel_tx: None,
        }
    }

    /// Subscribe to courier and start listening for new tasks.
    pub async fn subscribe(
        &mut self,
        memory: &MemoryMcpClient,
        key: &str,
        agent_id: &str,
        swarm_id: &str,
        app_state: Arc<AppState>,
    ) -> Result<String> {
        if self.sub_id.is_some() {
            anyhow::bail!("already subscribed");
        }

        // `memory_url` may legitimately come in as bare host, with a
        // trailing slash, or already including `/mcp` (per the same
        // GlobalConfig.authority_url accepted by HttpTransport and the
        // stdio mcp-proxy). Build the SSE endpoint off the canonical
        // `<base>/mcp` form so a config that already includes `/mcp`
        // doesn't double up into `/mcp/mcp/sse`.
        let sse_url = format!("{}/sse", canonical_mcp_url(&self.memory_url));
        let agent_target = format!("agent:{agent_id}");
        let filter = json!({"kind": "task", "target": agent_target});

        // Start the SSE listener first. gosh.memory generates the canonical
        // connection_id server-side and returns it in the initial `connected`
        // event; courier_subscribe must use that value.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (connected_tx, connected_rx) = oneshot::channel();

        let agent_id_owned = agent_id.to_string();
        let swarm_id_owned = swarm_id.to_string();
        let key_owned = key.to_string();
        let memory_token = self.memory_token.clone();
        let client = self.http_client.clone().unwrap_or_default();
        let app_state_for_listener = app_state.clone();

        tokio::spawn(async move {
            if let Err(e) = sse_listen_loop(
                client,
                &sse_url,
                &key_owned,
                &agent_id_owned,
                &swarm_id_owned,
                app_state_for_listener,
                memory_token,
                Some(connected_tx),
                cancel_rx,
            )
            .await
            {
                warn!(error = %e, "courier SSE listener stopped");
            }
        });

        let connection_id = tokio::time::timeout(Duration::from_secs(10), connected_rx)
            .await
            .context("timed out waiting for courier SSE connection_id")?
            .context("courier SSE listener exited before connected event")?;

        let result = memory
            .courier_subscribe(CourierSubscribeParams {
                key: key.to_string(),
                agent_id: agent_id.to_string(),
                swarm_id: swarm_id.to_string(),
                connection_id,
                filter: Some(filter),
            })
            .await?;

        let sub_id = result.get("sub_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if sub_id.is_empty() {
            anyhow::bail!("courier_subscribe returned no sub_id");
        }

        info!(sub_id = %sub_id, "subscribed to courier");
        self.sub_id = Some(sub_id.clone());
        self.cancel_tx = Some(cancel_tx);

        Ok(sub_id)
    }

    /// Unsubscribe from courier and stop the listener.
    pub async fn unsubscribe(&mut self, memory: &MemoryMcpClient) -> Result<()> {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(sub_id) = self.sub_id.take() {
            memory.courier_unsubscribe(&sub_id).await?;
            info!(sub_id = %sub_id, "unsubscribed from courier");
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn is_subscribed(&self) -> bool {
        self.sub_id.is_some()
    }
}

/// Listen to SSE stream from memory courier. On each task fact, trigger
/// agent_start. Reconnects automatically on stream drop.
#[allow(clippy::too_many_arguments)]
async fn sse_listen_loop(
    client: reqwest::Client,
    sse_url: &str,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    app_state: Arc<AppState>,
    memory_token: Option<String>,
    mut connected_tx: Option<oneshot::Sender<String>>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *cancel_rx.borrow() {
            break;
        }

        info!(url = sse_url, "connecting to courier SSE stream");

        let mut req = client.get(sse_url);
        if let Some(token) = &memory_token {
            req = req.header("x-server-token", token);
        }

        match req.send().await {
            Ok(mut resp) => {
                let mut buf = String::new();

                loop {
                    tokio::select! {
                        _ = cancel_rx.changed() => {
                            info!("courier listener cancelled");
                            return Ok(());
                        }
                        chunk = resp.chunk() => {
                            match chunk {
                                Ok(Some(bytes)) => {
                                    buf.push_str(&String::from_utf8_lossy(&bytes));
                                    process_lines(
                                        &mut buf,
                                        key,
                                        agent_id,
                                        swarm_id,
                                        &app_state,
                                        &mut connected_tx,
                                    );
                                }
                                Ok(None) => {
                                    // Stream ended (server closed connection)
                                    warn!("SSE stream ended");
                                    break;
                                }
                                Err(e) => {
                                    warn!(error = %e, "SSE stream read error");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to connect to courier SSE");
            }
        }

        // Reconnect after delay
        info!(delay_secs = RECONNECT_DELAY.as_secs(), "reconnecting to courier SSE");
        tokio::select! {
            _ = cancel_rx.changed() => {
                return Ok(());
            }
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }

    Ok(())
}

/// Extract complete lines from the buffer and dispatch SSE events.
fn process_lines(
    buf: &mut String,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    app_state: &Arc<AppState>,
    connected_tx: &mut Option<oneshot::Sender<String>>,
) {
    while let Some(pos) = buf.find('\n') {
        let line = buf[..pos].trim_end().to_string();
        *buf = buf[pos + 1..].to_string();

        if let Some(data) = line.strip_prefix("data: ") {
            dispatch_sse_event(data, key, agent_id, swarm_id, app_state, connected_tx);
        }
    }
}

/// Parse a single SSE data payload and spawn agent_start if it's a task.
#[allow(clippy::needless_return)]
fn dispatch_sse_event(
    data: &str,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    app_state: &Arc<AppState>,
    connected_tx: &mut Option<oneshot::Sender<String>>,
) {
    match classify_sse_event(data, agent_id) {
        DispatchDecision::Ignore => return,
        DispatchDecision::Connected(connection_id) => {
            if let Some(tx) = connected_tx.take() {
                let _ = tx.send(connection_id);
            }
            return;
        }
        DispatchDecision::StartTask(tid) => {
            info!(task_id = tid, "courier delivered new task");

            let state = app_state.clone();
            let mkey = key.to_string();
            let aid = agent_id.to_string();
            let sid = swarm_id.to_string();

            tokio::spawn(async move {
                if !state.claim_dispatch(&tid).await {
                    info!(task_id = tid, "courier: skipping already-dispatched task");
                    return;
                }
                let args = json!({
                    "agent_id": aid,
                    "swarm_id": sid,
                    "key": mkey,
                    "task_id": tid,
                    "budget_shell": 10.0,
                });
                let _ = crate::server::handle_agent_start_pub(&state, &args).await;
            });
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DispatchDecision {
    Ignore,
    Connected(String),
    StartTask(String),
}

fn classify_sse_event(data: &str, agent_id: &str) -> DispatchDecision {
    let event: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return DispatchDecision::Ignore,
    };

    if event.get("type").and_then(|t| t.as_str()) == Some("connected") {
        return event
            .get("connection_id")
            .and_then(|v| v.as_str())
            .map(|id| DispatchDecision::Connected(id.to_string()))
            .unwrap_or(DispatchDecision::Ignore);
    }

    if event.get("type").and_then(|t| t.as_str()) != Some("artifact") {
        return DispatchDecision::Ignore;
    }

    let payload = match event.get("payload") {
        Some(p) => p,
        None => return DispatchDecision::Ignore,
    };

    if payload.get("kind").and_then(|v| v.as_str()) != Some("task") {
        return DispatchDecision::Ignore;
    }

    let agent_target = format!("agent:{agent_id}");
    let target_ok = match payload.get("target") {
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(&agent_target)),
        Some(Value::String(s)) => s == &agent_target,
        _ => false,
    };
    if !target_ok {
        warn!(agent_id = agent_id, "courier: ignoring task with wrong/missing target");
        return DispatchDecision::Ignore;
    }

    payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(|id| DispatchDecision::StartTask(id.to_string()))
        .unwrap_or(DispatchDecision::Ignore)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::classify_sse_event;
    use super::DispatchDecision;

    #[test]
    fn classify_sse_event_accepts_connected_handshake() {
        let decision = classify_sse_event(
            &json!({"type": "connected", "connection_id": "conn-1"}).to_string(),
            "planner",
        );

        assert_eq!(decision, DispatchDecision::Connected("conn-1".to_string()));
    }

    #[test]
    fn classify_sse_event_rejects_non_task_artifact() {
        let decision = classify_sse_event(
            &json!({
                "type": "artifact",
                "payload": {"id": "note-1", "kind": "note", "target": ["agent:planner"]}
            })
            .to_string(),
            "planner",
        );

        assert_eq!(decision, DispatchDecision::Ignore);
    }

    #[test]
    fn classify_sse_event_rejects_wrong_target() {
        let decision = classify_sse_event(
            &json!({
                "type": "artifact",
                "payload": {"id": "task-1", "kind": "task", "target": ["agent:other"]}
            })
            .to_string(),
            "planner",
        );

        assert_eq!(decision, DispatchDecision::Ignore);
    }

    #[test]
    fn classify_sse_event_accepts_task_for_target_agent() {
        let decision = classify_sse_event(
            &json!({
                "type": "artifact",
                "payload": {"id": "task-1", "kind": "task", "target": ["agent:planner"]}
            })
            .to_string(),
            "planner",
        );

        assert_eq!(decision, DispatchDecision::StartTask("task-1".to_string()));
    }

    #[test]
    fn classify_sse_event_ignores_malformed_json() {
        let decision = classify_sse_event("{not-json", "planner");

        assert_eq!(decision, DispatchDecision::Ignore);
    }

    #[test]
    fn classify_sse_event_ignores_result_artifacts() {
        let decision = classify_sse_event(
            &json!({
                "type": "artifact",
                "payload": {"id": "result-1", "kind": "task_result", "target": ["agent:planner"]}
            })
            .to_string(),
            "planner",
        );

        assert_eq!(decision, DispatchDecision::Ignore);
    }
}
