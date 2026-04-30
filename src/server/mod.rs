// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

mod handlers;
mod state;

use std::sync::Arc;

use axum::middleware;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use serde_json::Value;
pub use state::AppState;
pub use state::DispatchedTracker;

pub fn build_router(state: Arc<AppState>) -> Router {
    // Admin routes share the same listener as the public ones, but
    // are isolated by the `require_admin_auth` middleware (loopback
    // peer-addr + matching admin Bearer). This stays correct even
    // when the daemon binds to `0.0.0.0` for remote MCP — admin
    // paths just refuse non-loopback callers regardless of their
    // bearer.
    let admin = Router::new()
        .route("/admin/oauth/clients", get(handlers::admin::oauth::clients::list))
        .route("/admin/oauth/clients", post(handlers::admin::oauth::clients::register))
        .route("/admin/oauth/clients/{client_id}", delete(handlers::admin::oauth::clients::revoke))
        .route("/admin/oauth/sessions", get(handlers::admin::oauth::sessions::list))
        .route("/admin/oauth/sessions/{session_id}", delete(handlers::admin::oauth::sessions::drop))
        .route(
            "/admin/oauth/sessions/{session_id}/pin",
            post(handlers::admin::oauth::sessions::issue_pin),
        )
        .route("/admin/oauth/tokens", get(handlers::admin::oauth::tokens::list))
        .route("/admin/oauth/tokens/{token_id}", delete(handlers::admin::oauth::tokens::revoke))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            handlers::admin::middleware::require_admin_auth,
        ));

    // `/mcp` accepts loopback callers (stdio mcp-proxy) without a
    // Bearer; remote callers must present `Authorization: Bearer
    // <access_token>`. Mounted on a sub-router so the middleware
    // applies only to `/mcp` and not to `/health` / `/oauth/*`.
    let mcp = Router::new().route("/mcp", post(handlers::mcp::handle)).layer(
        middleware::from_fn_with_state(
            Arc::clone(&state),
            handlers::mcp_auth::require_bearer_or_loopback,
        ),
    );

    Router::new()
        .route("/health", get(handlers::health::handle))
        .route("/.well-known/oauth-authorization-server", get(handlers::oauth::metadata::handle))
        .route("/oauth/register", post(handlers::oauth::register::handle))
        .route("/oauth/authorize", get(handlers::oauth::authorize::handle_get))
        .route("/oauth/authorize", post(handlers::oauth::authorize::handle_post))
        .route("/oauth/token", post(handlers::oauth::token::handle))
        .route("/oauth/revoke", post(handlers::oauth::revoke::handle))
        .merge(mcp)
        .merge(admin)
        .with_state(state)
}

/// Public entry for courier to trigger task execution.
pub async fn handle_agent_start_pub(state: &AppState, args: &Value) -> Value {
    handlers::mcp::start::handle(state, args).await
}
