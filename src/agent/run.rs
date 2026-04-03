// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use tracing::info;
use tracing::warn;

use super::budget::BudgetController;
use super::budget::Phase;
use super::cli::render_cli_prompt;
use super::cli::CliExecutorManager;
use super::config::profile_by_id;
use super::config::profile_by_model_id;
use super::config::AgentConfig;
use super::config::ModelBackend;
use super::config::ModelProfile;
use super::extract;
use super::resolve;
use super::router;
use super::task::TaskResult;
use super::task::TaskState;
use super::task::TaskStatus;
use super::task::ToolTraceEntry;
use crate::client::memory::IngestFactsParams;
use crate::client::memory::ListFactsParams;
use crate::client::memory::MemoryMcpClient;
use crate::client::memory::RecallParams;
use crate::llm::LlmProvider;
use crate::llm::Message;
use crate::llm::ToolCall;
use crate::llm::ToolDef;
use crate::llm::Usage;

const SYSTEM_PROMPT: &str = r#"You are an autonomous task executor. You have access to memory tools to retrieve context and information. Execute the task precisely and thoroughly.

When you are done, respond with your final answer as plain text. Do NOT call any more tools once you have the answer."#;

const REVIEW_PROMPT: &str = r#"You are a quality reviewer. Evaluate whether the execution result correctly and completely addresses the original task.

Respond with a JSON object:
{"verdict": "ok"} if the result is satisfactory.
{"verdict": "retry", "reason": "..."} if it needs improvement."#;

pub struct Agent {
    pub config: AgentConfig,
    pub memory: Arc<MemoryMcpClient>,
    api_llm: Option<Arc<dyn LlmProvider>>,
    cli: Arc<CliExecutorManager>,
}

impl Agent {
    pub fn new(
        config: AgentConfig,
        memory: Arc<MemoryMcpClient>,
        api_llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self { config, memory, api_llm, cli: CliExecutorManager::new() }
    }

    pub fn with_config(&self, config: AgentConfig) -> Self {
        Self {
            config,
            memory: self.memory.clone(),
            api_llm: self.api_llm.clone(),
            cli: self.cli.clone(),
        }
    }

    pub async fn extract_task_facts(
        &self,
        task_id: &str,
        description: &str,
    ) -> Result<Vec<extract::TaskFact>> {
        let profile = self.config.extraction_profile()?;
        let messages = vec![Message {
            role: "user".to_string(),
            content: extract::build_extract_user_content(task_id, description),
        }];

        let response =
            self.complete_text(profile, extract::EXTRACT_SYSTEM, &messages, 4096, None).await?;

        extract::parse_task_facts(task_id, &response.text)
    }

