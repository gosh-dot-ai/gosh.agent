// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use serde_json::Value;

use crate::server::AppState;

/// Build the absolute issuer URL for self-references in the metadata
/// document.
///
/// Host resolution: prefer `X-Forwarded-Host` over `Host` so a
/// reverse-proxy front (Caddy / cloudflared / Tailscale Funnel) that
/// rewrites `Host` to its internal upstream value can still publish
/// the public hostname. `Host` is the fallback for the no-proxy case
/// (operator running the daemon on the open address) and `localhost`
/// is the last-ditch fallback for misshapen requests.
///
/// Scheme: `X-Forwarded-Proto` if present, else `http` for localhost
/// (dev) and `https` otherwise (operator-deployment default).
///
/// These headers shape the advertised metadata only; the request still
/// has to pass the real auth gates (PKCE, PIN, loopback) before any
/// token issues, so an attacker who can spoof `X-Forwarded-Host`
/// cannot use that to mint tokens — they can only mislead the metadata
/// document Claude.ai fetches. That's already the proxy operator's
/// trust boundary.
fn issuer_from_request(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .or_else(|| headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()))
        .unwrap_or("localhost")
        .to_string();
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| if is_loopback_host(&host) { "http" } else { "https" });
    format!("{scheme}://{host}")
}

/// True when `host` is a loopback destination, regardless of whether
/// the operator wrote `localhost`, `127.0.0.1`, `[::1]`, or any of
/// those with a `:port` suffix. Used to pick the `http` scheme
/// fallback for the metadata document — without TLS-frontend
/// `X-Forwarded-Proto`, advertising `https://127.0.0.1:8767/...` to
/// a client that just hit the daemon's plain-HTTP listener triggers
/// a TLS handshake against an HTTP socket and breaks discovery.
///
/// The previous shape (`host.starts_with("localhost")`) only covered
/// the `localhost` literal — `127.0.0.1`, `[::1]`, and `::1` all fell
/// through to `https`, breaking the default-setup case (`gosh agent
/// setup` without `--host`, daemon on `127.0.0.1`).
fn is_loopback_host(host: &str) -> bool {
    // Strip the optional `:port` suffix. IPv6 literals are bracketed
    // (`[::1]:8767`), so we take everything up to and including `]`
    // first; for IPv4 / hostnames we just split on the first `:`.
    let bare = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    };
    bare == "localhost" || bare.starts_with("127.") || bare == "[::1]"
}

pub async fn handle(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let issuer = issuer_from_request(&headers);
    let body = build_metadata(&issuer, state.oauth_dcr_enabled);
    Json(body)
}

