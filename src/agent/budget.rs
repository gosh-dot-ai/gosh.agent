// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use super::pricing::ModelPricing;
use crate::llm::Usage;

pub struct BudgetController {
    budget: f64,
    spent: f64,
    review_reserve: f64,
}

impl BudgetController {
    pub fn new(budget_shell: f64, review_reserve_pct: f64) -> Self {
        Self { budget: budget_shell, spent: 0.0, review_reserve: review_reserve_pct }
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

pub fn estimate_from_usage(pricing: &ModelPricing, usage: &Usage) -> f64 {
    let uncached_input_tokens = usage
        .input_tokens
        .saturating_sub(usage.cached_input_read_tokens)
        .saturating_sub(usage.cached_input_write_tokens);
    ((uncached_input_tokens as f64) * pricing.input_per_1k
        + (usage.output_tokens as f64) * pricing.output_per_1k
        + (usage.reasoning_tokens as f64) * pricing.reasoning_per_1k
        + (usage.cached_input_read_tokens as f64) * pricing.cache_read_per_1k
        + (usage.cached_input_write_tokens as f64) * pricing.cache_write_per_1k)
        / 1000.0
}

pub fn estimate_preflight_cost(
    pricing: &ModelPricing,
    estimated_input_tokens: u32,
    max_output_tokens: u32,
) -> f64 {
    let conservative_output_rate = pricing.output_per_1k.max(pricing.reasoning_per_1k);
    ((estimated_input_tokens as f64) * pricing.input_per_1k
        + (max_output_tokens as f64) * conservative_output_rate)
        / 1000.0
}

#[cfg(test)]
mod tests {
    use super::estimate_from_usage;
    use super::estimate_preflight_cost;
    use super::BudgetController;
    use super::Phase;
    use crate::agent::pricing::ModelPricing;
    use crate::llm::Usage;

    fn pricing() -> ModelPricing {
        ModelPricing {
            input_per_1k: 2.0,
            output_per_1k: 8.0,
            reasoning_per_1k: 4.0,
            cache_read_per_1k: 0.5,
            cache_write_per_1k: 1.5,
        }
    }

    #[test]
    fn cost_formula_includes_all_billable_dimensions() {
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            reasoning_tokens: 250,
            cached_input_read_tokens: 200,
            cached_input_write_tokens: 100,
        };

        let cost = estimate_from_usage(&pricing(), &usage);
        assert!((cost - 6.65).abs() < 1e-9);
    }

    #[test]
    fn preflight_estimate_uses_input_and_max_output_with_conservative_output_rate() {
        let cost = estimate_preflight_cost(&pricing(), 1200, 600);
        assert!((cost - 7.2).abs() < 1e-9);
    }

    #[test]
    fn preflight_estimate_uses_reasoning_rate_when_it_exceeds_output_rate() {
        let mut reasoning_heavy_pricing = pricing();
        reasoning_heavy_pricing.output_per_1k = 4.0;
        reasoning_heavy_pricing.reasoning_per_1k = 10.0;

        let cost = estimate_preflight_cost(&reasoning_heavy_pricing, 1000, 500);
        assert!((cost - 7.0).abs() < 1e-9);
    }

    #[test]
    fn cached_tokens_are_not_double_billed_as_regular_input() {
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 0,
            reasoning_tokens: 0,
            cached_input_read_tokens: 400,
            cached_input_write_tokens: 100,
        };

        let cost = estimate_from_usage(&pricing(), &usage);
        let expected = ((500.0 * pricing().input_per_1k)
            + (400.0 * pricing().cache_read_per_1k)
            + (100.0 * pricing().cache_write_per_1k))
            / 1000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn post_charge_uses_full_runtime_usage() {
        let mut controller = BudgetController::new(20.0, 0.2);
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            reasoning_tokens: 250,
            cached_input_read_tokens: 200,
            cached_input_write_tokens: 100,
        };

        let cost = estimate_from_usage(&pricing(), &usage);
        assert!(controller.can_afford(cost, Phase::Execution));
        controller.charge(cost);

        assert!((controller.spent() - cost).abs() < 1e-9);
    }
}
