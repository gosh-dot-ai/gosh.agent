// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::path::PathBuf;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::plugin::config::state_dir;

/// How long a freshly minted access token remains valid. Short enough
/// that a leaked one rotates out fast; long enough that an interactive
/// chat session doesn't see token churn mid-call. RFC 6749 §1.4
/// recommends "short-lived"; 1h is a common point on that spectrum.
pub const ACCESS_TTL: Duration = Duration::hours(1);

/// One in-memory access-token record. Keyed in `TokenStore` by
/// `token_hash` (sha256 of the plaintext) for constant-time lookup.
#[derive(Debug, Clone)]
pub struct AccessToken {
    /// `tok_<8hex>` — the operator-visible handle of the refresh
    /// token this access token was minted from. Cascade-revoke uses
    /// this to evict every active access for a given refresh record.
    pub origin_token_id: String,
    pub client_id: String,
    /// Carried forward from `/authorize` — not consulted by the
    /// Bearer middleware in 7c (every authenticated caller gets the
    /// full `agent_*` + `memory_*` surface). Reserved for future
    /// scope-based gating without forcing a token-store migration.
    #[allow(dead_code)]
    pub scope: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// One persisted refresh-token record. Survives daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshToken {
    /// `tok_<8hex>` — operator handle, surfaced in
    /// `gosh agent oauth tokens list / revoke <id>`. Not a secret.
    pub token_id: String,
    /// `sha256(plain).hex()`. The plaintext (`rt_<base64>`) was
    /// returned to the client at mint time and is gone from the
    /// daemon's memory after that.
    pub token_hash: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

/// On-disk wrapper. The wrapper struct (vs bare `Vec`) leaves room for
/// schema metadata (versioning, last_compaction) without a migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokensFile {
    #[serde(default)]
    pub refresh_tokens: Vec<RefreshToken>,
}

/// Compact admin-listing view: drops `token_hash` so even an admin
/// caller can't reconstruct the on-disk hash from the API surface.
#[derive(Debug, Clone, Serialize)]
pub struct RefreshTokenView {
    pub token_id: String,
    pub client_id: String,
    pub scope: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// How many active (non-expired) access tokens were minted from
    /// this refresh and are currently in memory. Useful for the
    /// "is something still connected?" question without exposing the
    /// access tokens themselves.
    pub active_access_tokens: usize,
}

/// Path to the persisted refresh-token file for `agent_name`.
pub fn tokens_path(agent_name: &str) -> PathBuf {
    state_dir(agent_name).join("oauth").join("tokens.toml")
}

/// Outcome of a successful mint at `/oauth/token`. The plaintext
/// `access_token` and `refresh_token` are the one and only chance the
/// caller has to capture them. `scope` is whatever the auth flow
/// resolved (inherited from the `/authorize` request on the
/// `authorization_code` path; carried over from the prior refresh on
/// the `refresh_token` path).
#[derive(Debug, Clone)]
pub struct MintedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_id: String,
    /// The handler computes `expires_in` from the `ACCESS_TTL`
    /// constant rather than this absolute timestamp; the field is
    /// kept on the struct so callers (admin tools, future audit
    /// logs) can still see when this access token will lapse.
    #[allow(dead_code)]
    pub access_expires_at: DateTime<Utc>,
    pub scope: Option<String>,
}

/// In-memory + on-disk token registry. Wrapped in `tokio::sync::Mutex`
/// on `AppState` because mint/refresh/revoke flush refresh-token state
/// to disk and we don't want concurrent writers stepping on each
/// other's `tokens.toml`.
pub struct TokenStore {
    refresh_path: PathBuf,
    /// Refresh tokens keyed by `token_id` for revoke-by-id; verify
    /// path looks them up by `token_hash` via `find_by_refresh_hash`.
    refresh_by_id: HashMap<String, RefreshToken>,
    /// Access tokens keyed by `sha256(plain).hex()` for O(1) lookup
    /// from the Bearer middleware. Not persisted — daemon restart
    /// drops them all by design.
    access_by_hash: HashMap<String, AccessToken>,
}

#[cfg(test)]
impl TokenStore {
    /// Empty in-memory store with a tempdir-rooted persistence path.
    /// Tests that verify reload behaviour pass a path under
    /// `tempfile::tempdir()`; tests that don't touch disk can pass
    /// `/dev/null/...` since `save()` is gated behind mutating calls.
    pub(crate) fn empty_at(refresh_path: PathBuf) -> Self {
        Self { refresh_path, refresh_by_id: HashMap::new(), access_by_hash: HashMap::new() }
    }
}

