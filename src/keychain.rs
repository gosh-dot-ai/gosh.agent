// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;

const SERVICE_NAME: &str = "gosh";
const ACCOUNT_PREFIX: &str = "agent";
const GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR: &str = "GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR";

/// Mirror of `<gosh.cli>/src/keychain/agent.rs::AgentSecrets`. The
/// daemon only reads these fields, so all are optional — a partial
/// keychain entry (e.g. created agent without an explicit
/// `principal_token`) just yields `None` for the missing field and
/// the caller decides whether that's fatal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentSecrets {
    #[serde(default)]
    pub principal_token: Option<String>,
    #[serde(default)]
    pub join_token: Option<String>,
    /// X25519 private key (base64-encoded 32 bytes) for sealed-box
    /// decryption of memory-issued secret blobs.
    #[serde(default)]
    pub secret_key: Option<String>,
}

impl AgentSecrets {
    /// Load creds for the given agent name. Backend selection:
    ///   - if `GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR` is set → read JSON from
    ///     `<dir>/agent_<name>.json` (file backend, matches CLI's
    ///     `FileKeychain` layout);
    ///   - otherwise → OS keychain via `keyring`.
    ///
    /// Returns `Ok(None)` when no entry exists for this name (caller
    /// decides whether that's fatal — usually it is, since the daemon
    /// can't reach memory without creds).
    pub fn load(name: &str) -> Result<Option<Self>> {
        let account = format!("{ACCOUNT_PREFIX}/{name}");

        if let Ok(dir) = std::env::var(GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR) {
            return load_from_file(Path::new(&dir), &account);
        }

        let entry = keyring::Entry::new(SERVICE_NAME, &account)
            .with_context(|| format!("creating keychain entry for {account}"))?;
        match entry.get_password() {
            Ok(json) => parse_json(&json, &account).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("keychain error for {account}: {e}")),
        }
    }
}

fn load_from_file(dir: &Path, account: &str) -> Result<Option<AgentSecrets>> {
    let safe_name = account.replace('/', "_");
    let path = dir.join(format!("{safe_name}.json"));
    match std::fs::read_to_string(&path) {
        Ok(json) => parse_json(&json, account).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::anyhow!("test-keychain read error for {account} ({}): {e}", path.display()))
        }
    }
}

fn parse_json(json: &str, account: &str) -> Result<AgentSecrets> {
    serde_json::from_str(json).with_context(|| format!("parsing keychain JSON for {account}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_file_returns_none_when_path_absent() {
        let dir = tempfile::tempdir().unwrap();
        let res = load_from_file(dir.path(), "agent/missing").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn load_from_file_replaces_slash_with_underscore_in_filename() {
        // Mirrors `<gosh.cli>/src/keychain/mod.rs::FileKeychain::path_for`
        // — the on-disk filename for account "agent/foo" is
        // "agent_foo.json". Pin it so the two sides can't drift.
        let dir = tempfile::tempdir().unwrap();
        let payload = r#"{"principal_token":"pt","join_token":"jt","secret_key":"sk"}"#;
        std::fs::write(dir.path().join("agent_foo.json"), payload).unwrap();

        let secrets = load_from_file(dir.path(), "agent/foo").unwrap().unwrap();
        assert_eq!(secrets.principal_token.as_deref(), Some("pt"));
        assert_eq!(secrets.join_token.as_deref(), Some("jt"));
        assert_eq!(secrets.secret_key.as_deref(), Some("sk"));
    }

    #[test]
    fn load_from_file_round_trips_partial_secrets() {
        // CLI is allowed to write partial entries (e.g. an agent
        // created without an explicit `principal_token`). All fields
        // are `Option<String>`, missing ones come back as `None`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent_alpha.json"), r#"{"join_token":"only-this"}"#)
            .unwrap();

        let secrets = load_from_file(dir.path(), "agent/alpha").unwrap().unwrap();
        assert!(secrets.principal_token.is_none());
        assert_eq!(secrets.join_token.as_deref(), Some("only-this"));
        assert!(secrets.secret_key.is_none());
    }

    #[test]
    fn load_from_file_errors_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent_bad.json"), "{ not valid json").unwrap();
        let err = load_from_file(dir.path(), "agent/bad").unwrap_err();
        assert!(
            err.to_string().contains("parsing keychain JSON"),
            "expected parse error, got: {err}",
        );
    }

    #[test]
    fn load_dispatches_to_file_backend_when_env_var_is_set() {
        // Branch test for the public entry point. Env-var manipulation
        // is process-global, so we set + unset around the call. Other
        // tests in this module don't touch this var, so this should
        // be safe even under parallel `cargo test` execution.
        let dir = tempfile::tempdir().unwrap();
        let payload = r#"{"principal_token":"from-file","join_token":null,"secret_key":null}"#;
        std::fs::write(dir.path().join("agent_envtest.json"), payload).unwrap();

        // SAFETY: env mutation is process-global. We isolate by using a
        // unique account name (`agent/envtest`) so concurrent tests
        // can't collide on this exact key.
        unsafe {
            std::env::set_var(GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR, dir.path());
        }
        let res = AgentSecrets::load("envtest");
        unsafe {
            std::env::remove_var(GOSH_AGENT_TEST_MODE_KEYCHAIN_DIR);
        }

        let secrets = res.unwrap().unwrap();
        assert_eq!(secrets.principal_token.as_deref(), Some("from-file"));
    }
}