    /// Run a task end-to-end. Blocking until completion.
    pub async fn run(
        &self,
        agent_id: &str,
        swarm_id: &str,
        task_id: &str,
        key: &str,
        budget_shell: f64,
    ) -> Result<TaskResult> {
        let mut state = TaskState::new(task_id, agent_id, swarm_id, key, budget_shell);
        let mut budget = BudgetController::new(budget_shell, self.config.review_budget_reserve);

        info!(task_id, "bootstrap: resolving task from memory");
        state.phase = "bootstrap".to_string();

        let resolved =
            match resolve::resolve_task(&self.memory, task_id, agent_id, key, swarm_id).await {
                Ok(r) => r,
                Err(e) => {
                    state.status = TaskStatus::Failed;
                    state.error = Some(format!("RESOLVE_FAILED: {e}"));
                    return Ok(TaskResult {
                        task_id: task_id.to_string(),
                        status: state.status,
                        shell_spent: 0.0,
                        artifacts_written: vec![],
                        result: None,
                        error: state.error,
                    });
                }
            };

        state.task_fact_id = Some(resolved.task_fact_id.clone());
        state.external_task_id = resolved.external_task_id.clone();

        info!(task_id, task_fact_id = %resolved.task_fact_id, "task resolved");

        let mut context = resolved.fact.clone();
        let recall_result = match self
            .memory
            .recall(RecallParams {
                key: state.key.clone(),
                agent_id: agent_id.to_string(),
                swarm_id: swarm_id.to_string(),
                query: resolved.fact.clone(),
                token_budget: 4000,
            })
            .await
        {
            Ok(recall_result) => {
                if let Some(extra) = recall_result.get("context").and_then(|v| v.as_str()) {
                    if !extra.is_empty() && extra != context {
                        context.push_str("\n\n--- Additional context ---\n");
                        context.push_str(extra);
                    }
                }
                Some(recall_result)
            }
            Err(e) => {
                warn!(task_id, error = %e, "semantic recall failed, continuing with exact task");
                None
            }
        };

        let payload_plan =
            recall_result.as_ref().and_then(|value| self.execution_plan_from_recall(value));

        if context.is_empty() {
            state.status = TaskStatus::Failed;
            state.error = Some("NO_TASK_CONTEXT: resolved task has empty fact text".to_string());
            mark_finished(&mut state);
            return Ok(TaskResult {
                task_id: task_id.to_string(),
                status: state.status,
                shell_spent: 0.0,
                artifacts_written: vec![],
                result: None,
                error: state.error,
            });
        }

        let complexity_score =
            complexity_score_from_recall_or_fact(recall_result.as_ref(), &resolved.raw);
        let token_estimate = (context.len() / 4) as u32;
        let score = router::refine_score(complexity_score, token_estimate);

        let mut routing_tier = router::select_tier(score);
        let mut profile = payload_plan
            .as_ref()
            .map(|plan| plan.profile)
            .unwrap_or(self.config.execution_profile(routing_tier)?);
        state.model_current = profile.id.to_string();

        if router::is_too_complex(
            token_estimate,
            budget_shell,
            self.config.too_complex_threshold,
            profile.cost_per_1k,
        ) {
            info!(task_id, score, profile = profile.id, "task is too complex for budget");
            state.status = TaskStatus::TooComplex;
            mark_finished(&mut state);
            return Ok(TaskResult {
                task_id: task_id.to_string(),
                status: state.status,
                shell_spent: 0.0,
                artifacts_written: vec![],
                result: None,
                error: None,
            });
        }

        let workdir = extract_workdir(&resolved.raw);
        info!(task_id, profile = profile.id, score, "starting execution");

        state.phase = "execution".to_string();
        let mut retries = 0u32;
        let mut retry_reason: Option<String> = None;

        loop {
            let exec_result = self
                .execution_loop(
                    &mut state,
                    &mut budget,
                    &context,
                    profile,
                    workdir.as_deref(),
                    retry_reason.as_deref(),
                    if retry_reason.is_none() { payload_plan.as_ref() } else { None },
                )
                .await;

            let result_text = match exec_result {
                Ok(text) => text,
                Err(e) => {
                    state.status = TaskStatus::Failed;
                    state.error = Some(e.to_string());
                    break;
                }
            };

            state.result = Some(result_text.clone());

            state.phase = "review".to_string();
            let review_profile = self.config.review_profile()?;

            match self
                .review(
                    &state,
                    &mut budget,
                    &context,
                    &result_text,
                    review_profile,
                    workdir.as_deref(),
                )
                .await
            {
                Ok(ReviewVerdict::Ok) => {
                    info!(task_id, "review passed");
                    state.status = TaskStatus::Done;
                    break;
                }
                Ok(ReviewVerdict::Retry(reason)) => {
                    retries += 1;
                    if retries > self.config.max_retries {
                        warn!(task_id, "max retries exceeded, accepting result");
                        state.status = TaskStatus::Done;
                        break;
                    }
                    if budget.execution_remaining() <= 0.0 {
                        warn!(task_id, "no budget for retry");
                        state.status = TaskStatus::PartialBudgetOverdraw;
                        break;
                    }

                    if let Some(next) = routing_tier.escalate() {
                        let next_profile = self.config.execution_profile(next)?;
                        info!(
                            task_id,
                            from = profile.id,
                            to = next_profile.id,
                            "escalating execution profile"
                        );
                        routing_tier = next;
                        profile = next_profile;
                        state.model_current = profile.id.to_string();
                    }

                    retry_reason = Some(reason);
                    state.phase = "execution".to_string();
                    continue;
                }
                Err(e) => {
                    warn!(task_id, error = %e, "review failed, accepting result");
                    state.status = TaskStatus::Done;
                    break;
                }
            }
        }

        state.shell_spent = budget.spent();
        mark_finished(&mut state);
        let (artifacts, persist_error) = match self.persist_results(&state).await {
            Ok(a) => (a, None),
            Err(e) => {
                tracing::error!(task_id = %task_id, error = %e, "failed to persist task results to memory");
                (vec![], Some(e.to_string()))
            }
        };

        // If persist failed, task cannot be considered done — artifacts are missing.
        if persist_error.is_some() && state.status == TaskStatus::Done {
            state.status = TaskStatus::Failed;
            state.error =
                Some(format!("persist failed: {}", persist_error.as_deref().unwrap_or("unknown")));
        }

        Ok(TaskResult {
            task_id: task_id.to_string(),
            status: state.status.clone(),
            shell_spent: budget.spent(),
            artifacts_written: artifacts,
            result: state.result.clone(),
            error: state.error.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn execution_loop(
        &self,
        state: &mut TaskState,
        budget: &mut BudgetController,
        context: &str,
        profile: &'static ModelProfile,
        workdir: Option<&str>,
        retry_reason: Option<&str>,
        payload_plan: Option<&RecallExecutionPlan>,
    ) -> Result<String> {
        if profile.backend.supports_tools() {
            return self
                .execution_loop_api(state, budget, context, profile, retry_reason, payload_plan)
                .await;
        }

        self.execution_once_cli(
            state,
            budget,
            context,
            profile,
            workdir,
            retry_reason,
            payload_plan,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execution_once_cli(
        &self,
        state: &mut TaskState,
        budget: &mut BudgetController,
        context: &str,
        profile: &'static ModelProfile,
        workdir: Option<&str>,
        retry_reason: Option<&str>,
        payload_plan: Option<&RecallExecutionPlan>,
    ) -> Result<String> {
        state.iteration += 1;

        let estimated = budget.estimate_cost(4000, profile);
        if !budget.can_afford(estimated, Phase::Execution) {
            state.status = TaskStatus::PartialBudgetOverdraw;
            return Ok(format!("(budget exhausted before running profile {})", profile.id));
        }

        let response = if let Some(plan) = payload_plan.filter(|plan| !plan.use_tool) {
            self.complete_messages(profile, "", &plan.messages, plan.max_tokens, workdir).await?
        } else {
            let mut user_content = format!("Task context (from memory):\n{context}\n");
            if let Some(reason) = retry_reason {
                user_content.push_str(&format!(
                    "\nPrevious attempt failed review: {reason}\nFix the issues.\n"
                ));
            }

            let messages = vec![Message { role: "user".to_string(), content: user_content }];

            self.complete_text(profile, SYSTEM_PROMPT, &messages, 4096, workdir).await?
        };
        budget.charge(
            budget
                .estimate_cost(response.usage.input_tokens + response.usage.output_tokens, profile),
        );
        state.shell_spent = budget.spent();
        Ok(response.text)
    }

    async fn execution_loop_api(
        &self,
        state: &mut TaskState,
        budget: &mut BudgetController,
        context: &str,
        profile: &'static ModelProfile,
        retry_reason: Option<&str>,
        payload_plan: Option<&RecallExecutionPlan>,
    ) -> Result<String> {
        let llm = self.require_api_llm(profile)?;
        let tools = self.memory_tools();
        let mut tool_trace_text: Vec<String> = Vec::new();

        loop {
            state.iteration += 1;

            let estimated = budget.estimate_cost(4000, profile);
            if !budget.can_afford(estimated, Phase::Execution) {
                state.status = TaskStatus::PartialBudgetOverdraw;
                let partial = tool_trace_text.join("\n");
                return Ok(format!(
                    "(budget exhausted after {} iterations)\n{partial}",
                    state.iteration
                ));
            }

            let use_prebuilt = state.iteration == 1
                && retry_reason.is_none()
                && tool_trace_text.is_empty()
                && payload_plan.is_some()
                && !payload_plan.unwrap().use_tool;

            let response = if use_prebuilt {
                let plan = payload_plan.unwrap();
                llm.chat(profile.model_id, "", &plan.messages, &[], plan.max_tokens).await?
            } else {
                let mut user_content = format!("Task context (from memory):\n{context}\n");
                if let Some(reason) = retry_reason {
                    if state.iteration == 1 {
                        user_content.push_str(&format!(
                            "\nPrevious attempt failed review: {reason}\nFix the issues.\n"
                        ));
                    }
                }

                if !tool_trace_text.is_empty() {
                    user_content.push_str("\nTool results so far:\n");
                    for t in &tool_trace_text {
                        user_content.push_str(t);
                        user_content.push('\n');
                    }
                }

                let messages = vec![Message { role: "user".to_string(), content: user_content }];

                llm.chat(profile.model_id, SYSTEM_PROMPT, &messages, &tools, 4096).await?
            };

            let tokens = response.usage.input_tokens + response.usage.output_tokens;
            budget.charge(budget.estimate_cost(tokens, profile));
            state.shell_spent = budget.spent();

            if response.tool_calls.is_empty() {
                return Ok(response.text.unwrap_or_default());
            }

            for tc in &response.tool_calls {
                let tool_result = self.execute_tool(tc, state).await;
                let (output, success) = match tool_result {
                    Ok(v) => (serde_json::to_string(&v).unwrap_or_default(), true),
                    Err(e) => (format!("error: {e}"), false),
                };

                let truncated = if output.len() > 2000 {
                    let end = output
                        .char_indices()
                        .take_while(|(i, _)| *i <= 2000)
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    format!("{}...(truncated)", &output[..end])
                } else {
                    output.clone()
                };

                tool_trace_text.push(format!("[{}] → {truncated}", tc.name));
                state.tool_trace.push(ToolTraceEntry { tool: tc.name.clone(), success });
            }
        }
    }

    async fn review(
        &self,
        state: &TaskState,
        budget: &mut BudgetController,
        context: &str,
        result: &str,
        profile: &'static ModelProfile,
        workdir: Option<&str>,
    ) -> Result<ReviewVerdict> {
        let estimated = budget.estimate_cost(4000, profile);
        if !budget.can_afford(estimated, Phase::Review) {
            info!("no budget for review, accepting result");
            return Ok(ReviewVerdict::Ok);
        }

        let user_content = format!(
            "Original task context:\n{context}\n\nExecution result:\n{result}\n\nTool trace ({} calls):\n{}",
            state.tool_trace.len(),
            state
                .tool_trace
                .iter()
                .map(|t| format!("[{}] success={}", t.tool, t.success))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let messages = vec![Message { role: "user".to_string(), content: user_content }];

        let response = self.complete_text(profile, REVIEW_PROMPT, &messages, 1024, workdir).await?;
        budget.charge(
            budget
                .estimate_cost(response.usage.input_tokens + response.usage.output_tokens, profile),
        );

        if let Ok(v) = serde_json::from_str::<Value>(&response.text) {
            if v.get("verdict").and_then(|v| v.as_str()) == Some("retry") {
                let reason = v
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("review rejected")
                    .to_string();
                return Ok(ReviewVerdict::Retry(reason));
            }
        }

        Ok(ReviewVerdict::Ok)
    }

    async fn complete_text(
        &self,
        profile: &'static ModelProfile,
        system: &str,
        messages: &[Message],
        max_tokens: u32,
        workdir: Option<&str>,
    ) -> Result<TextCompletion> {
        self.complete_messages(profile, system, messages, max_tokens, workdir).await
    }

    async fn complete_messages(
        &self,
        profile: &'static ModelProfile,
        system: &str,
        messages: &[Message],
        max_tokens: u32,
        workdir: Option<&str>,
    ) -> Result<TextCompletion> {
        match profile.backend {
            ModelBackend::AnthropicApi
            | ModelBackend::OpenAiApi
            | ModelBackend::GroqApi
            | ModelBackend::InceptionApi => {
                let llm = self.require_api_llm(profile)?;
                let response =
                    llm.chat(profile.model_id, system, messages, &[], max_tokens).await?;
                Ok(TextCompletion {
                    text: response.text.unwrap_or_default(),
                    usage: response.usage,
                })
            }
            ModelBackend::ClaudeCli | ModelBackend::CodexCli | ModelBackend::GeminiCli => {
                let prompt = render_cli_prompt(profile.backend, system, messages);
                let text = self.cli.run_prompt(&self.config, profile, &prompt, workdir).await?;
                Ok(TextCompletion {
                    // CLI token usage is heuristic because the official CLIs do not expose
                    // a stable token accounting contract here.
                    usage: estimate_text_usage(&prompt, &text),
                    text,
                })
            }
        }
    }

    fn require_api_llm(&self, profile: &ModelProfile) -> Result<&dyn LlmProvider> {
        self.api_llm.as_deref().with_context(|| {
            format!("profile {} requires an API LLM but none is configured", profile.id)
        })
    }

    async fn execute_tool(&self, tc: &ToolCall, state: &TaskState) -> Result<Value> {
        match tc.name.as_str() {
            "memory_recall" => {
                let query = tc.input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let token_budget =
                    tc.input.get("token_budget").and_then(|v| v.as_i64()).unwrap_or(4000);
                self.memory
                    .recall(RecallParams {
                        key: state.key.clone(),
                        agent_id: state.agent_id.clone(),
                        swarm_id: state.swarm_id.clone(),
                        query: query.to_string(),
                        token_budget,
                    })
                    .await
            }
            "memory_list" => {
                let kind = tc.input.get("kind").and_then(|v| v.as_str()).map(|s| s.to_string());
                let limit = tc.input.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
                self.memory
                    .list_facts(ListFactsParams {
                        key: state.key.clone(),
                        agent_id: state.agent_id.clone(),
                        swarm_id: state.swarm_id.clone(),
                        kind,
                        limit: Some(limit),
                    })
                    .await
            }
            _ => bail!("unknown tool: {}", tc.name),
        }
    }

    fn memory_tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "memory_recall".to_string(),
                description: "Semantic search over memory. Returns relevant facts and context."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" },
                        "kind": { "type": "string", "description": "Filter by fact kind (optional)" },
                        "token_budget": { "type": "integer", "description": "Max tokens to return (default 4000)" }
                    },
                    "required": ["query"]
                }),
            },
            ToolDef {
                name: "memory_list".to_string(),
                description: "List facts with optional kind filter and pagination.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "description": "Filter by fact kind (optional)" },
                        "limit": { "type": "integer", "description": "Max facts to return (default 20)" }
                    }
                }),
            },
        ]
    }

    async fn persist_results(&self, state: &TaskState) -> Result<Vec<String>> {
        let mut artifacts = Vec::new();
        let task_fact_id = state.task_fact_id.as_deref().unwrap_or(&state.task_id);
        let external_task_id = state.external_task_id.as_deref().unwrap_or(&state.task_id);

        if let Some(result) = &state.result {
            let canonical_result =
                json!([canonical_result_fact(result, state, task_fact_id, external_task_id,)]);

            self.memory
                .ingest_asserted_facts(IngestFactsParams {
                    key: state.key.clone(),
                    agent_id: state.agent_id.clone(),
                    swarm_id: state.swarm_id.clone(),
                    facts: canonical_result,
                })
                .await?;
            artifacts.push(format!("result:{}", external_task_id));

            let legacy_result = json!([{
                "fact": result,
                "kind": "fact",
                "id": format!("result_{}", external_task_id),
                "session": 1,
                "entities": [],
                "tags": ["agent_result", format!("task:{}", external_task_id)],
            }]);

            let _ = self
                .memory
                .ingest_asserted_facts(IngestFactsParams {
                    key: state.key.clone(),
                    agent_id: state.agent_id.clone(),
                    swarm_id: state.swarm_id.clone(),
                    facts: legacy_result,
                })
                .await;
        }

        let session_text = format!(
            "Agent {} completed task {} with status {}. Iterations: {}, shell spent: {:.2}, profile: {}",
            state.agent_id,
            external_task_id,
            state.status,
            state.iteration,
            state.shell_spent,
            state.model_current,
        );
        let canonical_session =
            json!([canonical_session_fact(&session_text, state, task_fact_id, external_task_id,)]);

        self.memory
            .ingest_asserted_facts(IngestFactsParams {
                key: state.key.clone(),
                agent_id: state.agent_id.clone(),
                swarm_id: state.swarm_id.clone(),
                facts: canonical_session,
            })
            .await?;
        artifacts.push(format!("session:{}", external_task_id));

        let legacy_session = json!([{
            "fact": session_text,
            "kind": "action",
            "id": format!("session_{}", external_task_id),
            "session": 1,
            "entities": [state.agent_id, external_task_id],
            "tags": ["agent_session", format!("task:{}", external_task_id)],
        }]);

        let _ = self
            .memory
            .ingest_asserted_facts(IngestFactsParams {
                key: state.key.clone(),
                agent_id: state.agent_id.clone(),
                swarm_id: state.swarm_id.clone(),
                facts: legacy_session,
            })
            .await;

        Ok(artifacts)
    }

    fn execution_plan_from_recall(&self, recall_result: &Value) -> Option<RecallExecutionPlan> {
        let payload = recall_result.get("payload")?;
        let payload_meta = recall_result.get("payload_meta");
        let messages = payload_messages(payload)?;
        let max_tokens = payload_max_tokens(payload).unwrap_or(4096);
        let use_tool = payload_meta
            .and_then(|meta| meta.get("use_tool"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        if let Some(profile_id) =
            payload_meta.and_then(|meta| meta.get("profile_used")).and_then(|value| value.as_str())
        {
            if let Some(profile) = profile_by_id(profile_id) {
                return Some(RecallExecutionPlan { profile, messages, max_tokens, use_tool });
            }
            if let Ok(profile) = self.config.resolve_profile(profile_id) {
                return Some(RecallExecutionPlan { profile, messages, max_tokens, use_tool });
            }
        }

        let model_id = payload.get("model").and_then(|value| value.as_str())?;
        let profile = self
            .config
            .allowed_profile_ids()
            .into_iter()
            .filter_map(|profile_id| self.config.resolve_profile(profile_id).ok())
            .find(|profile| {
                profile_by_model_id(model_id).map(|candidate| candidate.id) == Some(profile.id)
            })
            .or_else(|| profile_by_model_id(model_id))?;

        Some(RecallExecutionPlan { profile, messages, max_tokens, use_tool })
    }
}

#[derive(Debug)]
struct TextCompletion {
    text: String,
    usage: Usage,
}

struct RecallExecutionPlan {
    profile: &'static ModelProfile,
    messages: Vec<Message>,
    max_tokens: u32,
    use_tool: bool,
}

enum ReviewVerdict {
    Ok,
    Retry(String),
}

fn canonical_result_fact(
    result: &str,
    state: &TaskState,
    task_fact_id: &str,
    external_task_id: &str,
) -> Value {
    let summary = summarize_text(result);
    json!({
        "id": format!("task_result_{}", external_task_id),
        "fact": result,
        "kind": "task_result",
        "session": 1,
        "entities": [],
        "tags": ["agent_result", format!("task:{}", external_task_id)],
        "metadata": {
            "task_fact_id": task_fact_id,
            "task_id": external_task_id,
            "status": state.status.to_string(),
            "summary": summary,
            "result_text": result,
            "phase": state.phase.as_str(),
            "iteration": state.iteration,
            "profile_used": state.model_current.as_str(),
            "backend_used": backend_name(&state.model_current),
            "shell_spent": state.shell_spent,
            "started_at": state.started_at.as_str(),
            "finished_at": state.finished_at.as_deref(),
            "error": state.error.as_deref(),
            "tool_trace": state.tool_trace.iter().map(|t| format!("{}:{}", t.tool, if t.success { "ok" } else { "fail" })).collect::<Vec<_>>(),
        },
    })
}

fn canonical_session_fact(
    session_text: &str,
    state: &TaskState,
    task_fact_id: &str,
    external_task_id: &str,
) -> Value {
    json!({
        "id": format!("task_session_{}", external_task_id),
        "fact": session_text,
        "kind": "task_session",
        "session": 1,
        "entities": [state.agent_id.as_str(), external_task_id],
        "tags": ["agent_session", format!("task:{}", external_task_id)],
        "metadata": {
            "task_fact_id": task_fact_id,
            "task_id": external_task_id,
            "status": state.status.to_string(),
            "phase": state.phase.as_str(),
            "iteration": state.iteration,
            "shell_spent": state.shell_spent,
            "profile_used": state.model_current.as_str(),
            "backend_used": backend_name(&state.model_current),
            "started_at": state.started_at.as_str(),
            "finished_at": state.finished_at.as_deref(),
            "error": state.error.as_deref(),
            "tool_trace": state.tool_trace.iter().map(|t| format!("{}:{}", t.tool, if t.success { "ok" } else { "fail" })).collect::<Vec<_>>(),
        },
    })
}

fn extract_workdir(raw: &Value) -> Option<String> {
    raw.get("metadata")
        .and_then(|metadata| metadata.get("workdir").or_else(|| metadata.get("cwd")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn estimate_text_usage(prompt: &str, text: &str) -> Usage {
    Usage { input_tokens: (prompt.len() / 4) as u32, output_tokens: (text.len() / 4) as u32 }
}

fn summarize_text(text: &str) -> String {
    let mut chars = text.chars();
    let summary: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{summary}...")
    } else {
        summary
    }
}

fn backend_name(profile_id: &str) -> &'static str {
    match crate::agent::config::profile_by_id(profile_id).map(|profile| profile.backend) {
        Some(ModelBackend::AnthropicApi) => "anthropic_api",
        Some(ModelBackend::OpenAiApi) => "openai_api",
        Some(ModelBackend::GroqApi) => "groq_api",
        Some(ModelBackend::InceptionApi) => "inception_api",
        Some(ModelBackend::ClaudeCli) => "claude_cli",
        Some(ModelBackend::CodexCli) => "codex_cli",
        Some(ModelBackend::GeminiCli) => "gemini_cli",
        None => "unknown",
    }
}

fn payload_messages(payload: &Value) -> Option<Vec<Message>> {
    let raw_messages = payload.get("messages")?.as_array()?;
    let mut messages = Vec::with_capacity(raw_messages.len());
    for raw in raw_messages {
        messages.push(Message {
            role: raw.get("role")?.as_str()?.to_string(),
            content: payload_content_text(raw.get("content")?)?,
        });
    }
    Some(messages)
}

fn payload_content_text(raw: &Value) -> Option<String> {
    match raw {
        Value::String(text) => Some(text.to_string()),
        Value::Array(items) => {
            let mut chunks = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => chunks.push(text.to_string()),
                    Value::Object(map) => {
                        if map.get("type").and_then(|value| value.as_str()) == Some("text") {
                            chunks.push(map.get("text")?.as_str()?.to_string());
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            Some(chunks.join("\n"))
        }
        _ => None,
    }
}

fn mark_finished(state: &mut TaskState) {
    if state.finished_at.is_none() {
        state.finished_at = Some(Utc::now().to_rfc3339());
    }
}

fn payload_max_tokens(payload: &Value) -> Option<u32> {
    payload
        .get("max_tokens")
        .and_then(|value| value.as_u64())
        .or_else(|| payload.get("max_output_tokens").and_then(|value| value.as_u64()))
        .or_else(|| payload.get("max_completion_tokens").and_then(|value| value.as_u64()))
        .and_then(|value| u32::try_from(value).ok())
}

fn complexity_score_from_recall_or_fact(recall_result: Option<&Value>, task_fact: &Value) -> f64 {
    recall_result
        .and_then(|value| {
            value
                .get("complexity_hint")
                .and_then(|hint| hint.get("score"))
                .and_then(|score| score.as_f64())
        })
        .or_else(|| {
            task_fact
                .get("complexity_hint")
                .and_then(|hint| hint.get("score"))
                .and_then(|score| score.as_f64())
        })
        .unwrap_or(0.3)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use serde_json::Value;

    use super::canonical_result_fact;
    use super::canonical_session_fact;
    use super::complexity_score_from_recall_or_fact;
    use super::mark_finished;
    use super::Agent;
    use crate::agent::config::AgentConfig;
    use crate::agent::task::TaskState;
    use crate::agent::task::TaskStatus;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::McpTransport;

    struct DummyTransport;

    #[async_trait]
    impl McpTransport for DummyTransport {
        async fn send(
            &self,
            _body: &Value,
            _session_id: Option<&str>,
        ) -> anyhow::Result<(Value, Option<String>)> {
            Ok((json!({}), Some("dummy-session".to_string())))
        }
    }

    fn test_agent() -> Agent {
        let mut config = AgentConfig::default();
        config.fast_profile = "claude_code_cli".to_string();
        config.balanced_profile = "claude_code_cli".to_string();
        config.strong_profile = "claude_code_cli".to_string();
        Agent::new(config, Arc::new(MemoryMcpClient::new(DummyTransport)), None)
    }

    #[test]
    fn routing_prefers_recall_complexity_hint() {
        let recall = json!({"complexity_hint": {"score": 0.82}});
        let task_fact = json!({"complexity_hint": {"score": 0.11}});

        let score = complexity_score_from_recall_or_fact(Some(&recall), &task_fact);
        assert!((score - 0.82).abs() < f64::EPSILON);
    }

    #[test]
    fn routing_falls_back_to_task_fact_complexity_hint() {
        let task_fact = json!({"complexity_hint": {"score": 0.47}});

        let score = complexity_score_from_recall_or_fact(None, &task_fact);
        assert!((score - 0.47).abs() < f64::EPSILON);
    }

    #[test]
    fn routing_defaults_when_no_complexity_hint_exists() {
        let task_fact = json!({"fact": "simple task"});

        let score = complexity_score_from_recall_or_fact(None, &task_fact);
        assert!((score - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn canonical_result_fact_has_stable_top_level_id() {
        let mut state = TaskState::new("task-ext-1", "planner", "swarm", "key", 10.0);
        state.status = TaskStatus::Done;
        state.model_current = "claude_code_cli".to_string();
        state.finished_at = Some(state.started_at.clone());
        let fact = canonical_result_fact("done", &state, "fact-123", "task-ext-1");
        assert_eq!(fact.get("id").and_then(|v| v.as_str()), Some("task_result_task-ext-1"));
        assert_eq!(fact.get("kind").and_then(|v| v.as_str()), Some("task_result"));
        assert_eq!(
            fact.get("metadata").and_then(|m| m.get("task_fact_id")).and_then(|v| v.as_str()),
            Some("fact-123")
        );
        assert_eq!(
            fact.get("metadata").and_then(|m| m.get("backend_used")).and_then(|v| v.as_str()),
            Some("claude_cli")
        );
        assert_eq!(
            fact.get("metadata")
                .and_then(|m| m.get("tool_trace"))
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn canonical_session_fact_has_stable_top_level_id() {
        let mut state = TaskState::new("task-ext-1", "planner", "swarm", "key", 10.0);
        state.status = TaskStatus::Done;
        state.model_current = "claude_code_cli".to_string();
        state.finished_at = Some(state.started_at.clone());
        let fact = canonical_session_fact(
            "Agent planner completed task task-ext-1 with status done.",
            &state,
            "fact-123",
            "task-ext-1",
        );
        assert_eq!(fact.get("id").and_then(|v| v.as_str()), Some("task_session_task-ext-1"));
        assert_eq!(fact.get("kind").and_then(|v| v.as_str()), Some("task_session"));
        assert_eq!(
            fact.get("metadata").and_then(|m| m.get("task_id")).and_then(|v| v.as_str()),
            Some("task-ext-1")
        );
        assert_eq!(
            fact.get("metadata")
                .and_then(|m| m.get("tool_trace"))
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn execution_plan_prefers_profile_used_from_recall_payload() {
        let agent = test_agent();
        let recall = json!({
            "payload": {
                "model": "gpt-4.1-mini",
                "messages": [{"role": "user", "content": "Say hello"}],
                "max_tokens": 321
            },
            "payload_meta": {
                "profile_used": "claude_code_cli",
                "use_tool": false
            }
        });

        let plan = agent
            .execution_plan_from_recall(&recall)
            .expect("plan should be built from recall payload");

        assert_eq!(plan.profile.id, "claude_code_cli");
        assert_eq!(plan.max_tokens, 321);
        assert!(!plan.use_tool);
        assert_eq!(plan.messages.len(), 1);
        assert_eq!(plan.messages[0].content, "Say hello");
    }

    #[test]
    fn execution_plan_supports_structured_text_blocks() {
        let agent = test_agent();
        let recall = json!({
            "payload": {
                "model": "gpt-4.1-mini",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Line one"},
                        {"type": "text", "text": "Line two"}
                    ]
                }],
                "max_tokens": 128
            },
            "payload_meta": {
                "profile_used": "claude_code_cli",
                "use_tool": false
            }
        });

        let plan = agent
            .execution_plan_from_recall(&recall)
            .expect("structured payload should be accepted");

        assert_eq!(plan.messages[0].content, "Line one\nLine two");
    }

    #[test]
    fn execution_plan_accepts_known_memory_profile_even_when_not_in_agent_tiers() {
        let agent = test_agent();
        let recall = json!({
            "payload": {
                "model": "qwen/qwen3-32b",
                "messages": [{"role": "user", "content": "Use the fast path"}],
                "max_tokens": 256
            },
            "payload_meta": {
                "profile_used": "qwen_fast",
                "use_tool": false
            }
        });

        let plan = agent
            .execution_plan_from_recall(&recall)
            .expect("known memory profile should be reusable by the agent");

        assert_eq!(plan.profile.id, "qwen_fast");
        assert_eq!(plan.profile.model_id, "qwen/qwen3-32b");
    }

    #[test]
    fn execution_plan_matches_known_model_without_profile_hint() {
        let agent = test_agent();
        let recall = json!({
            "payload": {
                "model": "gpt-4.1",
                "messages": [{"role": "user", "content": "Use the max path"}],
                "max_tokens": 512
            },
            "payload_meta": {
                "use_tool": false
            }
        });

        let plan = agent
            .execution_plan_from_recall(&recall)
            .expect("known model id should map to a built-in execution profile");

        assert_eq!(plan.profile.id, "gpt41_max");
        assert_eq!(plan.profile.model_id, "gpt-4.1");
    }

    #[test]
    fn mark_finished_sets_timestamp_once() {
        let mut state = TaskState::new("task-ext-1", "planner", "swarm", "key", 10.0);
        assert!(state.finished_at.is_none());
        mark_finished(&mut state);
        assert!(state.finished_at.is_some());
        let first = state.finished_at.clone();
        mark_finished(&mut state);
        assert_eq!(state.finished_at, first);
    }
}