impl TokenStore {
    /// Load the store, or create an empty one if `tokens.toml`
    /// doesn't exist yet (first-run case). Errors only on real I/O /
    /// parse failures.
    pub fn load(agent_name: &str) -> Result<Self> {
        let path = tokens_path(agent_name);
        let refresh_by_id = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let parsed: TokensFile = toml::from_str(&text)
                .with_context(|| format!("invalid TOML in {}", path.display()))?;
            parsed.refresh_tokens.into_iter().map(|t| (t.token_id.clone(), t)).collect()
        } else {
            HashMap::new()
        };
        Ok(Self { refresh_path: path, refresh_by_id, access_by_hash: HashMap::new() })
    }

    /// Persist refresh tokens to disk (mode 0600) atomically.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.refresh_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut refresh_tokens: Vec<RefreshToken> = self.refresh_by_id.values().cloned().collect();
        // Stable sort for deterministic file content across saves —
        // makes diffing / reviewing the on-disk state predictable.
        refresh_tokens.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        let file = TokensFile { refresh_tokens };
        let text = toml::to_string_pretty(&file).context("serialising tokens.toml")?;
        let tmp = self.refresh_path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.refresh_path).with_context(|| {
            format!("renaming {} -> {}", tmp.display(), self.refresh_path.display())
        })?;
        Ok(())
    }

    /// Mint a fresh access+refresh pair for `client_id`. Returns the
    /// plaintext values — the only chance to capture them. The new
    /// refresh-token record is persisted; the access token lives in
    /// memory only.
    pub fn mint_pair(&mut self, client_id: &str, scope: Option<String>) -> Result<MintedTokens> {
        let token_id = generate_token_id();
        let access_plain = generate_access_token();
        let refresh_plain = generate_refresh_token();
        let now = Utc::now();
        let expires_at = now + ACCESS_TTL;

        let access_hash = sha256_hex(&access_plain);
        self.access_by_hash.insert(
            access_hash,
            AccessToken {
                origin_token_id: token_id.clone(),
                client_id: client_id.to_string(),
                scope: scope.clone(),
                expires_at,
            },
        );

        let refresh_hash = sha256_hex(&refresh_plain);
        let refresh = RefreshToken {
            token_id: token_id.clone(),
            token_hash: refresh_hash,
            client_id: client_id.to_string(),
            scope: scope.clone(),
            created_at: now,
            last_used_at: None,
        };
        self.refresh_by_id.insert(token_id.clone(), refresh);
        self.save()?;

        Ok(MintedTokens {
            access_token: access_plain,
            refresh_token: refresh_plain,
            token_id,
            access_expires_at: expires_at,
            scope,
        })
    }

    /// Rotate a refresh token: invalidate the presented one, mint a
    /// fresh pair (new access + new refresh) under a new `token_id`
    /// for the same client. RFC 6749 §6 allows refresh rotation;
    /// OAuth 2.1 (BCP draft) recommends it — narrows the leak window
    /// of a compromised refresh.
    ///
    /// Returns `Ok(None)` when the presented refresh is unknown or
    /// for a different client than the one authenticating — the
    /// caller maps that to RFC 6749 §5.2 `invalid_grant`.
    pub fn rotate_refresh(
        &mut self,
        presented: &str,
        authenticated_client_id: &str,
    ) -> Result<Option<MintedTokens>> {
        let Some(old_id) = self.find_by_refresh_hash(presented) else {
            return Ok(None);
        };
        let Some(old) = self.refresh_by_id.get(&old_id).cloned() else {
            return Ok(None);
        };
        if old.client_id != authenticated_client_id {
            // Refresh token belongs to a different client — treat as
            // invalid grant. Don't reveal which client holds it.
            return Ok(None);
        }
        // Drop the old refresh record AND any access tokens minted
        // from it (defence-in-depth: a leaked old refresh + a still-
        // live access shouldn't survive rotation).
        self.refresh_by_id.remove(&old_id);
        self.access_by_hash.retain(|_, a| a.origin_token_id != old_id);

        let minted = self.mint_pair(&old.client_id, old.scope)?;
        Ok(Some(minted))
    }

    /// Verify a Bearer access token. Returns the access record on
    /// hit, `None` on miss / expiry. Looking up by the SHA-256 hash
    /// keeps the verify path constant-time for a fixed-size token.
    pub fn verify_access(&self, presented: &str) -> Option<&AccessToken> {
        let hash = sha256_hex(presented);
        let entry = self.access_by_hash.get(&hash)?;
        if Utc::now() >= entry.expires_at {
            return None;
        }
        Some(entry)
    }

    /// Resolve a presented refresh token to its `token_id` if known.
    /// Used by both rotate and revoke paths so they share lookup
    /// semantics.
    fn find_by_refresh_hash(&self, presented: &str) -> Option<String> {
        let hash = sha256_hex(presented);
        self.refresh_by_id
            .values()
            .find(|t| constant_time_eq(t.token_hash.as_bytes(), hash.as_bytes()))
            .map(|t| t.token_id.clone())
    }

    /// Update `last_used_at` on the refresh record after a successful
    /// rotation. Display-only field.
    pub fn touch(&mut self, token_id: &str) -> Result<()> {
        if let Some(t) = self.refresh_by_id.get_mut(token_id) {
            t.last_used_at = Some(Utc::now());
            self.save()?;
        }
        Ok(())
    }

    /// Revoke a refresh token by its operator-visible `token_id` and
    /// cascade to every active access token minted from it. Returns
    /// `true` if the refresh existed and was removed, `false` if no
    /// such id (idempotent).
    pub fn revoke_by_id(&mut self, token_id: &str) -> Result<bool> {
        let removed = self.refresh_by_id.remove(token_id).is_some();
        // Cascade unconditionally — even if the refresh is already
        // gone, evict any orphaned access tokens that point at this
        // id (shouldn't happen under normal flow, but cheap insurance
        // against state-machine bugs).
        self.access_by_hash.retain(|_, a| a.origin_token_id != token_id);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Revoke a refresh token by presented plaintext. Used by
    /// `/oauth/revoke` (RFC 7009). Idempotent: returns `false` if no
    /// match. Cascades access-token eviction same as `revoke_by_id`.
    pub fn revoke_refresh_plain(&mut self, presented: &str) -> Result<bool> {
        let Some(id) = self.find_by_refresh_hash(presented) else {
            return Ok(false);
        };
        self.revoke_by_id(&id)
    }

    /// Revoke an access token by presented plaintext. Used by
    /// `/oauth/revoke` (RFC 7009). Removes the in-memory record only;
    /// the parent refresh stays alive (RFC 7009 §2.1: revoking an
    /// access token does not revoke its refresh).
    pub fn revoke_access_plain(&mut self, presented: &str) -> bool {
        let hash = sha256_hex(presented);
        self.access_by_hash.remove(&hash).is_some()
    }

    /// Cascade-revoke: drop every refresh + access for `client_id`.
    /// Called when the client itself is deleted via
    /// `DELETE /admin/oauth/clients/<id>` so a leaked access token
    /// from a deleted client stops working immediately rather than
    /// waiting out the access-token TTL.
    pub fn revoke_by_client(&mut self, client_id: &str) -> Result<usize> {
        let before = self.refresh_by_id.len();
        self.refresh_by_id.retain(|_, t| t.client_id != client_id);
        let removed = before - self.refresh_by_id.len();
        self.access_by_hash.retain(|_, a| a.client_id != client_id);
        if removed > 0 {
            self.save()?;
        }
        Ok(removed)
    }

    /// Snapshot of all refresh-token records for the admin listing.
    /// Hashes are stripped; the operator-visible payload is the
    /// `token_id` plus contextual metadata.
    pub fn list_refresh(&self) -> Vec<RefreshTokenView> {
        let now = Utc::now();
        let mut out: Vec<RefreshTokenView> = self
            .refresh_by_id
            .values()
            .map(|t| {
                let active = self
                    .access_by_hash
                    .values()
                    .filter(|a| a.origin_token_id == t.token_id && now < a.expires_at)
                    .count();
                RefreshTokenView {
                    token_id: t.token_id.clone(),
                    client_id: t.client_id.clone(),
                    scope: t.scope.clone(),
                    created_at: t.created_at.to_rfc3339(),
                    last_used_at: t.last_used_at.map(|d| d.to_rfc3339()),
                    active_access_tokens: active,
                }
            })
            .collect();
        out.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        out
    }

    /// Evict expired access tokens. Sweep task calls this every 60s
    /// in `serve()`. Refresh tokens have no TTL — they live until
    /// explicitly revoked. Returns the number of access tokens
    /// removed for observability.
    pub fn sweep_access(&mut self) -> usize {
        let now = Utc::now();
        let before = self.access_by_hash.len();
        self.access_by_hash.retain(|_, a| now < a.expires_at);
        before - self.access_by_hash.len()
    }
}

