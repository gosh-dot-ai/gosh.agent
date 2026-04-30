// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::header::WWW_AUTHENTICATE;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;

use crate::server::AppState;

/// Tower middleware: `/mcp` accepts *direct* loopback callers
/// unconditionally; everyone else (including loopback via a TLS
/// frontend that sets `X-Forwarded-*`) must present a Bearer.
pub async fn require_bearer_or_loopback(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if is_direct_loopback(&addr, req.headers()) {
        return next.run(req).await;
    }

    let header = req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    let presented = match header.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => return missing_token_response(),
    };

    let valid = state.oauth_tokens.lock().await.verify_access(presented).is_some();
    if !valid {
        return invalid_token_response();
    }

    next.run(req).await
}

/// Returns `true` only when the peer IP is loopback **and** no
/// reverse-proxy forwarding headers are present. The OR is wrong:
/// a same-host TLS frontend forwards from 127.0.0.1 with the
/// forwarded headers set, and we must treat that as remote.
///
/// Public so `admin::middleware` can apply the same gate without
/// duplicating the header list.
pub(crate) fn is_direct_loopback(addr: &std::net::SocketAddr, headers: &HeaderMap) -> bool {
    addr.ip().is_loopback() && !has_forwarded_headers(headers)
}

/// True if any standard reverse-proxy forwarding header is set. We
/// don't try to *trust* their values — we only use their *presence*
/// as a signal that the request crossed a proxy boundary.
fn has_forwarded_headers(headers: &HeaderMap) -> bool {
    ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "forwarded", "x-real-ip"]
        .iter()
        .any(|name| headers.contains_key(*name))
}

/// 401 with RFC 6750 §3 `WWW-Authenticate: Bearer realm=...` and no
/// `error` (so a polite first-time caller gets the realm prompt
/// without an error code that implies an attempt).
fn missing_token_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"gosh-agent\""))],
        r#"{"error":"unauthorized","error_description":"Bearer access token required"}"#,
    )
        .into_response()
}

/// 401 with `error="invalid_token"` per RFC 6750 §3. Triggers
/// Claude.ai's connector to invoke `/oauth/token` with the saved
/// refresh token to mint a fresh access — the standard "access
/// token expired" recovery loop.
fn invalid_token_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(
                "Bearer realm=\"gosh-agent\", error=\"invalid_token\", \
                 error_description=\"access token is unknown or expired\"",
            ),
        )],
        r#"{"error":"invalid_token","error_description":"access token is unknown or expired"}"#,
    )
        .into_response()
}
