// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::HashMap;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use base64::Engine;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use serde::Serialize;

/// Total time a `Pending` session has to receive PIN approval before
/// the sweep task evicts it. Generous so an operator pasting a PIN
/// from terminal into browser doesn't race the clock.
pub const SESSION_TTL: Duration = Duration::minutes(10);

/// Time a freshly issued PIN remains valid. Re-issuing via
/// `gosh agent oauth sessions pin <id>` invalidates the prior PIN
/// and starts a new window.
pub const PIN_TTL: Duration = Duration::minutes(5);

/// Time `/oauth/token` has to exchange a freshly minted authorisation
/// `code` before it expires. RFC 6749 §4.1.2 recommends "short
/// (e.g. 10 minutes)"; we go shorter — the path between `Approve`
/// click and Claude.ai's POST is sub-second in normal flow, so a
/// minute is plenty. Tightening this shrinks the window for a
/// stolen-code attack.
pub const CODE_TTL: Duration = Duration::seconds(60);

/// Session status — drives both the consent UI and `oauth sessions
/// list` rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Pending,
    Approved,
    Denied,
    /// Reserved for 7c: set when `/oauth/token` exchanges the code.
    /// Sweeping happens regardless of status — `Consumed` just makes
    /// the listing meaningful between exchange and sweep.
    #[allow(dead_code)]
    Consumed,
}

/// Operator-issued PIN tied to a specific session. One-time use:
/// successful approval marks `used=true` and the value can't be
/// reused even if the session is somehow re-Pending'd.
#[derive(Debug, Clone)]
pub struct PinInfo {
    pub pin: String,
    pub expires_at: DateTime<Utc>,
    pub used: bool,
}

impl PinInfo {
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        !self.used && now < self.expires_at
    }
}

/// One pending authorisation session.
///
/// Several fields (`code_challenge`, `code_challenge_method`,
/// `scope`) are captured here in 7b but read only in 7c's
/// `/oauth/token` handler — that's where PKCE verification happens.
/// `#[allow(dead_code)]` keeps the lint quiet without lying about
/// the lifecycle.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AuthSession {
    pub session_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    /// PKCE — RFC 7636. We accept only `S256`; `plain` is rejected
    /// at session-create time so this field is always literally
    /// `"S256"`. Stored anyway for echo-back / logging.
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: SessionStatus,
    pub pin: Option<PinInfo>,
    /// Authorisation code minted on PIN-approve. Cleared by 7c's
    /// `/oauth/token` after exchange.
    pub authorization_code: Option<String>,
    pub code_expires_at: Option<DateTime<Utc>>,
}

/// Compact display struct for `/admin/oauth/sessions` — drops the
/// `pin` and `authorization_code` so admin listings can't leak
/// either. Status + identifying metadata only.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub status: SessionStatus,
    pub created_at: String,
    pub expires_at: String,
    pub has_pending_pin: bool,
}

impl From<&AuthSession> for SessionView {
    fn from(s: &AuthSession) -> Self {
        let now = Utc::now();
        Self {
            session_id: s.session_id.clone(),
            client_id: s.client_id.clone(),
            redirect_uri: s.redirect_uri.clone(),
            status: s.status,
            created_at: s.created_at.to_rfc3339(),
            expires_at: s.expires_at.to_rfc3339(),
            has_pending_pin: s.pin.as_ref().map(|p| p.is_active(now)).unwrap_or(false),
        }
    }
}

/// Inputs from the `GET /oauth/authorize` query string. The handler
/// validates these against the registered client and PKCE rules
/// before calling `SessionStore::create`.
#[derive(Debug, Clone)]
pub struct AuthorizeRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: Option<String>,
}

/// Outcome of a successful PIN-approve.
#[derive(Debug, Clone)]
pub struct ApproveOutcome {
    pub authorization_code: String,
    pub redirect_uri: String,
    pub state: Option<String>,
}

/// Why approval was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveError {
    UnknownSession,
    Expired,
    AlreadyDecided,
    NoPin,
    PinExpired,
    PinMismatch,
}

