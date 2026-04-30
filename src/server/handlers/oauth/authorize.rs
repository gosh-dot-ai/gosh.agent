// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use axum::extract::Form;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Redirect;
use serde::Deserialize;

use crate::oauth::sessions::ApproveError;
use crate::oauth::sessions::AuthorizeRequest;
use crate::server::AppState;

/// Query string for `GET /oauth/authorize` per RFC 6749 §4.1.1
/// + RFC 7636 §4.3 (PKCE).
#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub state: Option<String>,
    pub code_challenge: String,
    pub code_challenge_method: String,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Form body for `POST /oauth/authorize`. Operator submits this from
/// the consent page after typing the PIN.
#[derive(Debug, Deserialize)]
pub struct AuthorizeSubmit {
    pub session_id: String,
    pub pin: String,
    /// `approve` or `deny`. The Deny button just drops the session
    /// without touching `redirect_uri` — Claude.ai's flow times out
    /// and surfaces a "user cancelled" error to the caller.
    pub action: String,
}

/// `GET /oauth/authorize` — validate, allocate session, render
/// consent. Errors that prevent session creation render a
/// browser-readable error page instead of redirecting; this keeps
/// open-redirect risk off the table for malformed requests
/// (RFC 6749 §4.1.2.1: "The authorization server SHOULD NOT
/// redirect the user-agent to the redirection URI").
pub async fn handle_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AuthorizeQuery>,
) -> impl IntoResponse {
    if q.response_type != "code" {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Unsupported response_type",
            "This server only supports `response_type=code` (Authorization \
             Code with PKCE per OAuth 2.1).",
        );
    }
    if q.code_challenge_method != "S256" {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Unsupported PKCE method",
            "This server requires `code_challenge_method=S256`. Plain \
             challenges are rejected per the committed design.",
        );
    }
    if q.code_challenge.is_empty() {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Missing code_challenge",
            "PKCE `code_challenge` is required.",
        );
    }
    if q.redirect_uri.is_empty() {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Missing redirect_uri",
            "OAuth `redirect_uri` is required so the authorization \
             code can be returned to the calling client.",
        );
    }

    // Look up the client. Failure here doesn't redirect either —
    // an unregistered `client_id` (or a mismatched redirect_uri) means
    // we have no validated redirect target to send the error to, so
    // we render an error page rather than 302'ing back per
    // RFC 6749 §4.1.2.1 (open-redirect avoidance).
    let client_name = {
        let store = state.oauth_clients.lock().await;
        match store.find(&q.client_id) {
            Some(c) => {
                // Exact-match per RFC 6749 §3.1.2.3 + RFC 7591 §2:
                // the `redirect_uri` parameter MUST equal one of the
                // values the client registered. A client whose
                // registered set is empty (e.g. a pre-7e on-disk
                // record loaded from a now-defaulted field, or a
                // manual-register that didn't supply any) cannot
                // pass this gate — operator must re-register with
                // explicit URIs.
                if !c.redirect_uris.iter().any(|u| u == &q.redirect_uri) {
                    return error_page(
                        StatusCode::BAD_REQUEST,
                        "Redirect URI mismatch",
                        "The supplied `redirect_uri` is not registered \
                         for this `client_id`. Re-register the client \
                         with the URI you actually want, or update the \
                         OAuth client config in the calling app to one \
                         of the URIs registered here.",
                    );
                }
                c.name.clone()
            }
            None => {
                return error_page(
                    StatusCode::BAD_REQUEST,
                    "Unknown client",
                    "The supplied `client_id` is not registered with this \
                     server. If you're configuring a Claude.ai connector, \
                     either enable Dynamic Client Registration (default) \
                     or register the client manually with `gosh agent \
                     oauth clients register`.",
                );
            }
        }
    };

    // Allocate the session.
    let session = {
        let mut store = state.oauth_sessions.lock().await;
        store.create(AuthorizeRequest {
            client_id: q.client_id,
            redirect_uri: q.redirect_uri,
            state: q.state,
            code_challenge: q.code_challenge,
            code_challenge_method: q.code_challenge_method,
            scope: q.scope,
        })
    };

    Html(consent_html(&session.session_id, &session.client_id, &client_name, &session.redirect_uri))
        .into_response()
}

