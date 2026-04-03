// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use anyhow::bail;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use serde_json::Value;

use super::anthropic::AnthropicProvider;
use super::LlmProvider;
use super::LlmResponse;
use super::Message;
use super::ToolCall;
use super::ToolDef;
use super::Usage;

const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const INCEPTION_BASE_URL: &str = "https://api.inceptionlabs.ai/v1";

pub struct MultiProvider {
    anthropic: Option<AnthropicProvider>,
    openai_api_key: Option<String>,
    groq_api_key: Option<String>,
    inception_api_key: Option<String>,
    http: reqwest::Client,
}

impl MultiProvider {
    pub fn new(
        anthropic_api_key: Option<String>,
        openai_api_key: Option<String>,
        groq_api_key: Option<String>,
        inception_api_key: Option<String>,
    ) -> Self {
        Self {
            anthropic: anthropic_api_key.map(AnthropicProvider::new),
            openai_api_key,
            groq_api_key,
            inception_api_key,
            http: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiRoute {
    Anthropic,
    OpenAi,
    Groq,
    Inception,
}

fn route_for_model(model: &str) -> ApiRoute {
    if model.starts_with("anthropic/") || model.starts_with("claude-") {
        return ApiRoute::Anthropic;
    }
    if model.starts_with("inception/") {
        return ApiRoute::Inception;
    }
    if model.starts_with("openai/") {
        return ApiRoute::OpenAi;
    }
    if model.starts_with("qwen/")
        || model.starts_with("groq/")
        || model.starts_with("meta-llama/")
        || model.starts_with("moonshotai/")
        || model.starts_with("canopylabs/")
    {
        return ApiRoute::Groq;
    }
    ApiRoute::OpenAi
}

fn api_model_name(model: &str, route: ApiRoute) -> &str {
    match route {
        ApiRoute::Anthropic => model.strip_prefix("anthropic/").unwrap_or(model),
        ApiRoute::Inception => model.strip_prefix("inception/").unwrap_or(model),
        ApiRoute::Groq => model,
        ApiRoute::OpenAi => model.strip_prefix("openai/").unwrap_or(model),
    }
}

fn openai_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut api_messages = Vec::with_capacity(messages.len() + usize::from(!system.is_empty()));
    if !system.is_empty() {
        api_messages.push(json!({ "role": "system", "content": system }));
    }
    api_messages.extend(
        messages.iter().map(|message| json!({ "role": message.role, "content": message.content })),
    );
    api_messages
}

fn openai_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
}

fn parse_tool_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn parse_openai_content(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(text)) => Some(text.to_string()),
        Some(Value::Array(items)) => {
            let mut chunks = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => chunks.push(text.to_string()),
                    Value::Object(map) => {
                        if map.get("type").and_then(|value| value.as_str()) == Some("text") {
                            if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                                chunks.push(text.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            if chunks.is_empty() {
                None
            } else {
                Some(chunks.join("\n"))
            }
        }
        _ => None,
    }
}

#[async_trait]
impl LlmProvider for MultiProvider {
    async fn chat(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<LlmResponse> {
        let route = route_for_model(model);
        if route == ApiRoute::Anthropic {
            let provider = self.anthropic.as_ref().ok_or_else(|| {
                anyhow::anyhow!("ANTHROPIC_API_KEY is required for model {model}")
            })?;
            return provider
                .chat(api_model_name(model, route), system, messages, tools, max_tokens)
                .await;
        }

        let (base_url, api_key) = match route {
            ApiRoute::OpenAi => (
                OPENAI_BASE_URL,
                self.openai_api_key.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("OPENAI_API_KEY is required for model {model}")
                })?,
            ),
            ApiRoute::Groq => (
                GROQ_BASE_URL,
                self.groq_api_key
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("GROQ_API_KEY is required for model {model}"))?,
            ),
            ApiRoute::Inception => (
                INCEPTION_BASE_URL,
                self.inception_api_key.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "INCEPTION_API_KEY or MERCURY_API_KEY is required for model {model}"
                    )
                })?,
            ),
            ApiRoute::Anthropic => unreachable!(),
        };

        let mut body = json!({
            "model": api_model_name(model, route),
            "messages": openai_messages(system, messages),
            "max_tokens": max_tokens,
            "temperature": 0,
        });

        if !tools.is_empty() {
            body["tools"] = json!(openai_tools(tools));
            body["tool_choice"] = json!("auto");
        }

        let resp = self
            .http
            .post(format!("{base_url}/chat/completions"))
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("API error for {model} (HTTP {status}): {text}");
        }

        let data: Value = resp.json().await?;
        let choice = data
            .get("choices")
            .and_then(|value| value.as_array())
            .and_then(|choices| choices.first())
            .ok_or_else(|| anyhow::anyhow!("completion response missing choices[0]"))?;
        let message = choice
            .get("message")
            .ok_or_else(|| anyhow::anyhow!("completion response missing choices[0].message"))?;

        let usage = Usage {
            input_tokens: data
                .get("usage")
                .and_then(|value| value.get("prompt_tokens"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32,
            output_tokens: data
                .get("usage")
                .and_then(|value| value.get("completion_tokens"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32,
        };

        let tool_calls = message
            .get("tool_calls")
            .and_then(|value| value.as_array())
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| {
                        let function = call.get("function")?;
                        let name = function.get("name")?.as_str()?.to_string();
                        let arguments = function
                            .get("arguments")
                            .and_then(|value| value.as_str())
                            .map(parse_tool_arguments)
                            .unwrap_or_else(|| json!({}));
                        Some(ToolCall {
                            id: call
                                .get("id")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name,
                            input: arguments,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(LlmResponse {
            text: parse_openai_content(message),
            tool_calls,
            usage,
            stop_reason: choice
                .get("finish_reason")
                .and_then(|value| value.as_str())
                .unwrap_or("stop")
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::api_model_name;
    use super::route_for_model;
    use super::ApiRoute;

    #[test]
    fn route_for_model_matches_expected_provider_family() {
        assert_eq!(route_for_model("qwen/qwen3-32b"), ApiRoute::Groq);
        assert_eq!(route_for_model("anthropic/claude-sonnet-4-6"), ApiRoute::Anthropic);
        assert_eq!(route_for_model("claude-sonnet-4-6"), ApiRoute::Anthropic);
        assert_eq!(route_for_model("inception/mercury-2"), ApiRoute::Inception);
        assert_eq!(route_for_model("openai/gpt-4.1"), ApiRoute::OpenAi);
        assert_eq!(route_for_model("gpt-4.1"), ApiRoute::OpenAi);
    }

    #[test]
    fn api_model_name_strips_only_provider_prefixes_that_require_it() {
        assert_eq!(
            api_model_name("anthropic/claude-sonnet-4-6", ApiRoute::Anthropic),
            "claude-sonnet-4-6"
        );
        assert_eq!(api_model_name("inception/mercury-2", ApiRoute::Inception), "mercury-2");
        assert_eq!(api_model_name("qwen/qwen3-32b", ApiRoute::Groq), "qwen/qwen3-32b");
        assert_eq!(api_model_name("openai/gpt-4.1", ApiRoute::OpenAi), "gpt-4.1");
        assert_eq!(api_model_name("gpt-4.1", ApiRoute::OpenAi), "gpt-4.1");
    }
}