/// Why a `/oauth/token` `authorization_code` exchange was rejected.
/// Every variant maps to RFC 6749 §5.2 `invalid_grant` at the wire —
/// the handler collapses them rather than leaking which condition
/// tripped (defence-in-depth: an attacker probing with random codes
/// shouldn't learn anything from the error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeExchangeError {
    /// No session has this code, or the session was already consumed,
    /// denied, or expired out from under us.
    UnknownCode,
    /// Code was issued but its 60-second window elapsed before this
    /// `/oauth/token` POST.
    CodeExpired,
    /// The authenticating client_id is not the one the code was
    /// minted for. Possible attack surface (one client trying to
    /// burn another's code).
    ClientMismatch,
    /// `redirect_uri` in the token request doesn't match the one
    /// the session was created with — RFC 6749 §4.1.3 requires
    /// exact match.
    RedirectUriMismatch,
    /// PKCE verification failed: `sha256(code_verifier)` (base64-url,
    /// no pad) didn't match the `code_challenge` captured at
    /// `/oauth/authorize`.
    PkceMismatch,
}

/// Outcome of a successful code exchange. The session is marked
/// `Consumed` as a side effect; the handler then mints tokens via
/// `TokenStore::mint_pair(client_id, scope)`.
#[derive(Debug, Clone)]
pub struct ConsumedCode {
    pub client_id: String,
    pub scope: Option<String>,
}

