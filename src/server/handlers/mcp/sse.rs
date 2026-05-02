// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::server::AppState;

const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");

pub async fn handle(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let session_id = headers
        .get(&MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let stream = state.mcp_events.subscribe(&session_id).await.map(|event| {
        let data = serde_json::to_string(&event.data).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"error","data":"failed to serialize SSE event"}}"#.to_string()
        });
        Ok::<_, Infallible>(Event::default().event(event.event).data(data))
    });

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keepalive"))
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&session_id) {
        response.headers_mut().insert(MCP_SESSION_ID, value);
    }
    response
}
