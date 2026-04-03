// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;

use serde_json::json;
use serde_json::Value;

use crate::server::AppState;

pub async fn handle(state: Arc<AppState>, args: &Value) -> Value {
    let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("default");
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default");
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("default");

    let mut courier = state.courier.lock().await;
    match courier.subscribe(&state.memory, key, agent_id, swarm_id, state.clone()).await {
        Ok(sub_id) => json!({"sub_id": sub_id}),
        Err(e) => json!({"error": e.to_string(), "code": "SUBSCRIBE_ERROR"}),
    }
}