/// In-memory session registry. One per daemon process; lives behind
/// a `tokio::sync::Mutex` on `AppState`.
#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, AuthSession>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a fresh `Pending` session and return the
    /// auto-allocated `session_id`. The session_id is short
    /// (`sess_<8 hex>`) and meant to be human-typable from the
    /// consent page into the operator's terminal.
    pub fn create(&mut self, req: AuthorizeRequest) -> AuthSession {
        let session_id = generate_session_id();
        let now = Utc::now();
        let session = AuthSession {
            session_id: session_id.clone(),
            client_id: req.client_id,
            redirect_uri: req.redirect_uri,
            state: req.state,
            code_challenge: req.code_challenge,
            code_challenge_method: req.code_challenge_method,
            scope: req.scope,
            created_at: now,
            expires_at: now + SESSION_TTL,
            status: SessionStatus::Pending,
            pin: None,
            authorization_code: None,
            code_expires_at: None,
        };
        self.sessions.insert(session_id, session.clone());
        session
    }

    /// Lookup by session id. Returns `None` if absent or if the
    /// session has expired (caller treats both the same way).
    /// Used by the unit tests; 7c's `/oauth/token` will be the
    /// production caller (looking up the session behind a code).
    #[allow(dead_code)]
    pub fn find(&self, session_id: &str) -> Option<&AuthSession> {
        let s = self.sessions.get(session_id)?;
        if Utc::now() >= s.expires_at {
            return None;
        }
        Some(s)
    }

    /// Snapshot for admin listing.
    pub fn list(&self) -> Vec<SessionView> {
        self.sessions.values().map(SessionView::from).collect()
    }

    /// Issue a fresh PIN for a session. Returns `None` if the
    /// session doesn't exist or has expired/been decided. Re-issuing
    /// invalidates any prior PIN by overwriting.
    pub fn issue_pin(&mut self, session_id: &str) -> Option<String> {
        let now = Utc::now();
        let s = self.sessions.get_mut(session_id)?;
        if now >= s.expires_at || s.status != SessionStatus::Pending {
            return None;
        }
        let pin = generate_pin();
        s.pin = Some(PinInfo { pin: pin.clone(), expires_at: now + PIN_TTL, used: false });
        Some(pin)
    }

    /// Verify the PIN and, on match, mint an authorisation code,
    /// transition the session to `Approved`, and return the
    /// outcome. Returns a typed error for every failure reason so
    /// the consent page can render an actionable message without
    /// leaking which condition tripped (defence-in-depth: the
    /// caller can collapse all errors to a generic "PIN didn't
    /// match" message).
    pub fn approve(
        &mut self,
        session_id: &str,
        provided_pin: &str,
    ) -> Result<ApproveOutcome, ApproveError> {
        let now = Utc::now();
        let s = self.sessions.get_mut(session_id).ok_or(ApproveError::UnknownSession)?;
        if now >= s.expires_at {
            return Err(ApproveError::Expired);
        }
        if s.status != SessionStatus::Pending {
            return Err(ApproveError::AlreadyDecided);
        }
        let pin = s.pin.as_mut().ok_or(ApproveError::NoPin)?;
        if pin.used || now >= pin.expires_at {
            return Err(ApproveError::PinExpired);
        }
        if !constant_time_eq(provided_pin.as_bytes(), pin.pin.as_bytes()) {
            return Err(ApproveError::PinMismatch);
        }
        pin.used = true;
        let code = generate_authorization_code();
        s.authorization_code = Some(code.clone());
        s.code_expires_at = Some(now + CODE_TTL);
        s.status = SessionStatus::Approved;
        Ok(ApproveOutcome {
            authorization_code: code,
            redirect_uri: s.redirect_uri.clone(),
            state: s.state.clone(),
        })
    }

    /// Mark a session denied — explicit operator decline through
    /// the `/admin/oauth/sessions/<id>` DELETE or future Deny
    /// button on the consent page. Idempotent: re-denying or
    /// dropping a non-existent session is a no-op.
    #[allow(dead_code)]
    pub fn deny(&mut self, session_id: &str) -> bool {
        if let Some(s) = self.sessions.get_mut(session_id) {
            if s.status == SessionStatus::Pending {
                s.status = SessionStatus::Denied;
                return true;
            }
        }
        false
    }

    /// Hard-remove a session by id. Used by the admin
    /// `DELETE /admin/oauth/sessions/<id>` for explicit cleanup.
    /// Returns `true` if it existed and was removed.
    pub fn drop_session(&mut self, session_id: &str) -> bool {
        self.sessions.remove(session_id).is_some()
    }

    /// Verify a presented authorisation `code` against an Approved
    /// session and consume it. Performs the RFC 6749 §4.1.3 checks
    /// (redirect_uri match, code expiry, single-use) plus PKCE
    /// S256 verification (RFC 7636 §4.6).
    ///
    /// On success the session transitions to `Consumed` so the same
    /// code can't be exchanged twice (RFC 6749 §4.1.2: "MUST be
    /// short lived and single-use").
    pub fn consume_authorization_code(
        &mut self,
        code: &str,
        authenticated_client_id: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<ConsumedCode, CodeExchangeError> {
        let now = Utc::now();
        // Find a session whose authorisation_code matches via
        // constant-time comparison. Linear scan, but session count
        // is bounded by `SESSION_TTL` (10 min) — production traffic
        // won't grow this beyond a handful at a time.
        let session_id = self
            .sessions
            .values()
            .find(|s| {
                s.authorization_code
                    .as_deref()
                    .map(|c| constant_time_eq(c.as_bytes(), code.as_bytes()))
                    .unwrap_or(false)
            })
            .map(|s| s.session_id.clone())
            .ok_or(CodeExchangeError::UnknownCode)?;

        let s = self.sessions.get_mut(&session_id).ok_or(CodeExchangeError::UnknownCode)?;

        // Must be Approved (not Pending — code was never minted; not
        // Consumed — already exchanged once; not Denied — operator
        // rejected).
        if s.status != SessionStatus::Approved {
            return Err(CodeExchangeError::UnknownCode);
        }
        if let Some(exp) = s.code_expires_at {
            if now >= exp {
                return Err(CodeExchangeError::CodeExpired);
            }
        } else {
            return Err(CodeExchangeError::UnknownCode);
        }
        if s.client_id != authenticated_client_id {
            return Err(CodeExchangeError::ClientMismatch);
        }
        // RFC 6749 §4.1.3: `redirect_uri` MUST be identical (byte-
        // for-byte) to the one in the original `/authorize` request.
        if s.redirect_uri != redirect_uri {
            return Err(CodeExchangeError::RedirectUriMismatch);
        }
        // PKCE S256 (RFC 7636 §4.6):
        //     base64url-no-pad(sha256(code_verifier)) == code_challenge
        if !verify_pkce_s256(code_verifier, &s.code_challenge) {
            return Err(CodeExchangeError::PkceMismatch);
        }

        s.status = SessionStatus::Consumed;
        s.authorization_code = None;
        s.code_expires_at = None;
        Ok(ConsumedCode { client_id: s.client_id.clone(), scope: s.scope.clone() })
    }

    /// Evict expired sessions. Sweep task calls this on an
    /// interval. Returns the number removed for observability.
    pub fn sweep(&mut self) -> usize {
        let now = Utc::now();
        let before = self.sessions.len();
        self.sessions.retain(|_, s| now < s.expires_at);
        before - self.sessions.len()
    }
}

