// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use super::config::ModelProfile;

pub struct BudgetController {
    budget: f64,
    spent: f64,
    review_reserve: f64,
}

impl BudgetController {
    pub fn new(budget_shell: f64, review_reserve_pct: f64) -> Self {
        Self { budget: budget_shell, spent: 0.0, review_reserve: review_reserve_pct }
    }

    /// Estimate SHELL cost for a given token count and model profile.
    pub fn estimate_cost(&self, tokens: u32, profile: &ModelProfile) -> f64 {
        (tokens as f64 / 1000.0) * profile.cost_per_1k
    }

    /// Check if we can afford an estimated cost in the given phase.
    pub fn can_afford(&self, estimated: f64, phase: Phase) -> bool {
        let available = match phase {
            Phase::Execution => self.budget * (1.0 - self.review_reserve) - self.spent,
            Phase::Review => self.budget - self.spent,
        };
        available >= estimated
    }

    /// Charge SHELL cost after an LLM call.
    pub fn charge(&mut self, cost: f64) {
        self.spent += cost;
    }

    pub fn spent(&self) -> f64 {
        self.spent
    }

    #[allow(dead_code)]
    pub fn budget(&self) -> f64 {
        self.budget
    }

    #[allow(dead_code)]
    pub fn pct_used(&self) -> f64 {
        if self.budget <= 0.0 {
            return 1.0;
        }
        self.spent / self.budget
    }

    /// Remaining budget for execution (excludes review reserve).
    pub fn execution_remaining(&self) -> f64 {
        (self.budget * (1.0 - self.review_reserve) - self.spent).max(0.0)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Execution,
    Review,
}
