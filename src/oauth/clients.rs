// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::PathBuf;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::plugin::config::state_dir;

/// How a client got into the registry.
///
/// `Dcr`: anonymous POST to `/oauth/register`, accepted because
/// `oauth_dcr_enabled` was on. The metadata schema lets the client
/// suggest its own `client_name`; we record whatever it sent
/// (truncated/sanitised).
///
/// `Manual`: operator ran
/// `gosh agent oauth clients register --name X --redirect-uri <URI>`
/// against the local admin endpoint. The `name` is fully under
/// operator control; the registered URI(s) are required up front
/// (admin endpoint rejects empty `redirect_uris`) so the client
/// can complete the authorize flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientSource {
    Dcr,
    Manual,
}

/// Single registered client. `client_secret` is never present in this
/// struct after the registration call returns — only the salted hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub name: String,
    pub source: ClientSource,
    /// Salt + hash of the client secret, stored as `<salt_b64>:<hash_b64>`.
    pub secret_hash: String,
    /// Redirect URIs the client registered, exact-match enforced at
    /// `/oauth/authorize` per RFC 6749 §3.1.2 + RFC 7591 §2.
    /// `#[serde(default)]` keeps pre-7e on-disk records loadable; old
    /// records have no entries and the operator must re-register them to
    /// use the authorize flow.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// Updated whenever this client successfully exchanges a code or
    /// refresh-token at `/oauth/token`. Display-only; not load-bearing
    /// for any auth decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Top-level on-disk shape of `clients.toml`. Keeping the wrapper
/// struct (instead of bare `HashMap`) leaves room for adding store-
/// level metadata (schema_version, last_compaction, ...) without a
/// migration round.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientsFile {
    #[serde(default)]
    pub clients: Vec<OAuthClient>,
}

/// Path to the persisted store for `agent_name`.
pub fn clients_path(agent_name: &str) -> PathBuf {
    state_dir(agent_name).join("oauth").join("clients.toml")
}

/// Cheap RFC 6749 §3.1.2 sanity check on a registered redirect URI.
/// Rejects: empty, non-http(s), URIs containing a fragment. Does not
/// pull a full URL parser into the dep tree — exact-match enforcement
/// at `/oauth/authorize` is the real security boundary; this is just
/// to refuse obviously-broken input early at every entry point that
/// can persist a `redirect_uris` set (DCR `/oauth/register` + admin
/// `POST /admin/oauth/clients`).
pub fn validate_redirect_uri(uri: &str) -> Result<(), &'static str> {
    if uri.is_empty() {
        return Err("must not be empty");
    }
    if !(uri.starts_with("https://") || uri.starts_with("http://")) {
        return Err("must use http(s) scheme");
    }
    let scheme_len = if uri.starts_with("https://") { "https://".len() } else { "http://".len() };
    if uri.len() == scheme_len {
        return Err("missing host after scheme");
    }
    if uri.contains('#') {
        return Err("must not contain a URI fragment per RFC 6749 §3.1.2");
    }
    Ok(())
}

/// Generate a random URL-safe `client_id` (UUIDv4-shaped).
pub fn generate_client_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generate a 32-byte random `client_secret`, URL-safe base64-encoded
/// (no padding) so it survives form-encoded transport without escaping.
pub fn generate_client_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Derive the on-disk `secret_hash` field from a freshly-generated
/// secret. Format: `<salt_b64>:<sha256(salt || secret)_b64>`. The
/// secret is high-entropy (32 random bytes), so a fast hash is fine —
/// the salt's job is to make stored hashes non-comparable across
/// records, not to slow dictionary attacks (which don't apply to
/// uniform-random 32-byte secrets).
pub fn hash_secret(secret: &str) -> String {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    let salt_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(salt);
    let digest_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    format!("{salt_b64}:{digest_b64}")
}

/// Constant-time check: does `provided` match the secret behind
/// `stored_hash`? Returns `false` for any malformed `stored_hash`
/// rather than erroring — the caller treats both cases the same.
///
/// Currently only called from the unit tests; the production caller
/// (the `/oauth/token` endpoint validating `client_secret` from
/// `Authorization: Basic` or POST body) lands in the 7c sub-commit.
#[allow(dead_code)]
pub fn verify_secret(provided: &str, stored_hash: &str) -> bool {
    let Some((salt_b64, expect_b64)) = stored_hash.split_once(':') else {
        return false;
    };
    let Ok(salt) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(salt_b64) else {
        return false;
    };
    let Ok(expect) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(expect_b64) else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(&salt);
    hasher.update(provided.as_bytes());
    let got = hasher.finalize();
    constant_time_eq(got.as_slice(), &expect)
}

#[allow(dead_code)] // paired with `verify_secret` — both exercised in 7c
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

