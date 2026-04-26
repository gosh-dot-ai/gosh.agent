// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

pub mod anthropic;
pub mod local_cli;
pub mod multi;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

/// A tool definition passed to the LLM.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A tool-use request from the LLM.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Token usage from a single LLM call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub reasoning_tokens: u32,
    pub cached_input_read_tokens: u32,
    pub cached_input_write_tokens: u32,
}

/// Parsed response from an LLM call.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    #[allow(dead_code)]
    pub stop_reason: String,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<LlmResponse>;
}
