// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::fs;
use std::path::Path;

use aes_gcm::aead::Aead;
use aes_gcm::AeadCore;
use aes_gcm::Aes256Gcm;
use aes_gcm::KeyInit;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::PublicKey;
use x25519_dalek::StaticSecret;
use zeroize::Zeroize;

const ENVELOPE_MAGIC: &[u8; 4] = b"GMS1";
const SECRET_INFO: &[u8] = b"gosh.memory/agent-secrets/v1";

/// Load an X25519 private key (32 raw bytes) from a file.
#[allow(dead_code)]
pub fn load_secret_key(path: &Path) -> Result<StaticSecret> {
    let bytes =
        fs::read(path).with_context(|| format!("reading secret key: {}", path.display()))?;
    if bytes.len() != 32 {
        bail!("secret key file must be exactly 32 bytes, got {}", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    let key = StaticSecret::from(arr);
    arr.zeroize();
    Ok(key)
}

/// Decrypt a single sealed-box ciphertext (base64-encoded GMS1 envelope).
pub fn decrypt_agent_secret(private_key: &StaticSecret, ciphertext_b64: &str) -> Result<String> {
    use base64::Engine;
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(ciphertext_b64)
        .context("base64 decode of sealed secret")?;

    // Minimum: 4 (magic) + 32 (ephemeral pk) + 12 (nonce) + 16 (tag) = 64
    if envelope.len() < 64 {
        bail!("sealed envelope too short: {} bytes", envelope.len());
    }
    if &envelope[..4] != ENVELOPE_MAGIC {
        bail!("invalid envelope magic: expected GMS1");
    }

    let ephemeral_public_bytes: [u8; 32] = envelope[4..36].try_into().unwrap();
    let nonce_bytes: [u8; 12] = envelope[36..48].try_into().unwrap();
    let ciphertext = &envelope[48..];

    // X25519 ECDH
    let ephemeral_public = PublicKey::from(ephemeral_public_bytes);
    let shared_secret = private_key.diffie_hellman(&ephemeral_public);

    // HKDF-SHA256
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut aes_key = [0u8; 32];
    hkdf.expand(SECRET_INFO, &mut aes_key).map_err(|_| anyhow::anyhow!("HKDF expand failed"))?;

    // AES-256-GCM decrypt
    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .map_err(|e| anyhow::anyhow!("AES-256-GCM init: {e}"))?;
    aes_key.zeroize();
    let nonce = <Aes256Gcm as AeadCore>::NonceSize::default();
    let _ = nonce;
    let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
    let payload = aes_gcm::aead::Payload { msg: ciphertext, aad: SECRET_INFO };
    let plaintext =
        cipher.decrypt(nonce, payload).map_err(|_| anyhow::anyhow!("AES-GCM decrypt failed"))?;

    String::from_utf8(plaintext).context("decrypted secret is not valid UTF-8")
}

/// Save raw 32-byte secret key to file with mode 0600.
#[allow(dead_code)]
pub fn save_secret_key(path: &Path, key: &[u8; 32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(key)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a GMS1 envelope using the same logic as gosh.memory.
    fn encrypt_for_test(agent_public: &PublicKey, plaintext: &str) -> String {
        use aes_gcm::aead::OsRng;
        use base64::Engine;

        // Generate ephemeral keypair
        let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
        let ephemeral_public = PublicKey::from(&ephemeral_secret);

        // ECDH
        let shared = ephemeral_secret.diffie_hellman(agent_public);

        // HKDF
        let hkdf = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut aes_key = [0u8; 32];
        hkdf.expand(SECRET_INFO, &mut aes_key).unwrap();

        // AES-GCM encrypt
        let cipher = Aes256Gcm::new_from_slice(&aes_key).unwrap();
        let nonce = Aes256Gcm::generate_nonce(OsRng);
        let payload = aes_gcm::aead::Payload { msg: plaintext.as_bytes(), aad: SECRET_INFO };
        let ct = cipher.encrypt(&nonce, payload).unwrap();

        // Build envelope
        let mut envelope = Vec::with_capacity(4 + 32 + 12 + ct.len());
        envelope.extend_from_slice(ENVELOPE_MAGIC);
        envelope.extend_from_slice(ephemeral_public.as_bytes());
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&ct);

        base64::engine::general_purpose::STANDARD.encode(&envelope)
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        use aes_gcm::aead::OsRng;
        let private = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&private);

        // Neutral fixture — gitleaks and similar secret scanners flag
        // anything matching the well-known Anthropic API-key prefix,
        // even when it's an obvious test literal. This roundtrip test
        // only cares about equality of bytes in vs bytes out, so the
        // value doesn't need to mimic any real provider's key format.
        let secret = "agent-secret-roundtrip-fixture";
        let ciphertext_b64 = encrypt_for_test(&public, secret);

        let decrypted = decrypt_agent_secret(&private, &ciphertext_b64).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn wrong_key_fails() {
        use aes_gcm::aead::OsRng;
        let private1 = StaticSecret::random_from_rng(OsRng);
        let public1 = PublicKey::from(&private1);
        let private2 = StaticSecret::random_from_rng(OsRng);

        let ciphertext_b64 = encrypt_for_test(&public1, "secret");
        let result = decrypt_agent_secret(&private2, &ciphertext_b64);
        assert!(result.is_err());
    }

    #[test]
    fn bad_magic_rejected() {
        use base64::Engine;
        let mut envelope = vec![0u8; 64];
        envelope[..4].copy_from_slice(b"BAD!");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&envelope);

        use aes_gcm::aead::OsRng;
        let private = StaticSecret::random_from_rng(OsRng);
        let result = decrypt_agent_secret(&private, &b64);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("magic"));
    }

    #[test]
    fn too_short_rejected() {
        use base64::Engine;
        let envelope = b"GMS1short";
        let b64 = base64::engine::general_purpose::STANDARD.encode(envelope);

        use aes_gcm::aead::OsRng;
        let private = StaticSecret::random_from_rng(OsRng);
        let result = decrypt_agent_secret(&private, &b64);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn save_and_load_key_roundtrip() {
        use aes_gcm::aead::OsRng;
        let secret = StaticSecret::random_from_rng(OsRng);
        let bytes: [u8; 32] = secret.to_bytes();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        save_secret_key(&path, &bytes).unwrap();

        let loaded = load_secret_key(&path).unwrap();
        // Verify by checking the derived public key matches
        let pub1 = PublicKey::from(&secret);
        let pub2 = PublicKey::from(&loaded);
        assert_eq!(pub1.as_bytes(), pub2.as_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
