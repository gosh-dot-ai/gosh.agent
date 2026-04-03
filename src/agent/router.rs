// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

use super::config::RoutingTier;

/// Select routing tier based on complexity score (0.0–1.0).
pub fn select_tier(score: f64) -> RoutingTier {
    if score < 0.3 {
        RoutingTier::Fast
    } else if score < 0.6 {
        RoutingTier::Balanced
    } else {
        RoutingTier::Strong
    }
}

/// Check if task is too complex for the given budget using the selected profile
/// cost.
pub fn is_too_complex(context_tokens: u32, budget: f64, threshold: f64, cost_per_1k: f64) -> bool {
    let effort = cost_per_1k * (context_tokens as f64 / 1000.0);
    effort > budget * threshold
}

/// Refine complexity score with heuristics.
/// Boosts score if task text contains complexity keywords or context is large.
pub fn refine_score(base_score: f64, context_tokens: u32) -> f64 {
    let mut score = base_score;

    // Large context → more complex
    if context_tokens > 8000 {
        score += 0.1;
    }
    if context_tokens > 16000 {
        score += 0.1;
    }

    score.clamp(0.0, 1.0)
}
