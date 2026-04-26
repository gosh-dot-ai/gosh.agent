// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use serde_json::json;
use serde_json::Value;

use crate::server::AppState;

pub async fn handle(state: &AppState) -> Value {
    let mut courier = state.courier.lock().await;
    match courier.unsubscribe(&state.memory).await {
        Ok(()) => json!({"status": "ok"}),
        Err(e) => json!({"error": e.to_string(), "code": "UNSUBSCRIBE_ERROR"}),
    }
}
