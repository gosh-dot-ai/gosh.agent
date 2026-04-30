// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::oauth::clients::verify_secret;
use crate::server::handlers::oauth::token::parse_basic_auth_header;
use crate::server::handlers::oauth::token::ResolvedClient;
use crate::server::AppState;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RevokeRequest {
    pub token: String,
    pub token_type_hint: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(req): Form<RevokeRequest>,
) -> impl IntoResponse {
    // 1. Resolve client. If client auth fails, RFC 7009 §2.2 still
    // says we MAY return 200 to avoid leakage — but clients that
    // forgot to authenticate at all should hit 401 so they fix
    // their integration, not silently fail to revoke. We return 401
    // only for missing creds; bad creds (wrong secret) get the
    // benign 200.
    let creds = match resolve_client_for_revoke(&headers, &req) {
        Some(c) => c,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                client_auth_required_headers(),
                "client authentication required",
            )
                .into_response();
        }
    };

    // 2. Verify the client. On mismatch, fall through to the silent
    // 200 — RFC 7009 §2.2.
    let client_ok = {
        let store = state.oauth_clients.lock().await;
        store.find(&creds.id).map(|c| verify_secret(&creds.secret, &c.secret_hash)).unwrap_or(false)
    };
    if !client_ok {
        return (StatusCode::OK, no_store_headers(), "").into_response();
    }

    if req.token.is_empty() {
        // Spec doesn't mandate a body; treat empty as no-op success.
        return (StatusCode::OK, no_store_headers(), "").into_response();
    }

    // 3. Try the hinted shape first, then the other. Either path
    // ends in 200 regardless of hit/miss.
    let hint = req.token_type_hint.as_deref().unwrap_or("");
    let try_refresh_first =
        hint == "refresh_token" || (hint != "access_token" && req.token.starts_with("rt_"));
    let mut tokens = state.oauth_tokens.lock().await;
    if try_refresh_first {
        if !tokens.revoke_refresh_plain(&req.token).unwrap_or(false) {
            tokens.revoke_access_plain(&req.token);
        }
    } else if !tokens.revoke_access_plain(&req.token) {
        let _ = tokens.revoke_refresh_plain(&req.token);
    }

    (StatusCode::OK, no_store_headers(), "").into_response()
}

fn resolve_client_for_revoke(headers: &HeaderMap, body: &RevokeRequest) -> Option<ResolvedClient> {
    if let Some(c) = parse_basic_auth_header(headers) {
        return Some(c);
    }
    let id = body.client_id.as_deref()?;
    let secret = body.client_secret.as_deref()?;
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(ResolvedClient { id: id.to_string(), secret: secret.to_string() })
}

fn no_store_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    h.insert("pragma", HeaderValue::from_static("no-cache"));
    h
}

fn client_auth_required_headers() -> HeaderMap {
    let mut h = no_store_headers();
    h.insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"gosh-agent\""),
    );
    h
}
