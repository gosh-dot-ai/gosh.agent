// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use serde_json::json;
use serde_json::Value;

use crate::client::memory::MemoryQueryParams;
use crate::server::AppState;

pub async fn handle(state: &AppState, args: &Value) -> Value {
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("default");
    let agent_id = args.get("agent_id").and_then(|v| v.as_str()).unwrap_or(&state.agent_id);
    let swarm_id = args.get("swarm_id").and_then(|v| v.as_str()).unwrap_or("default");
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);

    let filter = json!({
        "kind": "task",
        "target": format!("agent:{agent_id}"),
    });

    let result = state
        .memory
        .memory_query(MemoryQueryParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            filter,
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(limit),
        })
        .await;

    match result {
        Ok(value) => value,
        Err(e) => json!({
            "error": format!("failed to list tasks: {e}"),
            "code": "QUERY_ERROR",
        }),
    }
}
