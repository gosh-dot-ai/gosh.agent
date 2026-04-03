// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;

use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tracing::warn;

use super::config::AgentConfig;
use crate::client::memory::MemoryMcpClient;
use crate::client::memory::MemoryQueryParams;

const AGENT_CONFIG_SCHEMA_VERSION: i64 = 1;

pub async fn load_agent_config(
    memory: &Arc<MemoryMcpClient>,
    bootstrap: &AgentConfig,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
) -> Result<AgentConfig> {
    let target = format!("agent:{agent_id}");

    let keyed_filter = json!({
        "kind": "agent_config",
        "target": target,
        "metadata.agent_id": agent_id,
        "metadata.swarm_id": swarm_id,
        "metadata.key": key,
    });
    if let Some(fact) = query_single(memory, key, agent_id, swarm_id, keyed_filter).await? {
        return parse_agent_config(bootstrap, &fact, agent_id);
    }

    let shared_filter = json!({
        "kind": "agent_config",
        "target": format!("agent:{agent_id}"),
        "metadata.agent_id": agent_id,
        "metadata.swarm_id": swarm_id,
    });
    if let Some(fact) = query_single(memory, key, agent_id, swarm_id, shared_filter).await? {
        return parse_agent_config(bootstrap, &fact, agent_id);
    }

    warn!(agent_id, swarm_id, key, "no persisted agent_config found; using bootstrap defaults");
    Ok(bootstrap.clone())
}

async fn query_single(
    memory: &Arc<MemoryMcpClient>,
    key: &str,
    agent_id: &str,
    swarm_id: &str,
    filter: Value,
) -> Result<Option<Value>> {
    let query_result = memory
        .memory_query(MemoryQueryParams {
            key: key.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            filter,
            sort_by: Some("created_at".to_string()),
            sort_order: Some("desc".to_string()),
            limit: Some(2),
        })
        .await?;

    let facts = query_result.get("facts").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    match facts.len() {
        0 => Ok(None),
        1 => Ok(Some(facts[0].clone())),
        _ => {
            warn!(
                key,
                agent_id, swarm_id, "multiple agent_config facts found; using newest by created_at"
            );
            Ok(Some(facts[0].clone()))
        }
    }
}

fn parse_agent_config(
    bootstrap: &AgentConfig,
    fact: &Value,
    expected_agent_id: &str,
) -> Result<AgentConfig> {
    let target = format!("agent:{expected_agent_id}");
    if !target_contains(fact, &target) {
        bail!("AGENT_CONFIG_TARGET_MISMATCH");
    }

    let metadata = fact
        .get("metadata")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow::anyhow!("agent_config fact missing metadata object"))?;

    let schema_version = metadata
        .get("schema_version")
        .ok_or_else(|| anyhow::anyhow!("AGENT_CONFIG_SCHEMA_VERSION_REQUIRED"))
        .and_then(|value| {
            as_i64(value).ok_or_else(|| anyhow::anyhow!("INVALID_AGENT_CONFIG_SCHEMA_VERSION"))
        })?;
    if schema_version != AGENT_CONFIG_SCHEMA_VERSION {
        bail!("UNSUPPORTED_AGENT_CONFIG_SCHEMA_VERSION:{schema_version}");
    }

    if let Some(agent_id) = metadata.get("agent_id").and_then(|v| v.as_str()) {
        if agent_id != expected_agent_id {
            bail!("AGENT_CONFIG_AGENT_ID_MISMATCH");
        }
    }

    let mut cfg = bootstrap.clone();
    cfg.enabled = metadata.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    if let Some(value) = metadata.get("review_budget_reserve").and_then(as_f64) {
        cfg.review_budget_reserve = value;
    }
    if let Some(value) = metadata.get("too_complex_threshold").and_then(as_f64) {
        cfg.too_complex_threshold = value;
    }
    if let Some(value) = metadata.get("max_retries").and_then(as_u32) {
        cfg.max_retries = value;
    }
    if let Some(value) = metadata.get("extraction_profile").and_then(|v| v.as_str()) {
        cfg.extraction_profile = value.to_string();
    }
    if let Some(value) = metadata.get("fast_profile").and_then(|v| v.as_str()) {
        cfg.fast_profile = value.to_string();
    }
    if let Some(value) = metadata.get("balanced_profile").and_then(|v| v.as_str()) {
        cfg.balanced_profile = value.to_string();
    }
    if let Some(value) = metadata.get("strong_profile").and_then(|v| v.as_str()) {
        cfg.strong_profile = value.to_string();
    }
    if let Some(value) = metadata.get("review_profile").and_then(|v| v.as_str()) {
        cfg.review_profile = value.to_string();
    }
    if let Some(value) = metadata.get("max_parallel_tasks").and_then(as_usize) {
        cfg.max_parallel_tasks = value;
    }
    if let Some(value) = metadata.get("global_cli_cooldown_secs").and_then(as_u64) {
        cfg.global_cli_cooldown_secs = Some(value);
    }

    for (key, value) in metadata {
        if let Some(rest) = key.strip_prefix("profile.") {
            if let Some(profile_id) = rest.strip_suffix(".cooldown_secs") {
                cfg.profile_runtime.entry(profile_id.to_string()).or_default().cooldown_secs =
                    Some(as_u64(value).ok_or_else(|| {
                        anyhow::anyhow!("profile cooldown override must be integer")
                    })?);
            } else if let Some(profile_id) = rest.strip_suffix(".max_concurrency") {
                cfg.profile_runtime.entry(profile_id.to_string()).or_default().max_concurrency =
                    Some(as_usize(value).ok_or_else(|| {
                        anyhow::anyhow!("profile max_concurrency override must be integer")
                    })?);
            }
        }
    }

    cfg.validate()?;
    Ok(cfg)
}

