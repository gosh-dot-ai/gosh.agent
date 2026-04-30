// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::PathBuf;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use anyhow::Context;
use anyhow::Result;
use base64::Engine;

use crate::plugin::config::state_dir;

/// Path of the admin-token file for `agent_name`.
pub fn admin_token_path(agent_name: &str) -> PathBuf {
    state_dir(agent_name).join("admin.token")
}

/// Generate a fresh random token string (32 bytes, URL-safe base64
/// without padding so it survives an HTTP header round-trip without
/// escaping).
pub fn generate_admin_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a fresh token, write it atomically to the per-instance
/// admin-token file (mode 0600 on Unix), return the token. Used at
/// daemon startup. Overwrites any previous file — tokens from prior
/// daemon runs become invalid as a side-effect.
pub fn write_fresh_token(agent_name: &str) -> Result<String> {
    let token = generate_admin_token();
    let path = admin_token_path(agent_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("token.tmp");
    std::fs::write(&tmp, &token).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_url_safe_and_distinct() {
        let a = generate_admin_token();
        let b = generate_admin_token();
        assert_ne!(a, b);
        // URL-safe base64 — no '+', '/', or '=' padding.
        for c in a.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "unexpected char {c:?} in admin token"
            );
        }
        // 32 bytes b64-encoded without padding = 43 chars.
        assert_eq!(a.len(), 43);
    }
}
