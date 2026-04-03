// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use anyhow::bail;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

const PREFIX: &str = "gosh_join_";

/// Decoded join token payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinToken {
    /// Memory server URL (e.g. "https://192.168.1.10:8765")
    pub url: String,
    /// Auth token for x-server-token header
    pub token: String,
    /// SHA-256 fingerprint of the server's TLS certificate (hex)
    pub fingerprint: String,
    /// PEM-encoded server certificate (for TLS pinning)
    pub ca: String,
}

impl JoinToken {
    /// Encode token to a portable string.
    #[allow(dead_code)]
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        Ok(format!("{PREFIX}{}", base64url_encode(&json)))
    }

    /// Decode token from string.
    pub fn decode(input: &str) -> Result<Self> {
        let b64 = input
            .strip_prefix(PREFIX)
            .ok_or_else(|| anyhow::anyhow!("join token must start with '{PREFIX}'"))?;
        let json = base64url_decode(b64)?;
        let token: Self = serde_json::from_slice(&json)?;
        if token.url.is_empty() || token.token.is_empty() {
            bail!("join token has empty url or token");
        }
        if !token.url.starts_with("https://") {
            bail!("join token URL must use https:// (got: {})", token.url);
        }
        Ok(token)
    }

    /// Build a reqwest Client that trusts only this server's certificate.
    pub fn build_http_client(&self) -> Result<reqwest::Client> {
        let cert = reqwest::Certificate::from_pem(self.ca.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid CA in join token: {e}"))?;

        let client = reqwest::Client::builder()
            .tls_certs_only([cert])
            .danger_accept_invalid_hostnames(true)
            .build()?;

        Ok(client)
    }
}

#[allow(dead_code)]
fn base64url_encode(data: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            1 => (chunk[0] as u32) << 16,
            _ => unreachable!(),
        };
        s.push(alphabet[((n >> 18) & 0x3f) as usize] as char);
        s.push(alphabet[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            s.push(alphabet[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            s.push(alphabet[(n & 0x3f) as usize] as char);
        }
    }
    s
}

fn base64url_decode(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    for chunk in bytes.chunks(4) {
        let vals: Vec<u32> = chunk
            .iter()
            .map(|&b| match b {
                b'A'..=b'Z' => Ok((b - b'A') as u32),
                b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
                b'-' => Ok(62),
                b'_' => Ok(63),
                _ => bail!("invalid base64url character: {}", b as char),
            })
            .collect::<Result<_>>()?;
        let n = match vals.len() {
            4 => vals[0] << 18 | vals[1] << 12 | vals[2] << 6 | vals[3],
            3 => vals[0] << 18 | vals[1] << 12 | vals[2] << 6,
            2 => vals[0] << 18 | vals[1] << 12,
            _ => bail!("invalid base64url chunk length"),
        };
        out.push((n >> 16) as u8);
        if vals.len() > 2 {
            out.push((n >> 8 & 0xff) as u8);
        }
        if vals.len() > 3 {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let token = JoinToken {
            url: "https://192.168.1.10:8765".to_string(),
            token: "secret123".to_string(),
            fingerprint: "sha256:abcdef".to_string(),
            ca: "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----".to_string(),
        };
        let encoded = token.encode().unwrap();
        assert!(encoded.starts_with("gosh_join_"));
        let decoded = JoinToken::decode(&encoded).unwrap();
        assert_eq!(decoded.url, "https://192.168.1.10:8765");
        assert_eq!(decoded.token, "secret123");
        assert_eq!(decoded.fingerprint, "sha256:abcdef");
    }
}