/// Pure builder for the metadata JSON. Factored out so unit tests
/// can pin the document shape without spinning up an HTTP server.
pub fn build_metadata(issuer: &str, dcr_enabled: bool) -> Value {
    let mut doc = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "revocation_endpoint": format!("{issuer}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": [
            "client_secret_basic",
            "client_secret_post",
        ],
        "code_challenge_methods_supported": ["S256"],
        "revocation_endpoint_auth_methods_supported": [
            "client_secret_basic",
            "client_secret_post",
        ],
    });
    // Per the committed design (`<gosh.cli>/specs/agent_mcp_unification.md`),
    // we *omit* `registration_endpoint` when DCR is off so Claude.ai's
    // auto-detection sees no endpoint and falls back to expecting a
    // manually-issued client_id/secret.
    if dcr_enabled {
        doc["registration_endpoint"] = json!(format!("{issuer}/oauth/register"));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_advertises_registration_endpoint_when_dcr_on() {
        let m = build_metadata("https://example.com", true);
        assert_eq!(m["issuer"], "https://example.com");
        assert_eq!(m["authorization_endpoint"], "https://example.com/oauth/authorize");
        assert_eq!(m["token_endpoint"], "https://example.com/oauth/token");
        assert_eq!(m["registration_endpoint"], "https://example.com/oauth/register");
        assert_eq!(m["response_types_supported"], json!(["code"]));
        assert_eq!(m["code_challenge_methods_supported"], json!(["S256"]));
    }

    #[test]
    fn metadata_omits_registration_endpoint_when_dcr_off() {
        // Required by the committed design: clients auto-detect DCR
        // by presence of `registration_endpoint` in metadata, so when
        // DCR is disabled the field must be entirely absent — not
        // `null`, not an empty string. Otherwise a strict client like
        // Claude.ai might still try to register and we'd have to
        // reject mid-flow with a less actionable 405 error.
        let m = build_metadata("https://example.com", false);
        assert!(
            m.get("registration_endpoint").is_none(),
            "registration_endpoint must not appear when DCR is off, got: {m}",
        );
    }

    #[test]
    fn metadata_pkce_s256_only_no_plain_fallback() {
        // Strictly S256 — the committed design rejects `plain` to
        // make sure DCR'd public clients can't downgrade. Pin it
        // here so a future "let's accept plain too" tweak is loud.
        let m = build_metadata("https://example.com", true);
        assert_eq!(m["code_challenge_methods_supported"], json!(["S256"]));
    }

    #[test]
    fn issuer_from_request_uses_host_header_with_https_for_non_localhost() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "agent.example.com".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "https://agent.example.com");
    }

    #[test]
    fn issuer_from_request_uses_http_for_localhost() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "localhost:8767".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "http://localhost:8767");
    }

    #[test]
    fn issuer_from_request_uses_http_for_127_0_0_1() {
        // Regression: the previous heuristic only treated `localhost*`
        // as loopback, so the default `gosh agent setup` (no `--host`,
        // daemon on 127.0.0.1) had its metadata advertise
        // `https://127.0.0.1:8767/...` — a client fetching that doc
        // over plain HTTP would then try TLS handshake against an
        // HTTP socket and break discovery.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "127.0.0.1:8767".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "http://127.0.0.1:8767");
    }

    #[test]
    fn issuer_from_request_uses_http_for_ipv6_loopback() {
        // Same fix for IPv6 loopback: bracketed `[::1]` (with or
        // without `:port`) must drop into the http branch.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "[::1]:8767".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "http://[::1]:8767");
    }

    #[test]
    fn is_loopback_host_covers_all_loopback_shapes() {
        use super::is_loopback_host;
        // Hostnames
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("localhost:8767"));
        // IPv4 — full /8 block, not just 127.0.0.1
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.1:8767"));
        assert!(is_loopback_host("127.255.255.254"));
        // IPv6 — bracketed only (URI form)
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("[::1]:8767"));
        // Non-loopback must NOT match
        assert!(!is_loopback_host("agent.example.com"));
        assert!(!is_loopback_host("agent.example.com:8767"));
        assert!(!is_loopback_host("192.168.1.50"));
        assert!(!is_loopback_host("[2001:db8::1]"));
    }

    #[test]
    fn issuer_from_request_honors_x_forwarded_proto_for_reverse_proxy() {
        // Operator-deployment shape: Caddy / cloudflared terminates
        // TLS and forwards plain HTTP to the daemon, but the issuer
        // URL Claude.ai needs is the public `https://` one. The
        // proxy is expected to set X-Forwarded-Proto.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "agent.example.com".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "https://agent.example.com");
    }

    #[test]
    fn issuer_from_request_prefers_x_forwarded_host_over_host() {
        // Same-host TLS terminator forwards from 127.0.0.1 with
        // `Host: internal:8767` (its upstream rewrite) and
        // `X-Forwarded-Host: <public>`. Without honouring the
        // forwarded host, the metadata doc would advertise
        // internal endpoints to Claude.ai — those would then fail
        // DNS resolution from the public internet. Honour the
        // forwarded host so the public deployment shape works
        // out of the box.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "internal:8767".parse().unwrap());
        headers.insert("x-forwarded-host", "agent.example.com".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "https://agent.example.com");
    }

    #[test]
    fn issuer_from_request_falls_back_to_host_when_forwarded_host_empty() {
        // Defensive: an empty `X-Forwarded-Host` (e.g. a misconfigured
        // proxy that emits the header but doesn't fill it) must not
        // produce a "https:///path" URL — fall back to Host.
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "agent.example.com".parse().unwrap());
        headers.insert("x-forwarded-host", "".parse().unwrap());
        assert_eq!(issuer_from_request(&headers), "https://agent.example.com");
    }
}
