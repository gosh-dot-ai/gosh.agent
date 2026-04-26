// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::path::PathBuf;
use std::time::Duration;

use anyhow::bail;
use anyhow::Result;

/// Agent runtime configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Whether this agent is enabled.
    pub enabled: bool,
    /// Fraction of budget reserved for review phase.
    pub review_budget_reserve: f64,
    /// If estimated effort > budget × this, reject as too_complex.
    pub too_complex_threshold: f64,
    /// Max retries after failed review.
    pub max_retries: u32,
    /// Maximum number of tasks allowed to run concurrently.
    pub max_parallel_tasks: usize,
    /// Maximum time for internal bootstrap memory control-plane calls.
    pub bootstrap_memory_timeout: Duration,
    /// Optional override for local emergency failure artifacts.
    pub local_failure_artifact_dir: Option<PathBuf>,
    /// Maximum number of local emergency failure artifacts retained per agent.
    /// A value of 0 disables pruning.
    pub local_failure_artifact_retention: usize,
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_parallel_tasks == 0 {
            bail!("max_parallel_tasks must be >= 1");
        }
        if !(0.0..1.0).contains(&self.review_budget_reserve) {
            bail!("review_budget_reserve must be >= 0.0 and < 1.0");
        }
        if !(0.0..=1.0).contains(&self.too_complex_threshold) || self.too_complex_threshold == 0.0 {
            bail!("too_complex_threshold must be > 0.0 and <= 1.0");
        }
        Ok(())
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            review_budget_reserve: 0.2,
            too_complex_threshold: 0.8,
            max_retries: 2,
            max_parallel_tasks: 4,
            bootstrap_memory_timeout: Duration::from_secs(60),
            local_failure_artifact_dir: None,
            local_failure_artifact_retention: 512,
        }
    }
}
