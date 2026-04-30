// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::Form;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::header::WWW_AUTHENTICATE;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::oauth::clients::verify_secret;
use crate::oauth::sessions::CodeExchangeError;
use crate::oauth::tokens::ACCESS_TTL;
use crate::server::AppState;

/// Form body for `POST /oauth/token`. Extra fields are tolerated
/// (Claude.ai may send `audience` / `resource` in some flows; we
/// just ignore unknowns).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TokenRequest {
    pub grant_type: String,
    // `authorization_code` grant
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    // `refresh_token` grant
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    // Client auth in body (alternative to HTTP Basic)
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// Resolved client identity after parsing whichever auth shape the
/// caller used. Both fields are required for any grant. Re-exported
/// for `/oauth/revoke` which shares the auth shape.
pub struct ResolvedClient {
    pub id: String,
    pub secret: String,
}

/// `POST /oauth/token` entry point. Routes on `grant_type` after
/// authenticating the client.
pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(req): Form<TokenRequest>,
) -> impl IntoResponse {
    // 1. Resolve client credentials. Failure → invalid_client (401).
    let creds = match resolve_client_creds(&headers, &req) {
        Some(c) => c,
        None => return invalid_client_response("missing client credentials"),
    };

    // 2. Verify against the registered client.
    {
        let store = state.oauth_clients.lock().await;
        let Some(client) = store.find(&creds.id) else {
            return invalid_client_response("unknown client_id");
        };
        if !verify_secret(&creds.secret, &client.secret_hash) {
            return invalid_client_response("client_secret mismatch");
        }
    }

    // 3. Dispatch on grant_type.
    match req.grant_type.as_str() {
        "authorization_code" => grant_authorization_code(&state, &creds, &req).await,
        "refresh_token" => grant_refresh_token(&state, &creds, &req).await,
        "" => oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "grant_type is required"),
        other => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("grant_type '{other}' is not supported"),
        ),
    }
}

async fn grant_authorization_code(
    state: &Arc<AppState>,
    creds: &ResolvedClient,
    req: &TokenRequest,
) -> (StatusCode, HeaderMap, Json<Value>) {
    let Some(code) = req.code.as_deref() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code is required");
    };
    let Some(redirect_uri) = req.redirect_uri.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is required for authorization_code grant",
        );
    };
    let Some(code_verifier) = req.code_verifier.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_verifier is required (PKCE S256 enforced)",
        );
    };

    // Consume the code: verifies PKCE, redirect_uri match, client
    // match, code expiry. All failure modes collapse to
    // `invalid_grant` per RFC 6749 §5.2 — the typed enum is only
    // for the daemon's own logs.
    let consumed = {
        let mut sessions = state.oauth_sessions.lock().await;
        match sessions.consume_authorization_code(code, &creds.id, redirect_uri, code_verifier) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = ?e, client = %creds.id, "oauth/token: code exchange failed");
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    code_exchange_description(&e),
                );
            }
        }
    };

    // Mint the pair. Persistence failure here would lose the refresh
    // record but the access token was already inserted into the
    // in-memory map — surface as `server_error`. The session is
    // already Consumed so a retry wouldn't help; the client redoes
    // the full /authorize flow.
    let minted = {
        let mut tokens = state.oauth_tokens.lock().await;
        match tokens.mint_pair(&consumed.client_id, consumed.scope.clone()) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "oauth/token: mint_pair failed");
                return oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "internal error issuing tokens",
                );
            }
        }
    };

    // Stamp the client's last_seen_at — display-only, best-effort
    // (a write failure here doesn't fail the exchange).
    {
        let mut store = state.oauth_clients.lock().await;
        if let Err(e) = store.touch(&creds.id) {
            tracing::warn!(error = %e, "oauth/token: client touch failed");
        }
    }

    success_response(&minted.access_token, &minted.refresh_token, minted.scope.as_deref())
}

async fn grant_refresh_token(
    state: &Arc<AppState>,
    creds: &ResolvedClient,
    req: &TokenRequest,
) -> (StatusCode, HeaderMap, Json<Value>) {
    let Some(refresh) = req.refresh_token.as_deref() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token is required",
        );
    };

    let minted = {
        let mut tokens = state.oauth_tokens.lock().await;
        match tokens.rotate_refresh(refresh, &creds.id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "refresh_token is unknown, expired, or belongs to a different client",
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "oauth/token: rotate_refresh failed");
                return oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "internal error rotating refresh token",
                );
            }
        }
    };

    {
        let mut tokens = state.oauth_tokens.lock().await;
        if let Err(e) = tokens.touch(&minted.token_id) {
            tracing::warn!(error = %e, "oauth/token: refresh touch failed");
        }
    }
    {
        let mut store = state.oauth_clients.lock().await;
        if let Err(e) = store.touch(&creds.id) {
            tracing::warn!(error = %e, "oauth/token: client touch failed");
        }
    }

    // Scope is inherited from the prior refresh record, not from the
    // request — RFC 6749 §6 forbids broadening at refresh, and we
    // don't implement narrowing yet. Ignore `req.scope` for now.
    let _ = &req.scope;
    success_response(&minted.access_token, &minted.refresh_token, minted.scope.as_deref())
}

