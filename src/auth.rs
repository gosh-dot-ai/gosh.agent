// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::join::JoinToken;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MemoryAuthState {
    pub memory_url: String,
    #[serde(default)]
    pub transport_token: Option<String>,
    #[serde(default)]
    pub principal_id: Option<String>,
    #[serde(default)]
    pub principal_token: Option<String>,
    #[serde(default)]
    pub tls_fingerprint: Option<String>,
    #[serde(default)]
    pub tls_ca: Option<String>,
}

impl MemoryAuthState {
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(path)?;
        let parsed = serde_json::from_str::<Self>(&content)?;
        Ok(Some(parsed))
    }

    #[allow(dead_code)]
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private_text_file(path, &serde_json::to_string_pretty(self)?)
    }

    pub fn from_join_token(token: &JoinToken) -> Self {
        Self {
            memory_url: token.url.clone(),
            transport_token: token.transport_token.clone(),
            principal_id: token.principal_id.clone(),
            principal_token: token.principal_token.clone(),
            tls_fingerprint: token.fingerprint.clone(),
            tls_ca: token.ca.clone(),
        }
    }
}

fn write_private_text_file(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("auth state path must include a valid file name"))?;
        let tmp_path = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut handle =
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp_path)?;
        handle.write_all(content.as_bytes())?;
        handle.sync_all()?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryAuthState;

    #[test]
    fn auth_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory-auth.json");
        let state = MemoryAuthState {
            memory_url: "http://127.0.0.1:8765".into(),
            transport_token: Some("srv".into()),
            principal_id: Some("agent:planner".into()),
            principal_token: Some("ptok".into()),
            tls_fingerprint: Some("sha256:abc".into()),
            tls_ca: Some("PEM".into()),
        };
        state.save(&path).unwrap();
        let loaded = MemoryAuthState::load(&path).unwrap().unwrap();
        assert_eq!(loaded, state);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
