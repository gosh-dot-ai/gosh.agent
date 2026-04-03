// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::Agent;
use crate::client::memory::MemoryMcpClient;
use crate::courier::CourierListener;

pub(crate) const DISPATCH_TRACKER_CAPACITY: usize = 10_000;

#[derive(Default)]
pub struct DispatchedTracker {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DispatchedTracker {
    fn claim(&mut self, task_fact_id: &str) -> bool {
        if self.seen.contains(task_fact_id) {
            return false;
        }

        if self.order.len() >= DISPATCH_TRACKER_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }

        let owned = task_fact_id.to_string();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

/// Shared state across all request handlers.
pub struct AppState {
    pub agent: Agent,
    pub memory: Arc<MemoryMcpClient>,
    pub courier: Mutex<CourierListener>,
    pub session_counter: Mutex<u64>,
    pub dispatched_tasks: Mutex<DispatchedTracker>,
    pub in_flight_tasks: Mutex<HashSet<String>>,
    pub in_flight_by_agent: Mutex<HashMap<String, usize>>,
}

impl AppState {
    pub async fn claim_dispatch(&self, task_fact_id: &str) -> bool {
        let mut dispatched = self.dispatched_tasks.lock().await;
        dispatched.claim(task_fact_id)
    }
}

#[cfg(test)]
mod tests {
    use super::DISPATCH_TRACKER_CAPACITY;
    use crate::test_support::test_app_state;

    #[tokio::test]
    async fn claim_dispatch_deduplicates_task_ids_across_watch_paths() {
        let (state, _) = test_app_state(vec![]);

        assert!(state.claim_dispatch("task-123").await);
        assert!(!state.claim_dispatch("task-123").await);
        assert!(state.claim_dispatch("task-456").await);
    }

    #[tokio::test]
    async fn claim_dispatch_evicts_oldest_entries_when_capacity_is_exceeded() {
        let (state, _) = test_app_state(vec![]);

        for i in 0..(DISPATCH_TRACKER_CAPACITY + 1) {
            assert!(state.claim_dispatch(&format!("task-{i}")).await);
        }

        let dispatched = state.dispatched_tasks.lock().await;
        assert_eq!(dispatched.len(), DISPATCH_TRACKER_CAPACITY);
        drop(dispatched);

        assert!(state.claim_dispatch("task-0").await);
    }
}
