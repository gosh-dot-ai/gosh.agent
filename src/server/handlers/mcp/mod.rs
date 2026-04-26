// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

pub mod courier_subscribe;
pub mod courier_unsubscribe;
pub mod create_task;
pub mod start;
pub mod status;
pub mod task_list;

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use serde_json::Value;

use crate::server::AppState;

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = body.get("id").cloned();
    let params = body.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => {
            let mut counter = state.session_counter.lock().await;
            *counter += 1;
            let sid = format!("{:032x}", *counter);
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "gosh-agent", "version": "0.1.0" }
                }
            });
            (
                StatusCode::OK,
                [("content-type", "application/json"), ("Mcp-Session-Id", &sid)],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response()
        }

        "notifications/initialized" => (StatusCode::OK, "").into_response(),

        "tools/call" => {
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = match tool_name {
                "agent_start" => start::handle(&state, &args).await,
                "agent_status" => status::handle(&state, &args).await,
                "agent_create_task" => create_task::handle(&state, &args).await,
                "agent_task_list" => task_list::handle(&state, &args).await,
                "agent_courier_subscribe" => courier_subscribe::handle(state.clone(), &args).await,
                "agent_courier_unsubscribe" => courier_unsubscribe::handle(&state).await,
                _ => json!({
                    "error": format!("unknown tool: {tool_name}"),
                    "code": "UNKNOWN_TOOL"
                }),
            };

            let is_error =
                result.get("error").is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap() }],
                    "isError": is_error,
                }
            });

            let sid = headers
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();

            (
                StatusCode::OK,
                [("content-type", "application/json"), ("Mcp-Session-Id", sid.as_str())],
                serde_json::to_string(&resp).unwrap(),
            )
                .into_response()
        }

        _ => (StatusCode::OK, "").into_response(),
    }
}
