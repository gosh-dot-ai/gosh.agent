// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::oauth::clients::validate_redirect_uri;
use crate::oauth::clients::ClientSource;
use crate::oauth::clients::OAuthClient;
use crate::server::AppState;

/// Display shape for an OAuth client — `secret_hash` is intentionally
/// omitted from the response so admin listings can't be used to
/// reconstruct anything sensitive about a client even by an attacker
/// who has the admin token.
#[derive(Debug, Clone, Serialize)]
pub struct ClientView {
    pub client_id: String,
    pub name: String,
    pub source: ClientSource,
    pub redirect_uris: Vec<String>,
    pub created_at: String,
    pub last_seen_at: Option<String>,
}

impl From<&OAuthClient> for ClientView {
    fn from(c: &OAuthClient) -> Self {
        Self {
            client_id: c.client_id.clone(),
            name: c.name.clone(),
            source: c.source,
            redirect_uris: c.redirect_uris.clone(),
            created_at: c.created_at.to_rfc3339(),
            last_seen_at: c.last_seen_at.map(|d| d.to_rfc3339()),
        }
    }
}

/// `GET /admin/oauth/clients` — list every registered client.
pub async fn list(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let clients = state.oauth_clients.lock().await.list();
    let view: Vec<ClientView> = clients.iter().map(ClientView::from).collect();
    Json(json!({ "clients": view }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    /// Redirect URIs the client may pass to `/oauth/authorize`.
    /// `#[serde(default)]` keeps the JSON shape lenient — missing /
    /// empty list is still parsed, then explicitly rejected in the
    /// handler with `invalid_redirect_uri` so the operator gets a
    /// clear error message instead of a silently-unusable client
    /// (the `/authorize` exact-match check would refuse every
    /// redirect_uri for an empty registered set).
    #[serde(default)]
    pub redirect_uris: Vec<String>,
}

/// `POST /admin/oauth/clients` — manually register a client. The
/// only place plaintext `client_secret` is returned over the wire;
/// after this call only its hash is on disk. CLI prints it to the
/// operator who then pastes into Claude.ai's connector form.
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let name = req.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_request",
                "error_description": "name must not be empty",
            })),
        )
            .into_response();
    }
    // Symmetric with DCR `/oauth/register`: at least one well-formed
    // redirect URI is required, otherwise `/oauth/authorize` can
    // never accept this client. The lenient #[serde(default)] above
    // exists only so missing-field input still produces this clear
    // 400 rather than a serde "missing field" error.
    if req.redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_redirect_uri",
                "error_description":
                    "redirect_uris is required and must contain at least one \
                     absolute http(s) URI; otherwise the registered client \
                     cannot complete the authorize flow.",
            })),
        )
            .into_response();
    }
    for uri in &req.redirect_uris {
        if let Err(reason) = validate_redirect_uri(uri) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_redirect_uri",
                    "error_description": format!("redirect_uri {uri:?} rejected: {reason}"),
                })),
            )
                .into_response();
        }
    }
    let mut store = state.oauth_clients.lock().await;
    match store.register(name, ClientSource::Manual, req.redirect_uris.clone()) {
        Ok(r) => (
            StatusCode::CREATED,
            Json(json!({
                "client_id": r.client_id,
                "client_secret": r.client_secret,
                "name": r.client.name,
                "redirect_uris": r.client.redirect_uris,
                "created_at": r.client.created_at.to_rfc3339(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "admin: manual register failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "server_error",
                    "error_description": "could not persist client registration",
                })),
            )
                .into_response()
        }
    }
}

/// `DELETE /admin/oauth/clients/<client_id>` — revoke a client and
/// cascade-revoke every refresh + access token issued to that client.
/// Idempotent: returns `200` with `{"removed": false, "revoked_tokens": 0}`
/// when the id wasn't there, `200` with `{"removed": true, "revoked_tokens":
/// N}` when it was. Without the cascade, an attacker who once held an
/// access token for a now-deleted client would keep passing
/// `/mcp` until the access expiry; cascade closes that window
/// immediately.
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(client_id): Path<String>,
) -> impl IntoResponse {
    let removed = {
        let mut store = state.oauth_clients.lock().await;
        match store.revoke(&client_id) {
            Ok(removed) => removed,
            Err(e) => {
                tracing::warn!(error = %e, "admin: client revoke failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "server_error",
                        "error_description": "could not persist client revocation",
                    })),
                )
                    .into_response();
            }
        }
    };

    // Cascade only when the client actually existed: a no-op revoke
    // (idempotent re-call) shouldn't churn the token store.
    let revoked_tokens = if removed {
        let mut tokens = state.oauth_tokens.lock().await;
        match tokens.revoke_by_client(&client_id) {
            Ok(n) => n,
            Err(e) => {
                // Client record is already gone, but we failed to
                // persist the token cascade. Log loudly — operator
                // should re-run the revoke (the cascade is
                // idempotent on a second run, since the in-memory
                // map is already drained).
                tracing::error!(
                    error = %e,
                    client = %client_id,
                    "admin: client revoke succeeded but token cascade failed to persist",
                );
                0
            }
        }
    } else {
        0
    };

    (StatusCode::OK, Json(json!({ "removed": removed, "revoked_tokens": revoked_tokens })))
        .into_response()
}
