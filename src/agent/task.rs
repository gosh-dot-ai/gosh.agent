// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

/// Task status throughout its lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    PartialBudgetOverdraw,
    TooComplex,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Done => write!(f, "done"),
            Self::Failed => write!(f, "failed"),
            Self::PartialBudgetOverdraw => write!(f, "partial_budget_overdraw"),
            Self::TooComplex => write!(f, "too_complex"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableKind {
    Document,
    Code,
}

impl DeliverableKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Code => "code",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "document" => Some(Self::Document),
            "code" => Some(Self::Code),
            _ => None,
        }
    }
}

/// Running state of a task, updated each iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub task_id: String,
    pub agent_id: String,
    pub swarm_id: String,
    pub scope: String,
    /// Memory key (namespace) used for task/review/result persistence.
    pub work_key: String,
    /// Memory key (namespace) used for retrieval/source-of-truth context.
    pub context_key: String,
    pub status: TaskStatus,
    pub phase: String,
    pub iteration: u32,
    pub shell_spent: f64,
    pub shell_budget: f64,
    pub model_current: String,
    #[serde(default)]
    pub backend_current: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub tool_trace: Vec<ToolTraceEntry>,
    /// Persisted fact id — authoritative for execution/result linkage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_fact_id: Option<String>,
    /// Stable external user-facing task id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliverable_kind: Option<DeliverableKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliverable_fact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceEntry {
    pub tool: String,
    pub success: bool,
}

impl TaskState {
    #[allow(dead_code)]
    pub fn new(task_id: &str, agent_id: &str, swarm_id: &str, key: &str, budget: f64) -> Self {
        Self::with_keys(task_id, agent_id, swarm_id, key, key, budget)
    }

    pub fn with_keys(
        task_id: &str,
        agent_id: &str,
        swarm_id: &str,
        work_key: &str,
        context_key: &str,
        budget: f64,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            agent_id: agent_id.to_string(),
            swarm_id: swarm_id.to_string(),
            scope: "agent-private".to_string(),
            work_key: work_key.to_string(),
            context_key: context_key.to_string(),
            status: TaskStatus::Running,
            phase: "bootstrap".to_string(),
            iteration: 0,
            shell_spent: 0.0,
            shell_budget: budget,
            model_current: String::new(),
            backend_current: String::new(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            result: None,
            error: None,
            tool_trace: Vec::new(),
            task_fact_id: None,
            external_task_id: None,
            deliverable_kind: None,
            deliverable_fact_id: None,
            workspace_dir: None,
        }
    }

    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Done
                | TaskStatus::Failed
                | TaskStatus::PartialBudgetOverdraw
                | TaskStatus::TooComplex
        )
    }

    pub fn external_or_task_id(&self) -> &str {
        self.external_task_id.as_deref().unwrap_or(&self.task_id)
    }

    pub fn task_fact_or_task_id(&self) -> &str {
        self.task_fact_id.as_deref().unwrap_or(&self.task_id)
    }
}

/// Result returned from agent_start.
#[derive(Debug, Serialize)]
pub struct TaskResult {
    pub task_id: String,
    pub status: TaskStatus,
    pub shell_spent: f64,
    pub artifacts_written: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliverable_kind: Option<DeliverableKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliverable_fact_id: Option<String>,
}