/// `POST /oauth/authorize` — operator submitted PIN. On success,
/// 302 → `redirect_uri?code=...&state=...`. On failure, re-render
/// the consent page with an inline error so the operator can retry
/// (within session+PIN TTL).
pub async fn handle_post(
    State(state): State<Arc<AppState>>,
    Form(form): Form<AuthorizeSubmit>,
) -> impl IntoResponse {
    if form.action == "deny" {
        let mut store = state.oauth_sessions.lock().await;
        // Idempotent: deny() returns false if not pending, drop_session
        // cleans up either way.
        store.deny(&form.session_id);
        store.drop_session(&form.session_id);
        return error_page(
            StatusCode::OK,
            "Connection denied",
            "You denied this connection. The remote client will see a \
             cancellation error; you can close this tab.",
        );
    }

    let outcome = {
        let mut store = state.oauth_sessions.lock().await;
        store.approve(&form.session_id, &form.pin)
    };

    match outcome {
        Ok(o) => {
            // Build redirect URL: `<redirect_uri>?code=<code>&state=<state>`.
            // Preserve any existing query string on the redirect_uri
            // (some IdPs encode their callback id there).
            let separator = if o.redirect_uri.contains('?') { '&' } else { '?' };
            let mut url = format!("{}{}code={}", o.redirect_uri, separator, o.authorization_code);
            if let Some(s) = o.state {
                url.push_str("&state=");
                url.push_str(&urlencoding::encode(&s));
            }
            Redirect::to(&url).into_response()
        }
        Err(e) => {
            let (title, body) = match e {
                ApproveError::UnknownSession => (
                    "Unknown session",
                    "This session ID isn't recognised. The flow may have \
                     timed out; restart from Claude.ai's connector form.",
                ),
                ApproveError::Expired => (
                    "Session expired",
                    "More than 10 minutes elapsed since the connection \
                     was opened. Restart from Claude.ai's connector form.",
                ),
                ApproveError::AlreadyDecided => (
                    "Session already decided",
                    "This session was already approved or denied. \
                     Restart from Claude.ai's connector form.",
                ),
                ApproveError::NoPin => (
                    "No PIN issued yet",
                    "Run `gosh agent oauth sessions pin <session_id>` on \
                     the agent host to mint a PIN, then return here and \
                     enter it.",
                ),
                ApproveError::PinExpired => (
                    "PIN expired",
                    "PINs are valid for 5 minutes. Run `gosh agent oauth \
                     sessions pin <session_id>` again on the agent host.",
                ),
                ApproveError::PinMismatch => (
                    "Wrong PIN",
                    "The PIN didn't match. You can retry within the PIN's \
                     5-minute window.",
                ),
            };
            error_page(StatusCode::UNAUTHORIZED, title, body)
        }
    }
}