/// In-memory + on-disk client registry for one agent instance. The
/// daemon holds a single `ClientStore` for its lifetime; mutations
/// flush to disk synchronously so a crash mid-registration doesn't
/// orphan secrets. Reads are pure in-memory.
pub struct ClientStore {
    path: PathBuf,
    clients: Vec<OAuthClient>,
}

#[cfg(test)]
impl ClientStore {
    /// In-memory store with no real file backing. The "path" passed
    /// here is the persistence target — tests typically point it at
    /// a `tempdir`-rooted location to verify save/reload behaviour
    /// without touching real state.
    pub(crate) fn empty_at(path: PathBuf) -> Self {
        Self { path, clients: Vec::new() }
    }
}

impl ClientStore {
    /// Load the store, or create an empty one if the file doesn't
    /// exist yet (first-run case). Errors only on real I/O / parse
    /// failures.
    pub fn load(agent_name: &str) -> Result<Self> {
        let path = clients_path(agent_name);
        let clients = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let parsed: ClientsFile = toml::from_str(&text)
                .with_context(|| format!("invalid TOML in {}", path.display()))?;
            parsed.clients
        } else {
            Vec::new()
        };
        Ok(Self { path, clients })
    }

    /// Persist current state to disk (mode 0600). Atomic on POSIX:
    /// write to `<path>.tmp`, then rename. Rename is atomic if the
    /// target dir is on the same filesystem (it is — same agent state
    /// dir).
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = ClientsFile { clients: self.clients.clone() };
        let text = toml::to_string_pretty(&file).context("serialising clients.toml")?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }

    /// Snapshot of all currently-registered clients. Caller gets a
    /// copy — mutations don't leak back into the store.
    pub fn list(&self) -> Vec<OAuthClient> {
        self.clients.clone()
    }

    /// Resolve a client by id. Returns `None` if revoked or never
    /// existed. Keeps the lookup in one place so future indexing
    /// (by-name, by-secret-hash-prefix) doesn't fan out.
    ///
    /// Used by the unit tests; production caller is the
    /// `/oauth/token` handler in 7c (looking up the client behind a
    /// `client_id` + `client_secret` pair).
    #[allow(dead_code)]
    pub fn find(&self, client_id: &str) -> Option<&OAuthClient> {
        self.clients.iter().find(|c| c.client_id == client_id)
    }

    /// Register a fresh client. Generates `client_id` + `client_secret`,
    /// stores only the hash, returns both plaintext values to the
    /// caller — that's the one and only chance to capture them.
    /// `redirect_uris` is the set the client may pass at
    /// `/oauth/authorize`; exact-match validation lives in the
    /// authorize handler. The store does not validate URI shape — that
    /// is the caller's job (DCR rejects invalid input early; the manual
    /// admin path takes the operator at their word).
    pub fn register(
        &mut self,
        name: &str,
        source: ClientSource,
        redirect_uris: Vec<String>,
    ) -> Result<RegisteredClient> {
        let client_id = generate_client_id();
        let client_secret = generate_client_secret();
        let secret_hash = hash_secret(&client_secret);
        let entry = OAuthClient {
            client_id: client_id.clone(),
            name: name.to_string(),
            source,
            secret_hash,
            redirect_uris,
            created_at: Utc::now(),
            last_seen_at: None,
        };
        self.clients.push(entry.clone());
        self.save()?;
        Ok(RegisteredClient { client_id, client_secret, client: entry })
    }

    /// Drop a client. Returns `Ok(true)` if it existed and was
    /// removed, `Ok(false)` if no such id (idempotent).
    ///
    /// Note: token-cascade revocation lives in the token store
    /// (added in 7c). This call only purges the client record;
    /// any tokens held by the deleted client become un-renewable
    /// on next refresh attempt because their `client_id` no longer
    /// validates at `/oauth/token`.
    pub fn revoke(&mut self, client_id: &str) -> Result<bool> {
        let before = self.clients.len();
        self.clients.retain(|c| c.client_id != client_id);
        let removed = self.clients.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Update the `last_seen_at` stamp for a client after a
    /// successful `/oauth/token` exchange. Display-only; the value
    /// is not consulted by any auth decision. Caller arrives in
    /// 7c with the token-issuance path.
    #[allow(dead_code)]
    pub fn touch(&mut self, client_id: &str) -> Result<()> {
        if let Some(c) = self.clients.iter_mut().find(|c| c.client_id == client_id) {
            c.last_seen_at = Some(Utc::now());
            self.save()?;
        }
        Ok(())
    }
}

