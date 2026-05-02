// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

use std::net::SocketAddr;
use std::time::Instant;

use axum::extract::ConnectInfo;
use axum::http::header::HeaderName;
use axum::http::header::HeaderValue;
use axum::http::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;
use uuid::Uuid;

static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

pub async fn log_request(req: Request<axum::body::Body>, next: Next) -> Response {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let remote = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.to_string())
        .unwrap_or_else(|| "-".to_string());

    let span = tracing::info_span!(
        target: "gosh_agent::http",
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
        remote = %remote,
    );

    let mut response = next.run(req).instrument(span.clone()).await;
    let status = response.status();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER.clone(), value);
    }

    let latency_ms = started.elapsed().as_millis();
    let _entered = span.enter();
    if status.is_server_error() {
        tracing::warn!(
            target: "gosh_agent::http",
            request_id = %request_id,
            method = %method,
            path = %path,
            remote = %remote,
            status = status.as_u16(),
            latency_ms,
            "request completed"
        );
    } else if path == "/health" && status == StatusCode::OK {
        tracing::debug!(
            target: "gosh_agent::http",
            request_id = %request_id,
            method = %method,
            path = %path,
            remote = %remote,
            status = status.as_u16(),
            latency_ms,
            "request completed"
        );
    } else {
        tracing::info!(
            target: "gosh_agent::http",
            request_id = %request_id,
            method = %method,
            path = %path,
            remote = %remote,
            status = status.as_u16(),
            latency_ms,
            "request completed"
        );
    }

    response
}
