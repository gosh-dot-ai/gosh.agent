// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_stream::Stream;

use crate::agent::task::TaskProgressEvent;

#[derive(Clone, Debug)]
pub struct McpSseEvent {
    pub event: &'static str,
    pub data: Value,
}

#[derive(Clone, Default)]
pub struct McpEventHub {
    inner: Arc<McpEventHubInner>,
}

#[derive(Default)]
struct McpEventHubInner {
    next_stream_id: AtomicU64,
    streams_by_session: Mutex<HashMap<String, HashMap<u64, mpsc::UnboundedSender<McpSseEvent>>>>,
    sessions_by_task: Mutex<HashMap<String, HashSet<String>>>,
}

impl McpEventHub {
    pub async fn subscribe(&self, session_id: &str) -> McpEventSubscription {
        let stream_id = self.inner.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut streams = self.inner.streams_by_session.lock().await;
        streams.entry(session_id.to_string()).or_default().insert(stream_id, tx);
        McpEventSubscription {
            hub: self.clone(),
            session_id: session_id.to_string(),
            stream_id,
            rx,
        }
    }

    pub async fn bind_task_session(&self, task_fact_id: &str, session_id: &str) {
        let mut sessions = self.inner.sessions_by_task.lock().await;
        sessions.entry(task_fact_id.to_string()).or_default().insert(session_id.to_string());
    }

    pub async fn emit_task_progress(&self, progress: TaskProgressEvent) {
        let Some(task_fact_id) = progress.task_fact_id.as_deref() else {
            return;
        };

        let session_ids = {
            let sessions = self.inner.sessions_by_task.lock().await;
            sessions.get(task_fact_id).cloned().unwrap_or_default()
        };
        if session_ids.is_empty() {
            return;
        }

        let event = McpSseEvent { event: "message", data: progress_notification(&progress) };
        self.send_to_sessions(&session_ids, event).await;

        if progress.terminal {
            let mut sessions = self.inner.sessions_by_task.lock().await;
            sessions.remove(task_fact_id);
        }
    }

    async fn send_to_sessions(&self, session_ids: &HashSet<String>, event: McpSseEvent) {
        let mut streams = self.inner.streams_by_session.lock().await;
        for session_id in session_ids {
            let Some(senders) = streams.get_mut(session_id) else {
                continue;
            };
            senders.retain(|_, tx| tx.send(event.clone()).is_ok());
        }
        streams.retain(|_, senders| !senders.is_empty());
    }

    async fn unsubscribe(&self, session_id: &str, stream_id: u64) {
        let session_has_streams = {
            let mut streams = self.inner.streams_by_session.lock().await;
            if let Some(senders) = streams.get_mut(session_id) {
                senders.remove(&stream_id);
                if senders.is_empty() {
                    streams.remove(session_id);
                    false
                } else {
                    true
                }
            } else {
                false
            }
        };

        if session_has_streams {
            return;
        }

        let mut sessions = self.inner.sessions_by_task.lock().await;
        for bound_sessions in sessions.values_mut() {
            bound_sessions.remove(session_id);
        }
        sessions.retain(|_, bound_sessions| !bound_sessions.is_empty());
    }

    #[cfg(test)]
    pub async fn bound_sessions_for_task(&self, task_fact_id: &str) -> Vec<String> {
        let sessions = self.inner.sessions_by_task.lock().await;
        let mut values = sessions
            .get(task_fact_id)
            .map(|set| set.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        values.sort();
        values
    }

    #[cfg(test)]
    pub async fn active_stream_count(&self, session_id: &str) -> usize {
        let streams = self.inner.streams_by_session.lock().await;
        streams.get(session_id).map(HashMap::len).unwrap_or_default()
    }
}

pub struct McpEventSubscription {
    hub: McpEventHub,
    session_id: String,
    stream_id: u64,
    rx: mpsc::UnboundedReceiver<McpSseEvent>,
}

impl McpEventSubscription {
    #[cfg(test)]
    pub async fn recv(&mut self) -> Option<McpSseEvent> {
        self.rx.recv().await
    }
}

impl Stream for McpEventSubscription {
    type Item = McpSseEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.rx).poll_recv(cx)
    }
}

impl Drop for McpEventSubscription {
    fn drop(&mut self) {
        let hub = self.hub.clone();
        let session_id = self.session_id.clone();
        let stream_id = self.stream_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                hub.unsubscribe(&session_id, stream_id).await;
            });
        }
    }
}

fn progress_notification(progress: &TaskProgressEvent) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {
            "progressToken": progress.task_fact_id.as_deref().unwrap_or(progress.task_id.as_str()),
            "progress": progress.progress,
            "total": progress.total,
            "message": progress.message,
            "_meta": {
                "task_id": progress.task_id,
                "task_fact_id": progress.task_fact_id,
                "external_task_id": progress.external_task_id,
                "stage": progress.stage,
                "terminal": progress.terminal,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::McpEventHub;
    use crate::agent::task::TaskProgressEvent;

    #[tokio::test]
    async fn task_progress_is_delivered_to_bound_session_streams() {
        let hub = McpEventHub::default();
        let mut rx = hub.subscribe("session_1").await;
        hub.bind_task_session("task_fact_1", "session_1").await;

        hub.emit_task_progress(TaskProgressEvent::new(
            "task_1",
            Some("task_fact_1"),
            Some("task_1"),
            "execution",
            6,
            9,
            "starting task execution",
        ))
        .await;

        let event = rx.recv().await.expect("progress event");
        assert_eq!(event.event, "message");
        assert_eq!(event.data["method"], "notifications/progress");
        assert_eq!(event.data["params"]["progressToken"], "task_fact_1");
        assert_eq!(event.data["params"]["progress"], 6);
        assert_eq!(event.data["params"]["_meta"]["stage"], "execution");
    }

    #[tokio::test]
    async fn terminal_progress_removes_task_session_binding() {
        let hub = McpEventHub::default();
        let _rx = hub.subscribe("session_1").await;
        hub.bind_task_session("task_fact_1", "session_1").await;

        hub.emit_task_progress(
            TaskProgressEvent::new(
                "task_1",
                Some("task_fact_1"),
                Some("task_1"),
                "done",
                9,
                9,
                "task finished",
            )
            .terminal(),
        )
        .await;

        assert!(hub.bound_sessions_for_task("task_fact_1").await.is_empty());
    }

    #[tokio::test]
    async fn dropping_idle_subscription_removes_stream_and_binding() {
        let hub = McpEventHub::default();
        let rx = hub.subscribe("session_1").await;
        hub.bind_task_session("task_fact_1", "session_1").await;
        assert_eq!(hub.active_stream_count("session_1").await, 1);
        assert_eq!(hub.bound_sessions_for_task("task_fact_1").await, vec!["session_1"]);

        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if hub.active_stream_count("session_1").await == 0
                    && hub.bound_sessions_for_task("task_fact_1").await.is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subscription cleanup should complete");
    }
}
