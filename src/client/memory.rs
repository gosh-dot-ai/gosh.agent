// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use super::McpClient;
use super::McpTransport;

#[derive(Serialize)]
pub struct RecallParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub query: String,
    pub token_budget: i64,
}

#[derive(Serialize)]
pub struct PlanInferenceParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub query: String,
    pub token_budget: i64,
}

#[derive(Serialize)]
pub struct StoreParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub content: String,
    pub scope: String,
    pub content_type: String,
    pub session_num: i32,
    pub session_date: String,
    pub speakers: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct MemoryStoreParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub fact: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Serialize)]
pub struct IngestFactsParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub scope: String,
    pub facts: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrich_l0: Option<bool>,
}

#[derive(Serialize)]
pub struct ListFactsParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct MemoryQueryParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub filter: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct MemoryGetParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub fact_id: String,
}

#[derive(Serialize)]
pub struct MemoryGetConfigParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
}

#[derive(Serialize)]
pub struct CourierSubscribeParams {
    pub key: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub connection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
}

/// Domain memory client.
pub struct MemoryMcpClient {
    client: McpClient,
}

#[allow(dead_code)]
impl MemoryMcpClient {
    pub fn new(transport: impl McpTransport + 'static) -> Self {
        Self { client: McpClient::new(transport, "gosh-agent") }
    }

    pub async fn recall(&self, params: RecallParams) -> Result<Value> {
        self.client.call_tool("memory_recall", serde_json::to_value(params)?).await
    }

    pub async fn plan_inference(&self, params: PlanInferenceParams) -> Result<Value> {
        self.client.call_tool("memory_plan_inference", serde_json::to_value(params)?).await
    }

    pub async fn store(&self, params: StoreParams) -> Result<Value> {
        self.client.call_tool("memory_store", serde_json::to_value(params)?).await
    }

    pub async fn memory_store(&self, params: MemoryStoreParams) -> Result<Value> {
        self.client.call_tool("memory_store", serde_json::to_value(params)?).await
    }

    pub async fn ingest_asserted_facts(&self, params: IngestFactsParams) -> Result<Value> {
        self.client.call_tool("memory_ingest_asserted_facts", serde_json::to_value(params)?).await
    }

    pub async fn list_facts(&self, params: ListFactsParams) -> Result<Value> {
        self.client.call_tool("memory_list", serde_json::to_value(params)?).await
    }

    pub async fn memory_query(&self, params: MemoryQueryParams) -> Result<Value> {
        self.client.call_tool("memory_query", serde_json::to_value(params)?).await
    }

    pub async fn memory_get(&self, params: MemoryGetParams) -> Result<Value> {
        self.client.call_tool("memory_get", serde_json::to_value(params)?).await
    }

    pub async fn memory_get_config(&self, params: MemoryGetConfigParams) -> Result<Value> {
        self.client.call_tool("memory_get_config", serde_json::to_value(params)?).await
    }

    pub async fn courier_subscribe(&self, params: CourierSubscribeParams) -> Result<Value> {
        self.client.call_tool("courier_subscribe", serde_json::to_value(params)?).await
    }

    pub async fn courier_unsubscribe(&self, sub_id: &str) -> Result<Value> {
        self.client.call_tool("courier_unsubscribe", json!({ "sub_id": sub_id })).await
    }
}
