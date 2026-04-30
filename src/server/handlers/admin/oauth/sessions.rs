// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::server::AppState;

/// `GET /admin/oauth/sessions` — list active sessions. The view
/// strips PIN and authorization-code fields; admin token alone
/// must not be enough to read in-flight credential material.
pub async fn list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let view = state.oauth_sessions.lock().await.list();
    Json(json!({ "sessions": view }))
}

/// `DELETE /admin/oauth/sessions/<session_id>` — drop a pending
/// session. Idempotent: returns `removed=false` when the id
/// wasn't there. Useful both for explicit "kill suspicious
/// pending session" and for prompt-cancellation flows.
pub async fn drop(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let removed = state.oauth_sessions.lock().await.drop_session(&session_id);
    Json(json!({ "removed": removed }))
}

/// `POST /admin/oauth/sessions/<session_id>/pin` — mint a PIN tied
/// to the session. 6 digits, 5-minute TTL, one-time use. Re-issuing
/// invalidates the prior PIN (the store overwrites). Returns 404
/// when the session id doesn't resolve to a pending session — the
/// CLI shows a friendly "session not pending" message rather than
/// a generic 500.
pub async fn issue_pin(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let result = state.oauth_sessions.lock().await.issue_pin(&session_id);
    match result {
        Some(pin) => (StatusCode::CREATED, Json(json!({ "pin": pin }))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "not_pending",
                "error_description":
                    "session not found, expired, or already approved/denied",
            })),
        )
            .into_response(),
    }
}