/// 8-hex `sess_` prefix. Random per-call, low collision probability
/// (2^32 space; sweep keeps the in-flight count bounded by SESSION_TTL).
fn generate_session_id() -> String {
    let mut bytes = [0u8; 4];
    OsRng.fill_bytes(&mut bytes);
    format!("sess_{}", hex_lower(&bytes))
}

/// 6-digit PIN, randomly chosen across the full [0, 999_999]
/// range. Each digit position has uniform distribution because the
/// modulus 1_000_000 fits cleanly into a u32 sample space (no
/// modulo bias for this magnitude).
fn generate_pin() -> String {
    let mut buf = [0u8; 4];
    OsRng.fill_bytes(&mut buf);
    let n = u32::from_le_bytes(buf) % 1_000_000;
    format!("{n:06}")
}

/// 32-byte URL-safe base64 authorisation code. Single-use; verified
/// at `/oauth/token` via straight equality + status check (7c).
fn generate_authorization_code() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// PKCE S256 verifier per RFC 7636 §4.6.
///
/// `code_verifier` is the high-entropy string the client kept private
/// during `/oauth/authorize`; `expected_challenge` is the base64-url
/// (no padding) SHA-256 we stored at session-create time. Returns
/// `true` when `base64url(sha256(code_verifier)) == expected_challenge`
/// under constant-time comparison.
fn verify_pkce_s256(code_verifier: &str, expected_challenge: &str) -> bool {
    use sha2::Digest;
    use sha2::Sha256;
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let digest = hasher.finalize();
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_slice());
    constant_time_eq(computed.as_bytes(), expected_challenge.as_bytes())
}

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
    use super::*;

    fn req() -> AuthorizeRequest {
        AuthorizeRequest {
            client_id: "client-x".to_string(),
            redirect_uri: "https://claude.ai/cb".to_string(),
            state: Some("s1".to_string()),
            code_challenge: "abc".to_string(),
            code_challenge_method: "S256".to_string(),
            scope: None,
        }
    }

    #[test]
    fn session_id_format_is_short_and_hex() {
        let id = generate_session_id();
        assert!(id.starts_with("sess_"));
        assert_eq!(id.len(), 5 + 8); // "sess_" + 8 hex chars
        assert!(id[5..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn pin_is_six_digit_zero_padded() {
        for _ in 0..50 {
            let p = generate_pin();
            assert_eq!(p.len(), 6, "pin must be exactly 6 chars: {p}");
            assert!(p.chars().all(|c| c.is_ascii_digit()), "non-digit in pin: {p}");
        }
    }

    #[test]
    fn create_session_returns_pending_with_expiry() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        assert_eq!(s.status, SessionStatus::Pending);
        assert!(s.pin.is_none());
        assert!(s.authorization_code.is_none());
        assert!(s.expires_at > s.created_at);
        assert_eq!(store.find(&s.session_id).unwrap().session_id, s.session_id);
    }

    #[test]
    fn issue_pin_then_approve_mints_code_and_marks_used() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let pin = store.issue_pin(&s.session_id).expect("session pending");
        let outcome = store.approve(&s.session_id, &pin).expect("approve");
        assert!(!outcome.authorization_code.is_empty());
        assert_eq!(outcome.redirect_uri, "https://claude.ai/cb");
        assert_eq!(outcome.state.as_deref(), Some("s1"));
        let after = store.find(&s.session_id).unwrap();
        assert_eq!(after.status, SessionStatus::Approved);
        assert!(after.pin.as_ref().unwrap().used);
        assert!(after.authorization_code.is_some());
    }

    #[test]
    fn approve_rejects_pin_mismatch_without_consuming_pin() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let _pin = store.issue_pin(&s.session_id).unwrap();
        let err = store.approve(&s.session_id, "000000").unwrap_err();
        assert_eq!(err, ApproveError::PinMismatch);
        // PIN remains valid for a retry within TTL.
        let after = store.find(&s.session_id).unwrap();
        assert!(!after.pin.as_ref().unwrap().used);
        assert_eq!(after.status, SessionStatus::Pending);
    }

    #[test]
    fn approve_rejects_when_no_pin_issued_yet() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let err = store.approve(&s.session_id, "123456").unwrap_err();
        assert_eq!(err, ApproveError::NoPin);
    }

    #[test]
    fn approve_rejects_after_pin_already_consumed() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let pin = store.issue_pin(&s.session_id).unwrap();
        let _ = store.approve(&s.session_id, &pin).unwrap();
        // Second approve attempt — session is now Approved.
        let err = store.approve(&s.session_id, &pin).unwrap_err();
        assert_eq!(err, ApproveError::AlreadyDecided);
    }

    #[test]
    fn issue_pin_returns_none_when_session_not_pending() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let pin = store.issue_pin(&s.session_id).unwrap();
        store.approve(&s.session_id, &pin).unwrap();
        // Session is Approved → issue_pin must refuse.
        assert!(store.issue_pin(&s.session_id).is_none());
    }

    #[test]
    fn re_issue_pin_invalidates_prior_pin() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let p1 = store.issue_pin(&s.session_id).unwrap();
        let p2 = store.issue_pin(&s.session_id).unwrap();
        // Old PIN must not approve.
        let err = store.approve(&s.session_id, &p1);
        // Either Mismatch (probably) or, on the off chance both
        // PINs collided to the same value, AlreadyDecided after p2
        // approves. The contract is "old PIN doesn't unlock the
        // session". Test the negative.
        if p1 != p2 {
            assert_eq!(err.unwrap_err(), ApproveError::PinMismatch);
        }
        // New PIN works.
        store.approve(&s.session_id, &p2).expect("new PIN approves");
    }

    #[test]
    fn drop_removes_session_idempotent() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        assert!(store.drop_session(&s.session_id));
        assert!(!store.drop_session(&s.session_id));
        assert!(store.find(&s.session_id).is_none());
    }

    #[test]
    fn deny_marks_session_without_removing() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        assert!(store.deny(&s.session_id));
        let after = store.find(&s.session_id).unwrap();
        assert_eq!(after.status, SessionStatus::Denied);
        // Re-deny is a no-op — session is no longer Pending.
        assert!(!store.deny(&s.session_id));
    }

    #[test]
    fn sweep_evicts_expired_sessions() {
        let mut store = SessionStore::new();
        let mut s = store.create(req());
        // Force expiry by rewriting timestamps in-place.
        s.expires_at = Utc::now() - Duration::seconds(1);
        store.sessions.insert(s.session_id.clone(), s.clone());
        let removed = store.sweep();
        assert_eq!(removed, 1);
        assert!(store.find(&s.session_id).is_none());
    }

    #[test]
    fn list_includes_all_sessions_with_compact_view() {
        let mut store = SessionStore::new();
        let s = store.create(req());
        let view = store.list();
        assert_eq!(view.len(), 1);
        let v = &view[0];
        assert_eq!(v.session_id, s.session_id);
        assert_eq!(v.status, SessionStatus::Pending);
        assert!(!v.has_pending_pin);
        // After issuing PIN, view reflects it without leaking the
        // value itself.
        store.issue_pin(&s.session_id).unwrap();
        let view = store.list();
        assert!(view[0].has_pending_pin);
    }

    #[test]
    fn pin_info_is_active_respects_used_and_expiry() {
        let now = Utc::now();
        let p =
            PinInfo { pin: "123456".into(), expires_at: now + Duration::minutes(5), used: false };
        assert!(p.is_active(now));
        let used = PinInfo { pin: p.pin.clone(), expires_at: p.expires_at, used: true };
        assert!(!used.is_active(now));
        let expired =
            PinInfo { pin: p.pin.clone(), expires_at: now - Duration::seconds(1), used: false };
        assert!(!expired.is_active(now));
    }

    /// Mirror RFC 7636 §4.6 with a known fixture so future
    /// refactors can't subtly change the encoding.
    #[test]
    fn verify_pkce_s256_known_vector() {
        // verifier = "abc" → sha256 = ba7816bf... → base64url-no-pad =
        //     "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0".
        let verifier = "abc";
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        assert!(verify_pkce_s256(verifier, challenge));
        assert!(!verify_pkce_s256("xyz", challenge));
        assert!(!verify_pkce_s256(verifier, "different-challenge"));
    }

    /// Helper for the consume tests: drives the session through
    /// authorize → PIN → approve so we have a real `code` to exchange.
    fn pump_to_approved(store: &mut SessionStore, challenge: &str) -> (String, String) {
        let mut r = req();
        r.code_challenge = challenge.to_string();
        let s = store.create(r);
        let pin = store.issue_pin(&s.session_id).unwrap();
        let outcome = store.approve(&s.session_id, &pin).unwrap();
        (s.session_id, outcome.authorization_code)
    }

    #[test]
    fn consume_code_happy_path_marks_session_consumed() {
        let mut store = SessionStore::new();
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        let (sid, code) = pump_to_approved(&mut store, challenge);

        let consumed = store
            .consume_authorization_code(&code, "client-x", "https://claude.ai/cb", "abc")
            .expect("happy path");
        assert_eq!(consumed.client_id, "client-x");

        // Status is now Consumed; the same code can't be exchanged
        // again (single-use per RFC 6749 §4.1.2).
        let err = store
            .consume_authorization_code(&code, "client-x", "https://claude.ai/cb", "abc")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::UnknownCode);

        // Status reflects consume.
        let after = store.sessions.get(&sid).unwrap();
        assert_eq!(after.status, SessionStatus::Consumed);
        assert!(after.authorization_code.is_none());
    }

    #[test]
    fn consume_code_rejects_pkce_mismatch() {
        let mut store = SessionStore::new();
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        let (_sid, code) = pump_to_approved(&mut store, challenge);

        let err = store
            .consume_authorization_code(&code, "client-x", "https://claude.ai/cb", "wrong-verifier")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::PkceMismatch);
    }

    #[test]
    fn consume_code_rejects_redirect_uri_mismatch() {
        let mut store = SessionStore::new();
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        let (_sid, code) = pump_to_approved(&mut store, challenge);

        let err = store
            .consume_authorization_code(&code, "client-x", "https://attacker.example/cb", "abc")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::RedirectUriMismatch);
    }

    #[test]
    fn consume_code_rejects_client_id_mismatch() {
        let mut store = SessionStore::new();
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        let (_sid, code) = pump_to_approved(&mut store, challenge);

        let err = store
            .consume_authorization_code(&code, "different-client", "https://claude.ai/cb", "abc")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::ClientMismatch);
    }

    #[test]
    fn consume_code_rejects_expired_code() {
        let mut store = SessionStore::new();
        let challenge = "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0";
        let (sid, code) = pump_to_approved(&mut store, challenge);
        // Force code expiry.
        store.sessions.get_mut(&sid).unwrap().code_expires_at =
            Some(Utc::now() - Duration::seconds(1));

        let err = store
            .consume_authorization_code(&code, "client-x", "https://claude.ai/cb", "abc")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::CodeExpired);
    }

    #[test]
    fn consume_code_unknown_for_random_string() {
        let mut store = SessionStore::new();
        // Even with no Approved sessions, a random code returns
        // UnknownCode rather than panicking.
        let err = store
            .consume_authorization_code("made-up-code", "client-x", "https://claude.ai/cb", "abc")
            .unwrap_err();
        assert_eq!(err, CodeExchangeError::UnknownCode);
    }
}
