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

/// `GET /admin/oauth/tokens` — list refresh-token records. The view
/// strips `token_hash`; admin token alone must not be enough to
/// reconstruct the on-disk hash from the API surface.
pub async fn list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let view = state.oauth_tokens.lock().await.list_refresh();
    Json(json!({ "tokens": view }))
}

/// `DELETE /admin/oauth/tokens/<token_id>` — revoke a refresh token
/// by its operator-visible `token_id` and cascade-evict every active
/// access token minted from it. Idempotent: returns
/// `removed=false` when the id wasn't there.
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(token_id): Path<String>,
) -> impl IntoResponse {
    match state.oauth_tokens.lock().await.revoke_by_id(&token_id) {
        Ok(removed) => (StatusCode::OK, Json(json!({ "removed": removed }))).into_response(),
        Err(e) => {
            tracing::error!(error = %e, token_id = %token_id, "oauth/admin: revoke_by_id failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "server_error",
                    "error_description": "internal error revoking token",
                })),
            )
                .into_response()
        }
    }
}
