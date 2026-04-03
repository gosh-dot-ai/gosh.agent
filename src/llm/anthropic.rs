// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use anyhow::bail;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;

use super::LlmProvider;
use super::LlmResponse;
use super::Message;
use super::ToolCall;
use super::ToolDef;
use super::Usage;

pub struct AnthropicProvider {
    api_key: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self { api_key, http: reqwest::Client::new() }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<LlmResponse> {
        let api_messages: Vec<Value> =
            messages.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();

        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": api_messages,
        });

        if !tools.is_empty() {
            let api_tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!(api_tools);
        }

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("Anthropic API error (HTTP {status}): {text}");
        }

        let data: Value = resp.json().await?;

        let usage = Usage {
            input_tokens: data["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: data["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
        };

        let stop_reason = data["stop_reason"].as_str().unwrap_or("end_turn").to_string();

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(content) = data["content"].as_array() {
            for block in content {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() {
                            text_parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            input: block["input"].clone(),
                        });
                    }
                    _ => {}
                }
            }
        }

        let text = if text_parts.is_empty() { None } else { Some(text_parts.join("\n")) };

        Ok(LlmResponse { text, tool_calls, usage, stop_reason })
    }
}
