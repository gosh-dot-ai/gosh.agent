// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;

use crate::server::handlers::mcp_auth::is_direct_loopback;
use crate::server::AppState;

/// Tower middleware: gate `/admin/*` on direct loopback origin
/// (loopback peer + no forwarding headers) + matching admin Bearer.
pub async fn require_admin_auth(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !is_direct_loopback(&addr, req.headers()) {
        // Don't leak why we rejected — same response shape as
        // a missing/invalid token. Defence-in-depth: the public
        // metadata at `/.well-known/oauth-authorization-server`
        // doesn't advertise admin paths, so a remote scanner
        // shouldn't even reach here without active probing.
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let header = req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    let provided = header.strip_prefix("Bearer ").unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), state.admin_token.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await
}

/// Constant-time byte comparison so timing the response doesn't
/// reveal how many leading bytes of the token matched. The admin
/// token is high-entropy so timing attacks are impractical anyway,
/// but the cost of doing it right is tiny.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_equal_strings() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn constant_time_eq_rejects_length_mismatch() {
        assert!(!constant_time_eq(b"prefix", b"prefix-extended"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
