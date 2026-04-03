// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

pub async fn handle() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}
