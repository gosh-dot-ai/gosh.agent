// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::oauth::clients::validate_redirect_uri;
use crate::oauth::clients::ClientSource;
use crate::server::AppState;

/// Subset of RFC 7591 client-metadata we read from the request. Other
/// fields the spec lists (token_endpoint_auth_method, scope,
/// contacts, tos_uri, …) are accepted but ignored — the daemon uses
/// fixed values for the auth methods it supports and doesn't need the
/// rest.
#[derive(Debug, Deserialize)]
pub struct ClientMetadataRequest {
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub grant_types: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub response_types: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub scope: Option<String>,
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ClientMetadataRequest>,
) -> impl IntoResponse {
    if !state.oauth_dcr_enabled {
        // Per the committed design: when DCR is off, the metadata
        // omits `registration_endpoint` so well-behaved clients
        // shouldn't reach this code path. Returning 405 makes the
        // "you've reached a disabled endpoint" case actionable
        // rather than wedging clients on a generic 404.
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({
                "error": "invalid_client_metadata",
                "error_description":
                    "Dynamic Client Registration is disabled on this server. \
                     The operator must run `gosh agent oauth clients register` \
                     and provide credentials manually.",
            })),
        )
            .into_response();
    }

    // RFC 7591 §2: `redirect_uris` is REQUIRED for clients using a
    // redirection-based grant. We support only `authorization_code`,
    // so an empty list means the client has no usable flow.
    if req.redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_redirect_uri",
                "error_description":
                    "redirect_uris is required and must contain at least one \
                     absolute http(s) URI per RFC 7591 §2 + RFC 6749 §3.1.2.",
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

    let name = sanitise_client_name(req.client_name.as_deref());
    let mut store = state.oauth_clients.lock().await;
    let registered = match store.register(&name, ClientSource::Dcr, req.redirect_uris.clone()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "oauth: DCR register failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "server_error",
                    "error_description": "could not persist client registration",
                })),
            )
                .into_response();
        }
    };

    // Per RFC 7591 §3.2.1: response includes `client_id`,
    // `client_secret`, `client_id_issued_at` (epoch seconds), and
    // `client_secret_expires_at` (0 == never). Echo back the
    // metadata fields the client sent so it can confirm what we
    // accepted.
    let body: Value = json!({
        "client_id": registered.client_id,
        "client_secret": registered.client_secret,
        "client_id_issued_at": registered.client.created_at.timestamp(),
        "client_secret_expires_at": 0,
        "client_name": registered.client.name,
        "redirect_uris": registered.client.redirect_uris,
        "token_endpoint_auth_method": "client_secret_basic",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
    });
    (StatusCode::CREATED, Json(body)).into_response()
}

/// Ensure the stored `name` is non-empty and bounded in length.
/// Defensive only — the field is display-text used by `oauth clients
/// list`, not a security boundary, but a 64-KB `client_name` would
/// bloat the on-disk store and the admin UI for no upside.
fn sanitise_client_name(raw: Option<&str>) -> String {
    let trimmed = raw.unwrap_or("").trim();
    let bounded = if trimmed.chars().count() > 200 {
        trimmed.chars().take(200).collect::<String>()
    } else {
        trimmed.to_string()
    };
    if bounded.is_empty() {
        "unnamed-dcr-client".to_string()
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use super::sanitise_client_name;

    #[test]
    fn sanitise_client_name_replaces_empty_with_placeholder() {
        assert_eq!(sanitise_client_name(None), "unnamed-dcr-client");
        assert_eq!(sanitise_client_name(Some("")), "unnamed-dcr-client");
        assert_eq!(sanitise_client_name(Some("   ")), "unnamed-dcr-client");
    }

    #[test]
    fn sanitise_client_name_passes_normal_names_through() {
        assert_eq!(sanitise_client_name(Some("Claude.ai")), "Claude.ai");
        assert_eq!(sanitise_client_name(Some("My App v1")), "My App v1");
    }

    #[test]
    fn sanitise_client_name_caps_at_200_chars() {
        let long = "a".repeat(500);
        let out = sanitise_client_name(Some(&long));
        assert_eq!(out.chars().count(), 200);
    }
}
