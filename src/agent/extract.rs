// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use anyhow::bail;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::llm::LlmProvider;
use crate::llm::Message;

pub const EXTRACT_SYSTEM: &str = r#"You are a fact extractor. Given a task description, extract atomic, self-contained facts.

Output a JSON object:
{
  "facts": [
    {
      "id": "f_01",
      "fact": "Self-contained fact text with exact values (names, numbers, dates)",
      "kind": "fact|constraint|rule|decision|action_item|preference",
      "entities": ["Entity1", "Entity2"],
      "tags": ["tag1"]
    }
  ]
}

Rules:
- ONE fact per item. Two pieces of info = two facts.
- Preserve exact values: names, numbers, URLs, dates. Never paraphrase.
- Use the most specific "kind": constraint > rule > decision > action_item > preference > fact.
- "entities" = named things mentioned (people, services, APIs, databases, etc).
- "tags" should capture domain topics.
- Aim for thorough coverage — do not skip details.
- Output ONLY valid JSON, no markdown fences."#;

#[derive(Debug, Deserialize)]
struct ExtractionResult {
    facts: Vec<ExtractedFact>,
}

#[derive(Debug, Deserialize)]
struct ExtractedFact {
    id: String,
    fact: String,
    kind: Option<String>,
    entities: Option<Vec<String>>,
    tags: Option<Vec<String>>,
}

/// Fact ready for ingestion into memory.
#[derive(Debug, Serialize)]
pub struct TaskFact {
    pub id: String,
    pub fact: String,
    pub kind: String,
    pub session: i32,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
}

pub fn build_extract_user_content(task_id: &str, description: &str) -> String {
    format!("Task ID: {task_id}\n\nTask description:\n{description}")
}

pub fn parse_task_facts(task_id: &str, text: &str) -> Result<Vec<TaskFact>> {
    let parsed: ExtractionResult = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("failed to parse extraction result: {e}\nraw: {text}"))?;

    if parsed.facts.is_empty() {
        bail!("extraction produced no facts");
    }

    let facts = parsed
        .facts
        .into_iter()
        .map(|f| {
            let mut entities = f.entities.unwrap_or_default();
            if !entities.contains(&task_id.to_string()) {
                entities.push(task_id.to_string());
            }

            let mut tags = f.tags.unwrap_or_default();
            if !tags.contains(&"task".to_string()) {
                tags.push("task".to_string());
            }
            tags.push(format!("task:{task_id}"));

            TaskFact {
                id: f.id,
                fact: f.fact,
                kind: f.kind.unwrap_or_else(|| "fact".to_string()),
                session: 1,
                entities,
                tags,
            }
        })
        .collect();

    Ok(facts)
}

/// Extract structured facts from a task description using LLM.
#[allow(dead_code)]
pub async fn extract_task_facts(
    llm: &dyn LlmProvider,
    model: &str,
    task_id: &str,
    description: &str,
) -> Result<Vec<TaskFact>> {
    let user_content = build_extract_user_content(task_id, description);
    let messages = vec![Message { role: "user".to_string(), content: user_content }];

    let response = llm.chat(model, EXTRACT_SYSTEM, &messages, &[], 4096).await?;

    let text = response.text.unwrap_or_default();
    parse_task_facts(task_id, &text)
}

/// Convert extracted facts to JSON Value for ingest_asserted_facts.
pub fn facts_to_value(facts: &[TaskFact]) -> Value {
    serde_json::to_value(facts).unwrap_or_default()
}