/// `tok_<8hex>` operator handle. Random per-call.
fn generate_token_id() -> String {
    let mut bytes = [0u8; 4];
    OsRng.fill_bytes(&mut bytes);
    format!("tok_{}", hex_lower(&bytes))
}

/// `at_<base64>` access token. 32 random bytes URL-safe-b64 (no pad)
/// so it survives form-encoded transport without escaping.
fn generate_access_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("at_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// `rt_<base64>` refresh token, same shape as access.
fn generate_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("rt_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    hex_lower(&digest)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

    fn store() -> (TokenStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("tokens.toml");
        (TokenStore::empty_at(path), dir)
    }

    #[test]
    fn token_id_format_is_short_and_hex() {
        let id = generate_token_id();
        assert!(id.starts_with("tok_"));
        assert_eq!(id.len(), 4 + 8);
        assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn access_and_refresh_have_distinct_prefixes() {
        let a = generate_access_token();
        let r = generate_refresh_token();
        assert!(a.starts_with("at_"));
        assert!(r.starts_with("rt_"));
        assert_ne!(a, r);
    }

    #[test]
    fn mint_returns_plaintext_and_persists_only_hash() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", Some("read".into())).unwrap();
        assert!(m.access_token.starts_with("at_"));
        assert!(m.refresh_token.starts_with("rt_"));
        assert!(m.token_id.starts_with("tok_"));
        assert!(m.access_expires_at > Utc::now());

        // Refresh token plaintext must NOT appear on disk anywhere.
        let on_disk = std::fs::read_to_string(&s.refresh_path).unwrap();
        assert!(
            !on_disk.contains(&m.refresh_token),
            "refresh plaintext leaked into tokens.toml: {on_disk}",
        );
        assert!(
            !on_disk.contains(&m.access_token),
            "access plaintext leaked into tokens.toml: {on_disk}",
        );
    }

    #[test]
    fn verify_access_round_trip_and_unknown_token_misses() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        let entry = s.verify_access(&m.access_token).expect("just minted");
        assert_eq!(entry.client_id, "client-x");
        assert_eq!(entry.origin_token_id, m.token_id);
        assert!(s.verify_access("at_unknown_value").is_none());
    }

    #[test]
    fn verify_access_returns_none_for_expired_token() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        let hash = sha256_hex(&m.access_token);
        s.access_by_hash.get_mut(&hash).unwrap().expires_at = Utc::now() - Duration::seconds(1);
        assert!(s.verify_access(&m.access_token).is_none());
    }

    #[test]
    fn rotate_refresh_invalidates_old_and_issues_new_pair() {
        let (mut s, _dir) = store();
        let first = s.mint_pair("client-x", Some("scope".into())).unwrap();
        let rotated = s.rotate_refresh(&first.refresh_token, "client-x").unwrap().unwrap();

        // New plaintext is different.
        assert_ne!(rotated.refresh_token, first.refresh_token);
        assert_ne!(rotated.access_token, first.access_token);
        assert_ne!(rotated.token_id, first.token_id);

        // Old refresh no longer rotates.
        assert!(s.rotate_refresh(&first.refresh_token, "client-x").unwrap().is_none());

        // Old access token also evicted (cascade).
        assert!(s.verify_access(&first.access_token).is_none());

        // New access token works.
        assert!(s.verify_access(&rotated.access_token).is_some());
    }

    #[test]
    fn rotate_refresh_rejects_when_authenticating_client_does_not_match() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-a", None).unwrap();
        // Different client tries to rotate — must be a miss without
        // revealing the actual owner.
        let res = s.rotate_refresh(&m.refresh_token, "client-b").unwrap();
        assert!(res.is_none());
        // Old refresh stays valid for its rightful owner.
        assert!(s.rotate_refresh(&m.refresh_token, "client-a").unwrap().is_some());
    }

    #[test]
    fn revoke_by_id_drops_refresh_and_cascades_access_tokens() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        assert!(s.verify_access(&m.access_token).is_some());

        assert!(s.revoke_by_id(&m.token_id).unwrap());
        // Refresh gone.
        assert!(s.rotate_refresh(&m.refresh_token, "client-x").unwrap().is_none());
        // Access cascaded.
        assert!(s.verify_access(&m.access_token).is_none());
        // Idempotent.
        assert!(!s.revoke_by_id(&m.token_id).unwrap());
    }

    #[test]
    fn revoke_refresh_plain_round_trip_and_idempotent() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        assert!(s.revoke_refresh_plain(&m.refresh_token).unwrap());
        assert!(!s.revoke_refresh_plain(&m.refresh_token).unwrap());
        assert!(!s.revoke_refresh_plain("rt_unknown").unwrap());
    }

    #[test]
    fn revoke_access_plain_evicts_only_access_keeps_refresh_alive() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        assert!(s.revoke_access_plain(&m.access_token));
        assert!(s.verify_access(&m.access_token).is_none());
        // Refresh still rotates — RFC 7009 §2.1: revoking access
        // does not revoke its refresh.
        assert!(s.rotate_refresh(&m.refresh_token, "client-x").unwrap().is_some());
    }

    #[test]
    fn revoke_by_client_drops_every_token_for_that_client() {
        let (mut s, _dir) = store();
        let a1 = s.mint_pair("client-a", None).unwrap();
        let a2 = s.mint_pair("client-a", None).unwrap();
        let b = s.mint_pair("client-b", None).unwrap();

        let removed = s.revoke_by_client("client-a").unwrap();
        assert_eq!(removed, 2);
        assert!(s.verify_access(&a1.access_token).is_none());
        assert!(s.verify_access(&a2.access_token).is_none());
        assert!(s.verify_access(&b.access_token).is_some());
    }

    #[test]
    fn list_refresh_strips_hashes_and_counts_active_access() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", Some("audit".into())).unwrap();
        let view = s.list_refresh();
        assert_eq!(view.len(), 1);
        let v = &view[0];
        assert_eq!(v.token_id, m.token_id);
        assert_eq!(v.client_id, "client-x");
        assert_eq!(v.scope.as_deref(), Some("audit"));
        assert_eq!(v.active_access_tokens, 1);
        // No public field exposes the hash; defence-in-depth check
        // by serialising and grepping.
        let body = serde_json::to_string(v).unwrap();
        assert!(!body.contains("token_hash"), "list output leaks token_hash: {body}");
    }

    #[test]
    fn sweep_access_evicts_expired_tokens_only() {
        let (mut s, _dir) = store();
        let live = s.mint_pair("client-x", None).unwrap();
        let stale = s.mint_pair("client-y", None).unwrap();
        let stale_hash = sha256_hex(&stale.access_token);
        s.access_by_hash.get_mut(&stale_hash).unwrap().expires_at =
            Utc::now() - Duration::seconds(1);

        let removed = s.sweep_access();
        assert_eq!(removed, 1);
        assert!(s.verify_access(&live.access_token).is_some());
        assert!(s.verify_access(&stale.access_token).is_none());
    }

    #[test]
    fn touch_updates_last_used_at_and_persists() {
        let (mut s, _dir) = store();
        let m = s.mint_pair("client-x", None).unwrap();
        assert!(s.refresh_by_id.get(&m.token_id).unwrap().last_used_at.is_none());

        s.touch(&m.token_id).unwrap();
        let after = s.refresh_by_id.get(&m.token_id).unwrap();
        assert!(after.last_used_at.is_some());

        // Persistence: touch should have flushed.
        let body = std::fs::read_to_string(&s.refresh_path).unwrap();
        assert!(body.contains("last_used_at"), "touch did not persist: {body}");
    }

    #[test]
    fn refresh_tokens_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("tokens.toml");
        let m = {
            let mut s = TokenStore::empty_at(path.clone());
            s.mint_pair("client-x", Some("read".into())).unwrap()
        };

        let parsed: TokensFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.refresh_tokens.len(), 1);
        assert_eq!(parsed.refresh_tokens[0].token_id, m.token_id);
        // Hash matches what `verify_refresh` would compute.
        assert_eq!(parsed.refresh_tokens[0].token_hash, sha256_hex(&m.refresh_token));
    }

    #[cfg(unix)]
    #[test]
    fn tokens_file_written_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (mut s, _dir) = store();
        s.mint_pair("client-x", None).unwrap();
        let mode = std::fs::metadata(&s.refresh_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tokens.toml must be 0600, got {mode:o}");
    }

    #[test]
    fn constant_time_eq_basic_sanity() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