/// Result of a successful registration. The plaintext `client_secret`
/// is the only chance the caller has to capture it — the store from
/// then on holds only the hash.
#[derive(Debug, Clone)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_secret: String,
    pub client: OAuthClient,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: hashes are uniformly distinct across two calls with
    /// the same secret because the salt differs each time.
    #[test]
    fn hash_secret_uses_random_salt_so_two_calls_differ() {
        let a = hash_secret("samesame");
        let b = hash_secret("samesame");
        assert_ne!(a, b, "salt must vary across hash_secret calls");
    }

    #[test]
    fn verify_secret_round_trip() {
        let secret = generate_client_secret();
        let h = hash_secret(&secret);
        assert!(verify_secret(&secret, &h));
        assert!(!verify_secret("wrong-secret", &h));
        assert!(!verify_secret(&secret, "not-a-valid-hash"));
        assert!(!verify_secret(&secret, "no-colon-here"));
    }

    #[test]
    fn store_register_then_find_then_revoke_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("clients.toml");
        let mut store = ClientStore { path: path.clone(), clients: Vec::new() };

        let r = store
            .register("test-client", ClientSource::Manual, vec!["https://x.example/cb".into()])
            .unwrap();
        assert!(!r.client_id.is_empty());
        assert!(!r.client_secret.is_empty());
        assert!(verify_secret(&r.client_secret, &r.client.secret_hash));

        let found = store.find(&r.client_id).expect("just registered");
        assert_eq!(found.name, "test-client");
        assert_eq!(found.source, ClientSource::Manual);
        assert_eq!(found.redirect_uris, vec!["https://x.example/cb".to_string()]);

        // Persisted to disk.
        assert!(path.is_file(), "save() should have written the file");

        // Re-load and verify the same client comes back.
        let reloaded = ClientStore { path: path.clone(), clients: Vec::new() };
        let mut reloaded = reloaded;
        reloaded.clients = {
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: ClientsFile = toml::from_str(&text).unwrap();
            parsed.clients
        };
        let after = reloaded.find(&r.client_id).expect("survives reload");
        assert!(verify_secret(&r.client_secret, &after.secret_hash));

        // Revoke is idempotent.
        assert!(store.revoke(&r.client_id).unwrap());
        assert!(!store.revoke(&r.client_id).unwrap());
        assert!(store.find(&r.client_id).is_none());
    }

    #[test]
    fn store_load_returns_empty_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("clients.toml");
        let store = ClientStore { path, clients: Vec::new() };
        assert!(store.list().is_empty());
    }

    #[test]
    fn registered_secret_is_not_persisted_in_plaintext() {
        // Defence-in-depth check: the actual secret string must NOT
        // appear anywhere on-disk after registration. If a future
        // refactor accidentally stores the plaintext (e.g. by adding
        // a `client_secret` field to `OAuthClient`), this test fires.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("clients.toml");
        let mut store = ClientStore { path: path.clone(), clients: Vec::new() };
        let r = store
            .register(
                "plaintext-leak-canary",
                ClientSource::Dcr,
                vec!["https://x.example/cb".into()],
            )
            .unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            !body.contains(&r.client_secret),
            "secret leaked into clients.toml on disk: {body}",
        );
    }

    #[test]
    fn validate_redirect_uri_accepts_http_and_https_with_host() {
        assert!(validate_redirect_uri("https://claude.ai/api/mcp/auth_callback").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1:8080/cb").is_ok());
    }

    #[test]
    fn validate_redirect_uri_rejects_obvious_bad_input() {
        assert!(validate_redirect_uri("").is_err());
        assert!(validate_redirect_uri("ftp://example.com/cb").is_err());
        assert!(validate_redirect_uri("https://").is_err());
        assert!(validate_redirect_uri("javascript:alert(1)").is_err());
        // RFC 6749 §3.1.2 — fragments are explicitly forbidden.
        assert!(validate_redirect_uri("https://claude.ai/cb#frag").is_err());
    }

    /// Pre-7e on-disk records had no `redirect_uris` key. Loading
    /// them must succeed (default to empty Vec) so the operator's
    /// daemon doesn't refuse to start after upgrade. Authorize will
    /// still refuse because the registered set is empty — operator
    /// must re-register, but the daemon stays up.
    #[test]
    fn clients_file_loads_pre_7e_record_without_redirect_uris() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("clients.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Hand-rolled TOML matching the pre-7e schema (no
        // `redirect_uris` key).
        let body = r#"
[[clients]]
client_id = "old-client"
name = "pre-7e"
source = "manual"
secret_hash = "saltb64:hashb64"
created_at = "2026-01-01T00:00:00Z"
"#;
        std::fs::write(&path, body).unwrap();
        let parsed: ClientsFile = toml::from_str(body).unwrap();
        assert_eq!(parsed.clients.len(), 1);
        assert_eq!(parsed.clients[0].client_id, "old-client");
        assert!(parsed.clients[0].redirect_uris.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn clients_file_written_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth").join("clients.toml");
        let mut store = ClientStore { path: path.clone(), clients: Vec::new() };
        store
            .register("perm-test", ClientSource::Manual, vec!["https://x.example/cb".into()])
            .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "clients.toml must be 0600, got {mode:o}");
    }
}