/// Minimal embedded HTML for the consent page. No JS, no external
/// CSS — keeps the daemon a single binary, and avoids any path
/// where a remote asset could observe the consent flow.
fn consent_html(
    session_id: &str,
    client_id: &str,
    client_name: &str,
    redirect_uri: &str,
) -> String {
    let session_id_e = html_escape(session_id);
    let client_id_e = html_escape(client_id);
    let client_name_e = html_escape(client_name);
    let redirect_uri_e = html_escape(redirect_uri);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>gosh-agent — connect Claude.ai</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         max-width: 38rem; margin: 3rem auto; padding: 0 1rem; line-height: 1.5; }}
  h1 {{ font-size: 1.4rem; }}
  code, pre {{ background: #f4f4f6; padding: 0.1rem 0.3rem; border-radius: 4px;
              font-family: "SF Mono", Menlo, monospace; font-size: 0.9rem; }}
  pre {{ padding: 0.6rem 0.8rem; overflow-x: auto; }}
  .session {{ font-size: 1.1rem; margin: 1rem 0; }}
  .meta {{ color: #555; font-size: 0.9rem; }}
  .meta dt {{ font-weight: 600; margin-top: 0.4rem; }}
  .meta dd {{ margin: 0.1rem 0 0.4rem 0; word-break: break-all; }}
  .row {{ display: flex; gap: 0.6rem; margin-top: 1rem; }}
  input[type=text] {{ font-size: 1.4rem; letter-spacing: 0.2rem;
                       padding: 0.5rem; flex: 1; min-width: 0;
                       font-family: "SF Mono", Menlo, monospace; }}
  button {{ font-size: 1rem; padding: 0.5rem 1rem; cursor: pointer; }}
  .approve {{ background: #2c7be5; color: white; border: 0; }}
  .deny {{ background: #f4f4f6; color: #333; border: 1px solid #ccc; }}
</style>
</head>
<body>
<h1>Claude.ai is connecting to gosh-agent</h1>

<p class="session">Session ID: <code>{session_id_e}</code></p>

<p>On the agent host, mint a one-time PIN:</p>
<pre>gosh agent oauth sessions pin {session_id_e}</pre>
<p>Then enter the printed 6-digit PIN below and click <strong>Approve</strong>.</p>

<form method="post" action="/oauth/authorize" autocomplete="off">
  <input type="hidden" name="session_id" value="{session_id_e}">
  <div class="row">
    <input type="text" name="pin" inputmode="numeric" pattern="[0-9]{{6}}"
           maxlength="6" placeholder="123456" autofocus required>
  </div>
  <div class="row">
    <button class="approve" type="submit" name="action" value="approve">Approve</button>
    <button class="deny" type="submit" name="action" value="deny">Deny</button>
  </div>
</form>

<dl class="meta">
  <dt>Client</dt>
  <dd>{client_name_e} (<code>{client_id_e}</code>)</dd>
  <dt>Redirect URI</dt>
  <dd><code>{redirect_uri_e}</code></dd>
</dl>
</body>
</html>
"#
    )
}

/// Tiny HTML error page. Used both for invalid-request states and
/// PIN failures. `IntoResponse` returns a `(StatusCode, Html<...>)`
/// tuple so axum picks the right shape.
fn error_page(status: StatusCode, title: &str, message: &str) -> axum::response::Response {
    let body = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>gosh-agent — {title}</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         max-width: 38rem; margin: 3rem auto; padding: 0 1rem; line-height: 1.5; }}
  h1 {{ font-size: 1.3rem; color: #c00; }}
</style>
</head>
<body>
<h1>{title}</h1>
<p>{message}</p>
</body>
</html>
"#,
        title = html_escape(title),
        message = html_escape(message),
    );
    (status, Html(body)).into_response()
}

/// Minimal HTML escape — only the characters that matter for body /
/// attribute context. Adequate because all interpolated strings are
/// either operator-controlled (session_id, client_id, redirect_uri,
/// client_name) or static error messages.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_handles_meta_chars() {
        assert_eq!(html_escape(r#"<a href="x">&"#), "&lt;a href=&quot;x&quot;&gt;&amp;");
        assert_eq!(html_escape("plain"), "plain");
        assert_eq!(html_escape("'single'"), "&#39;single&#39;");
    }

    #[test]
    fn consent_html_includes_session_id_and_run_command() {
        let html = consent_html("sess_deadbeef", "client-x", "Claude.ai", "https://claude.ai/cb");
        // Operator must see the session id verbatim — they type it
        // in the terminal command. A regression that broke either
        // the visible session id OR the displayed CLI command shape
        // would silently make the consent flow unusable.
        assert!(html.contains("sess_deadbeef"), "consent page must show session id verbatim");
        assert!(
            html.contains("gosh agent oauth sessions pin sess_deadbeef"),
            "consent page must show the exact CLI command, with the session id substituted",
        );
        assert!(html.contains("Claude.ai"), "client name shown");
        assert!(html.contains("https://claude.ai/cb"), "redirect_uri shown for operator review");
    }

    #[test]
    fn consent_html_escapes_attacker_controlled_client_name() {
        // Defence-in-depth: client_name flows from DCR registration
        // input. A malicious DCR'd client could submit an HTML-laden
        // name; the consent page must escape it so it can't inject
        // an `<input>` overriding the form action / `<script>` etc.
        let html =
            consent_html("sess_x", "client-x", "<script>alert(1)</script>", "https://claude.ai/cb");
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
}