fn target_contains(fact: &Value, target: &str) -> bool {
    match fact.get("target") {
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(target)),
        Some(Value::String(value)) => value == target,
        _ => false,
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

fn as_usize(value: &Value) -> Option<usize> {
    as_u64(value).and_then(|n| usize::try_from(n).ok())
}

fn as_u32(value: &Value) -> Option<u32> {
    as_u64(value).and_then(|n| u32::try_from(n).ok())
}

fn as_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::McpTransport;

    struct MockTransport {
        responses: Mutex<Vec<Value>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Value>) -> Self {
            Self { responses: Mutex::new(responses) }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn send(
            &self,
            body: &Value,
            _session_id: Option<&str>,
        ) -> anyhow::Result<(Value, Option<String>)> {
            let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
            if method == "initialize" {
                return Ok((
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {},
                            "serverInfo": { "name": "mock", "version": "0.1.0" }
                        }
                    }),
                    Some("mock-session".to_string()),
                ));
            }
            if method == "notifications/initialized" {
                return Ok((json!({}), Some("mock-session".to_string())));
            }

            let resp = {
                let mut responses = self.responses.lock().unwrap();
                responses.remove(0)
            };
            Ok((resp, Some("mock-session".to_string())))
        }
    }

    fn wrap(payload: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{"type": "text", "text": serde_json::to_string(payload).unwrap()}],
                "isError": false
            }
        })
    }

    #[tokio::test]
    async fn load_agent_config_uses_key_specific_fact_when_present() {
        let fact = json!({
            "facts": [{
                "kind": "agent_config",
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "planner",
                    "swarm_id": "swarm-alpha",
                    "key": "proj-a",
                    "fast_profile": "claude_code_cli",
                    "max_parallel_tasks": 2,
                    "profile.claude_code_cli.cooldown_secs": 1200
                }
            }]
        });
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![wrap(&fact)])));
        let cfg =
            load_agent_config(&memory, &AgentConfig::default(), "proj-a", "planner", "swarm-alpha")
                .await
                .unwrap();

        assert_eq!(cfg.fast_profile, "claude_code_cli");
        assert_eq!(cfg.max_parallel_tasks, 2);
        assert_eq!(
            cfg.profile_runtime.get("claude_code_cli").and_then(|p| p.cooldown_secs),
            Some(1200)
        );
    }

    #[tokio::test]
    async fn load_agent_config_falls_back_to_bootstrap_when_missing() {
        let empty = json!({"facts": []});
        let empty2 = json!({"facts": []});
        let memory =
            Arc::new(MemoryMcpClient::new(MockTransport::new(vec![wrap(&empty), wrap(&empty2)])));
        let bootstrap = AgentConfig::default();
        let cfg = load_agent_config(&memory, &bootstrap, "proj-a", "planner", "swarm-alpha")
            .await
            .unwrap();
        assert_eq!(cfg.fast_profile, bootstrap.fast_profile);
    }

    #[tokio::test]
    async fn load_agent_config_parses_enabled_and_runtime_limits() {
        let fact = json!({
            "facts": [{
                "kind": "agent_config",
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "planner",
                    "swarm_id": "swarm-alpha",
                    "key": "proj-a",
                    "enabled": false,
                    "max_parallel_tasks": 3,
                    "global_cli_cooldown_secs": 900,
                    "profile.codex_cli.max_concurrency": 1,
                    "profile.codex_cli.cooldown_secs": 1200
                }
            }]
        });
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![wrap(&fact)])));

        let cfg =
            load_agent_config(&memory, &AgentConfig::default(), "proj-a", "planner", "swarm-alpha")
                .await
                .unwrap();

        assert!(!cfg.enabled);
        assert_eq!(cfg.max_parallel_tasks, 3);
        assert_eq!(cfg.global_cli_cooldown_secs, Some(900));
        assert_eq!(cfg.profile_runtime.get("codex_cli").and_then(|p| p.max_concurrency), Some(1));
        assert_eq!(cfg.profile_runtime.get("codex_cli").and_then(|p| p.cooldown_secs), Some(1200));
    }

    #[tokio::test]
    async fn load_agent_config_uses_newest_when_multiple_configs_exist() {
        let first = wrap(&json!({
            "facts": [
                {
                    "id": "cfg-new",
                    "target": ["agent:planner"],
                    "metadata": {
                        "schema_version": 1,
                        "agent_id": "planner",
                        "swarm_id": "swarm-a",
                        "key": "project-x",
                        "fast_profile": "codex_cli"
                    }
                },
                {
                    "id": "cfg-old",
                    "target": ["agent:planner"],
                    "metadata": {
                        "schema_version": 1,
                        "agent_id": "planner",
                        "swarm_id": "swarm-a",
                        "key": "project-x",
                        "fast_profile": "claude_code_cli"
                    }
                }
            ]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let cfg =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap();

        assert_eq!(cfg.fast_profile, "codex_cli");
    }

    #[tokio::test]
    async fn load_agent_config_requires_schema_version() {
        let first = wrap(&json!({
            "facts": [{
                "id": "cfg-bad",
                "target": ["agent:planner"],
                "metadata": {
                    "agent_id": "planner",
                    "swarm_id": "swarm-a",
                    "key": "project-x"
                }
            }]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let err =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap_err();

        assert!(err.to_string().contains("AGENT_CONFIG_SCHEMA_VERSION_REQUIRED"));
    }

    #[tokio::test]
    async fn load_agent_config_rejects_unsupported_schema_version() {
        let first = wrap(&json!({
            "facts": [{
                "id": "cfg-future",
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 99,
                    "agent_id": "planner",
                    "swarm_id": "swarm-a",
                    "key": "project-x"
                }
            }]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let err =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap_err();

        assert!(err.to_string().contains("UNSUPPORTED_AGENT_CONFIG_SCHEMA_VERSION:99"));
    }

    #[tokio::test]
    async fn load_agent_config_rejects_target_mismatch() {
        let first = wrap(&json!({
            "facts": [{
                "id": "cfg-wrong-target",
                "target": ["agent:other-agent"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "planner",
                    "swarm_id": "swarm-a",
                    "key": "project-x"
                }
            }]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let err =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap_err();

        assert!(err.to_string().contains("AGENT_CONFIG_TARGET_MISMATCH"));
    }

    #[tokio::test]
    async fn load_agent_config_rejects_agent_id_mismatch() {
        let first = wrap(&json!({
            "facts": [{
                "id": "cfg-wrong-agent",
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "imposter",
                    "swarm_id": "swarm-a",
                    "key": "project-x"
                }
            }]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let err =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap_err();

        assert!(err.to_string().contains("AGENT_CONFIG_AGENT_ID_MISMATCH"));
    }

    #[tokio::test]
    async fn load_agent_config_rejects_invalid_numeric_ranges() {
        let first = wrap(&json!({
            "facts": [{
                "id": "cfg-bad-range",
                "target": ["agent:planner"],
                "metadata": {
                    "schema_version": 1,
                    "agent_id": "planner",
                    "swarm_id": "swarm-a",
                    "key": "project-x",
                    "review_budget_reserve": 2.0
                }
            }]
        }));
        let memory = Arc::new(MemoryMcpClient::new(MockTransport::new(vec![first])));

        let err =
            load_agent_config(&memory, &AgentConfig::default(), "project-x", "planner", "swarm-a")
                .await
                .unwrap_err();

        assert!(err.to_string().contains("review_budget_reserve"));
    }
}
