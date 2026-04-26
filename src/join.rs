// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

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
    /// Optional transport/perimeter token for x-server-token.
    #[serde(default, alias = "token")]
    pub transport_token: Option<String>,
    /// Principal identity carried by the bundle for local persistence.
    #[serde(default)]
    pub principal_id: Option<String>,
    /// Optional principal bearer token for Authorization header.
    #[serde(default, alias = "principal_auth_token")]
    pub principal_token: Option<String>,
    /// SHA-256 fingerprint of the server's TLS certificate (hex).
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// PEM-encoded server certificate (for TLS pinning).
    #[serde(default)]
    pub ca: Option<String>,
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
        if token.url.is_empty() {
            bail!("join token has empty url");
        }
        let has_transport = token.transport_token.as_deref().is_some_and(|v| !v.is_empty());
        let has_principal = token.principal_token.as_deref().is_some_and(|v| !v.is_empty());
        if !has_transport && !has_principal {
            bail!("join token must include transport_token or principal_token");
        }
        if !token.url.starts_with("https://") && !token.url.starts_with("http://") {
            bail!("join token URL must use http:// or https:// (got: {})", token.url);
        }
        Ok(token)
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
            transport_token: Some("secret123".to_string()),
            principal_id: Some("agent:planner".to_string()),
            principal_token: Some("principal-secret".to_string()),
            fingerprint: Some("sha256:abcdef".to_string()),
            ca: Some("-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----".to_string()),
        };
        let encoded = token.encode().unwrap();
        assert!(encoded.starts_with("gosh_join_"));
        let decoded = JoinToken::decode(&encoded).unwrap();
        assert_eq!(decoded.url, "https://192.168.1.10:8765");
        assert_eq!(decoded.transport_token.as_deref(), Some("secret123"));
        assert_eq!(decoded.principal_id.as_deref(), Some("agent:planner"));
        assert_eq!(decoded.principal_token.as_deref(), Some("principal-secret"));
        assert_eq!(decoded.fingerprint.as_deref(), Some("sha256:abcdef"));
    }
}