/// Resolve `client_id` + `client_secret` from either HTTP Basic or
/// the body. RFC 6749 §2.3.1 allows both; preference is Basic when
/// both are present (consistent with most OAuth servers; closes the
/// "client sent both, which wins?" ambiguity).
fn resolve_client_creds(headers: &HeaderMap, body: &TokenRequest) -> Option<ResolvedClient> {
    if let Some(basic) = parse_basic_auth_header(headers) {
        return Some(basic);
    }
    let id = body.client_id.as_deref()?;
    let secret = body.client_secret.as_deref()?;
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(ResolvedClient { id: id.to_string(), secret: secret.to_string() })
}

/// Parse `Authorization: Basic <base64(client_id:client_secret)>`.
/// Returns `None` for any parse failure — caller treats absent and
/// malformed identically. Public so `/oauth/revoke` can share it.
pub fn parse_basic_auth_header(headers: &HeaderMap) -> Option<ResolvedClient> {
    let raw = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let b64 = raw.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (id, secret) = s.split_once(':')?;
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(ResolvedClient { id: id.to_string(), secret: secret.to_string() })
}

/// RFC 6749 §5.1 success body.
fn success_response(
    access_token: &str,
    refresh_token: &str,
    scope: Option<&str>,
) -> (StatusCode, HeaderMap, Json<Value>) {
    let mut headers = HeaderMap::new();
    // Per RFC 6749 §5.1: "The authorization server MUST include the
    // HTTP `Cache-Control` response header field with a value of
    // `no-store` ..."
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    let mut body = json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL.num_seconds(),
        "refresh_token": refresh_token,
    });
    if let Some(s) = scope {
        body["scope"] = json!(s);
    }
    (StatusCode::OK, headers, Json(body))
}

/// RFC 6749 §5.2 error envelope (also `Cache-Control: no-store`).
fn oauth_error(
    status: StatusCode,
    error: &str,
    description: &str,
) -> (StatusCode, HeaderMap, Json<Value>) {
    let mut headers = HeaderMap::new();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    (
        status,
        headers,
        Json(json!({
            "error": error,
            "error_description": description,
        })),
    )
}

/// `invalid_client` per RFC 6749 §5.2 — 401 with
/// `WWW-Authenticate: Basic realm=...`. The realm string surfaces in
/// browser auth dialogs; we keep it boring and operator-neutral.
fn invalid_client_response(description: &str) -> (StatusCode, HeaderMap, Json<Value>) {
    let mut headers = HeaderMap::new();
    headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"gosh-agent\""));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("pragma", HeaderValue::from_static("no-cache"));
    (
        StatusCode::UNAUTHORIZED,
        headers,
        Json(json!({
            "error": "invalid_client",
            "error_description": description,
        })),
    )
}

/// Map the typed code-exchange failure to a short, user-facing
/// description. The wire `error` is always `invalid_grant` — only
/// the description varies for operator triage.
fn code_exchange_description(e: &CodeExchangeError) -> &'static str {
    match e {
        CodeExchangeError::UnknownCode => {
            "authorization code is unknown or has already been exchanged"
        }
        CodeExchangeError::CodeExpired => "authorization code has expired",
        CodeExchangeError::ClientMismatch => "authorization code was issued to a different client",
        CodeExchangeError::RedirectUriMismatch => {
            "redirect_uri does not match the value supplied at /oauth/authorize"
        }
        CodeExchangeError::PkceMismatch => "PKCE code_verifier does not match code_challenge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_auth(id: &str, secret: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        h.insert(AUTHORIZATION, format!("Basic {raw}").parse().unwrap());
        h
    }

    #[test]
    fn parse_basic_auth_round_trip() {
        let h = basic_auth("client-x", "secret-y");
        let c = parse_basic_auth_header(&h).unwrap();
        assert_eq!(c.id, "client-x");
        assert_eq!(c.secret, "secret-y");
    }

    #[test]
    fn parse_basic_auth_rejects_malformed_input() {
        // No "Basic " prefix.
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert!(parse_basic_auth_header(&h).is_none());

        // Bad base64.
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert!(parse_basic_auth_header(&h).is_none());

        // No colon in decoded payload.
        let raw = base64::engine::general_purpose::STANDARD.encode("no-colon-here");
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, format!("Basic {raw}").parse().unwrap());
        assert!(parse_basic_auth_header(&h).is_none());

        // Empty id or secret.
        let raw = base64::engine::general_purpose::STANDARD.encode(":secret");
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, format!("Basic {raw}").parse().unwrap());
        assert!(parse_basic_auth_header(&h).is_none());
    }

    #[test]
    fn resolve_client_creds_prefers_basic_over_body() {
        let h = basic_auth("from-basic", "secret");
        let body = TokenRequest {
            client_id: Some("from-body".into()),
            client_secret: Some("other".into()),
            ..Default::default()
        };
        let c = resolve_client_creds(&h, &body).unwrap();
        assert_eq!(c.id, "from-basic", "Basic auth should win when both present");
    }

    #[test]
    fn resolve_client_creds_falls_back_to_body_when_no_basic() {
        let h = HeaderMap::new();
        let body = TokenRequest {
            client_id: Some("from-body".into()),
            client_secret: Some("body-secret".into()),
            ..Default::default()
        };
        let c = resolve_client_creds(&h, &body).unwrap();
        assert_eq!(c.id, "from-body");
        assert_eq!(c.secret, "body-secret");
    }

    #[test]
    fn resolve_client_creds_returns_none_when_neither_present() {
        let h = HeaderMap::new();
        let body = TokenRequest::default();
        assert!(resolve_client_creds(&h, &body).is_none());
    }
}
