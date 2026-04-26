// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Serialize)]
pub struct SecretRef {
    pub name: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swarm_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncryptedSecret {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub scope: String,
    #[allow(dead_code)]
    pub algorithm: String,
    #[allow(dead_code)]
    pub key_id: String,
    pub ciphertext: String,
}

#[derive(Debug, Deserialize)]
struct ResolveResponse {
    secrets: Vec<EncryptedSecret>,
}

/// Call the memory server's secret resolve endpoint.
///
/// Returns encrypted blobs that must be decrypted with the agent's private key.
pub async fn resolve_secrets(
    http: &reqwest::Client,
    memory_url: &str,
    transport_token: Option<&str>,
    principal_token: &str,
    key: &str,
    refs: &[SecretRef],
) -> Result<Vec<EncryptedSecret>> {
    let url = format!("{}/api/v1/agent/secrets/resolve", memory_url.trim_end_matches('/'));

    let mut request =
        http.post(&url).bearer_auth(principal_token).json(&json!({ "key": key, "refs": refs }));

    if let Some(token) = transport_token {
        request = request.header("X-GOSH-MEMORY-TOKEN", token);
    }

    let response = request.send().await.context("POST /api/v1/agent/secrets/resolve")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("secret resolve failed (HTTP {status}): {body}");
    }

    let parsed: ResolveResponse =
        response.json().await.context("parsing secret resolve response")?;
    Ok(parsed.secrets)
}
