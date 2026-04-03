// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// License: MIT

mod handlers;
mod state;

use std::sync::Arc;

use axum::routing::get;
use axum::routing::post;
use axum::Router;
use serde_json::Value;
pub use state::AppState;
pub use state::DispatchedTracker;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(handlers::mcp::handle))
        .route("/health", get(handlers::health::handle))
        .with_state(state)
}

/// Public entry for courier to trigger task execution.
pub async fn handle_agent_start_pub(state: &AppState, args: &Value) -> Value {
    handlers::mcp::start::handle(state, args).await
}
