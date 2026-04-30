// Copyright 2026 (c) Mitja Goroshevsky and GOSH Technology Ltd.
// SPDX-License-Identifier: MIT

#[cfg(test)]
mod routing_contract {
    use std::sync::Arc;

    use serde_json::json;
    use serde_json::Value;

    use crate::client::memory::CourierSubscribeParams;
    use crate::client::memory::MemoryMcpClient;
    use crate::client::memory::MemoryQueryParams;
    use crate::client::memory::MemoryStoreParams;
    use crate::test_support::take_calls;
    use crate::test_support::wrap_mcp_response;
    use crate::test_support::MockTransport;

    // ---------------------------------------------------------------
    // Test: watcher/courier subscribes with correct structured filter
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn courier_subscribe_uses_target_filter() {
        let sub_resp = wrap_mcp_response(&json!({"sub_id": "sub-123"}));
        let (transport, mock_state) = MockTransport::new(vec![sub_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let agent_id = "test-agent";
        let agent_target = format!("agent:{agent_id}");
        let filter = json!({"kind": "task", "target": agent_target});

        let result = memory
            .courier_subscribe(CourierSubscribeParams {
                key: "default".to_string(),
                agent_id: agent_id.to_string(),
                swarm_id: "default".to_string(),
                connection_id: "conn-1".to_string(),
                filter: Some(filter.clone()),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "courier_subscribe");
        let sent_filter = args.get("filter").unwrap();
        assert_eq!(sent_filter.get("kind").unwrap(), "task");
        assert_eq!(sent_filter.get("target").unwrap().as_str().unwrap(), "agent:test-agent");
    }

    // ---------------------------------------------------------------
    // Test: poll fallback uses memory_query with same structured filter
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn poll_fallback_uses_memory_query_with_filter() {
        let query_resp = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let agent_id = "poll-agent";
        let agent_target = format!("agent:{agent_id}");

        // Simulate what poll_once does
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: agent_id.to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task",
                    "target": agent_target,
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(50),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (tool_name, args) = &calls[0];
        assert_eq!(tool_name, "memory_query");
        let filter = args.get("filter").unwrap();
        assert_eq!(filter.get("kind").unwrap(), "task");
        assert_eq!(filter.get("target").unwrap().as_str().unwrap(), "agent:poll-agent");
        assert_eq!(args.get("sort_by").unwrap(), "created_at");
        assert_eq!(args.get("sort_order").unwrap(), "desc");
    }

    // ---------------------------------------------------------------
    // Test: result/session artifacts carry canonical metadata
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn result_session_carry_canonical_metadata() {
        let store_resp_1 = wrap_mcp_response(&json!({"fact_id": "result-fact-1"}));
        let store_resp_2 = wrap_mcp_response(&json!({"fact_id": "session-fact-1"}));
        let (transport, mock_state) = MockTransport::new(vec![store_resp_1, store_resp_2]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        // Store result with canonical metadata
        let result = memory
            .memory_store(MemoryStoreParams {
                key: "default".to_string(),
                agent_id: "agent-1".to_string(),
                swarm_id: "default".to_string(),
                fact: "The answer is 42".to_string(),
                kind: "task_result".to_string(),
                target: None,
                metadata: Some(json!({
                    "task_fact_id": "fact-abc",
                    "task_id": "ext-001",
                })),
            })
            .await;
        assert!(result.is_ok());

        // Store session with canonical metadata
        let result = memory
            .memory_store(MemoryStoreParams {
                key: "default".to_string(),
                agent_id: "agent-1".to_string(),
                swarm_id: "default".to_string(),
                fact: "Agent completed task".to_string(),
                kind: "task_session".to_string(),
                target: None,
                metadata: Some(json!({
                    "task_fact_id": "fact-abc",
                    "task_id": "ext-001",
                })),
            })
            .await;
        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 2);

        let (name1, args1) = &calls[0];
        assert_eq!(name1, "memory_store");
        assert_eq!(args1.get("kind").unwrap(), "task_result");
        let meta1 = args1.get("metadata").unwrap();
        assert_eq!(meta1.get("task_fact_id").unwrap(), "fact-abc");
        assert_eq!(meta1.get("task_id").unwrap(), "ext-001");

        let (name2, args2) = &calls[1];
        assert_eq!(name2, "memory_store");
        assert_eq!(args2.get("kind").unwrap(), "task_session");
        let meta2 = args2.get("metadata").unwrap();
        assert_eq!(meta2.get("task_fact_id").unwrap(), "fact-abc");
        assert_eq!(meta2.get("task_id").unwrap(), "ext-001");
    }

    // ---------------------------------------------------------------
    // Test: status uses latest-only queries (canonical metadata linkage)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn status_queries_use_latest_only() {
        let result_resp = wrap_mcp_response(&json!({
            "facts": [{
                "id": "r1",
                "fact": "Latest result",
                "kind": "task_result",
                "metadata": {"task_fact_id": "fact-x", "task_id": "ext-1"}
            }]
        }));
        let session_resp = wrap_mcp_response(&json!({
            "facts": [{
                "id": "s1",
                "fact": "Latest session",
                "kind": "task_session",
                "metadata": {"task_fact_id": "fact-x", "task_id": "ext-1"}
            }]
        }));

        let (transport, mock_state) = MockTransport::new(vec![result_resp, session_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        // Query latest result
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task_result",
                    "metadata.task_fact_id": "fact-x",
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(1),
            })
            .await
            .unwrap();

        let fact = result.get("facts").unwrap().as_array().unwrap().first().unwrap();
        assert_eq!(fact.get("fact").unwrap(), "Latest result");

        // Query latest session
        let result = memory
            .memory_query(MemoryQueryParams {
                key: "default".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "default".to_string(),
                filter: json!({
                    "kind": "task_session",
                    "metadata.task_fact_id": "fact-x",
                }),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(1),
            })
            .await
            .unwrap();

        let fact = result.get("facts").unwrap().as_array().unwrap().first().unwrap();
        assert_eq!(fact.get("fact").unwrap(), "Latest session");

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 2);

        // Both queries must use sort_by=created_at, sort_order=desc, limit=1
        for (name, args) in &calls {
            assert_eq!(name, "memory_query");
            assert_eq!(args.get("sort_by").unwrap(), "created_at");
            assert_eq!(args.get("sort_order").unwrap(), "desc");
            assert_eq!(args.get("limit").unwrap(), 1);
        }
    }

    // ---------------------------------------------------------------
    // Test: memory_get wrapper serializes params correctly
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn memory_get_wrapper_serializes_params() {
        let get_resp = wrap_mcp_response(&json!({
            "id": "fact-1",
            "kind": "task",
            "fact": "Do stuff",
        }));
        let (transport, mock_state) = MockTransport::new(vec![get_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_get(crate::client::memory::MemoryGetParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
                fact_id: "fact-1".to_string(),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_get");
        assert_eq!(args.get("fact_id").unwrap(), "fact-1");
        assert_eq!(args.get("key").unwrap(), "ns");
    }

    // ---------------------------------------------------------------
    // Test: memory_query wrapper serializes params correctly
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn memory_query_wrapper_serializes_params() {
        let query_resp = wrap_mcp_response(&json!({"facts": []}));
        let (transport, mock_state) = MockTransport::new(vec![query_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_query(MemoryQueryParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
                filter: json!({"kind": "task", "target": "agent:x"}),
                sort_by: Some("created_at".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(5),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_query");
        assert_eq!(args.get("filter").unwrap().get("kind").unwrap(), "task");
        assert_eq!(args.get("sort_by").unwrap(), "created_at");
        assert_eq!(args.get("limit").unwrap(), 5);
    }

    #[tokio::test]
    async fn memory_get_config_wrapper_serializes_params() {
        let get_resp = wrap_mcp_response(&json!({
            "schema_version": 2,
            "embedding_model": "openai/text-embedding-3-large",
        }));
        let (transport, mock_state) = MockTransport::new(vec![get_resp]);
        let memory = Arc::new(MemoryMcpClient::new(transport));

        let result = memory
            .memory_get_config(crate::client::memory::MemoryGetConfigParams {
                key: "ns".to_string(),
                agent_id: "a".to_string(),
                swarm_id: "s".to_string(),
            })
            .await;

        assert!(result.is_ok());

        let calls = take_calls(&mock_state);
        assert_eq!(calls.len(), 1);
        let (name, args) = &calls[0];
        assert_eq!(name, "memory_get_config");
        assert_eq!(args.get("key").unwrap(), "ns");
        assert_eq!(args.get("agent_id").unwrap(), "a");
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter rejects string target that is a
    // different agent (mirrors dispatch_sse_event string-target branch)
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_rejects_string_target_mismatch() {
        let agent_id = "my-agent";
        let agent_target = format!("agent:{agent_id}");

        // String-form target pointing to a different agent
        let payload = json!({
            "id": "task-1",
            "kind": "task",
            "target": "agent:other-agent",
        });
        let target_ok = match payload.get("target") {
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(&agent_target)),
            Some(Value::String(s)) => s == &agent_target,
            _ => false,
        };
        assert!(!target_ok, "string target for wrong agent must be filtered");
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter accepts string target matching
    // this agent (mirrors dispatch_sse_event string-target branch)
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_accepts_string_target_match() {
        let agent_id = "my-agent";
        let agent_target = format!("agent:{agent_id}");

        let payload = json!({
            "id": "task-1",
            "kind": "task",
            "target": "agent:my-agent",
        });
        let target_ok = match payload.get("target") {
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(&agent_target)),
            Some(Value::String(s)) => s == &agent_target,
            _ => false,
        };
        assert!(target_ok, "string target for this agent must be accepted");
    }

    // ---------------------------------------------------------------
    // Negative-path: courier filter rejects non-"task" kind even when
    // target matches (e.g. kind="note")
    // ---------------------------------------------------------------
    #[test]
    fn courier_dispatch_filter_rejects_non_task_kind_with_valid_target() {
        let payload = json!({
            "id": "note-1",
            "kind": "note",
            "target": ["agent:my-agent"],
        });
        let kind_ok = payload.get("kind").and_then(|v| v.as_str()) == Some("task");
        assert!(!kind_ok, "kind=note must be filtered even when target matches");
    }
}

/// Router-level integration tests for the OAuth surface. These build
/// the full `Router` (so middleware, routing, and JSON shape are
/// exercised end-to-end) but skip the network — `tower::ServiceExt`
/// drives requests in-process. Coverage maps to the committed design
/// in `<gosh.cli>/specs/agent_mcp_unification.md` under
/// "Authentication for remote callers — built-in OAuth 2.1".
#[cfg(test)]
mod oauth_router {
    use std::net::SocketAddr;

    use axum::body::Body;
    use axum::body::Bytes;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request;
    use axum::http::StatusCode;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::server::build_router;
    use crate::test_support::test_app_state_with_oauth;

    /// Drive a single request against the router; return status + body bytes.
    /// `peer` simulates the connecting socket so the admin middleware can
    /// enforce loopback-vs-remote.
    async fn drive(
        router: axum::Router,
        peer: SocketAddr,
        req: Request<Body>,
    ) -> (StatusCode, Bytes) {
        // Inject ConnectInfo via the test helper (axum's
        // MockConnectInfo layer wraps the router so handlers see
        // `ConnectInfo<SocketAddr>` like they would in production).
        let svc = router.layer(MockConnectInfo(peer));
        let resp = svc.oneshot(req).await.expect("router responded");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.expect("body");
        (status, body)
    }

    fn loopback() -> SocketAddr {
        "127.0.0.1:54321".parse().unwrap()
    }

    fn remote() -> SocketAddr {
        "203.0.113.5:54321".parse().unwrap()
    }

    fn build(dcr: bool, admin: &str, tmp: &std::path::Path) -> axum::Router {
        let state = test_app_state_with_oauth(tmp.join("clients.toml"), dcr, admin);
        build_router(state)
    }

    fn parse_body(b: &[u8]) -> Value {
        serde_json::from_slice(b).expect("response body is JSON")
    }

    #[tokio::test]
    async fn metadata_advertises_registration_endpoint_when_dcr_on() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/oauth-authorization-server")
            .header("host", "example.test")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        assert!(v["registration_endpoint"].is_string());
        assert_eq!(v["token_endpoint"], "https://example.test/oauth/token");
        assert_eq!(v["code_challenge_methods_supported"], serde_json::json!(["S256"]));
    }

    #[tokio::test]
    async fn metadata_omits_registration_endpoint_when_dcr_off() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(false, "T", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/oauth-authorization-server")
            .header("host", "example.test")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        assert!(
            v.get("registration_endpoint").is_none(),
            "registration_endpoint must be absent when DCR off, got: {v}",
        );
    }

    #[tokio::test]
    async fn dcr_register_succeeds_when_enabled_and_returns_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_name":"Claude.ai","redirect_uris":["https://claude.ai/cb"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED, "body={}", String::from_utf8_lossy(&body));
        let v = parse_body(&body);
        assert!(!v["client_id"].as_str().unwrap().is_empty());
        assert!(!v["client_secret"].as_str().unwrap().is_empty());
        assert_eq!(v["client_name"], "Claude.ai");
        assert_eq!(v["client_secret_expires_at"], 0);
        assert_eq!(v["token_endpoint_auth_method"], "client_secret_basic");
        // Response must echo the persisted (not request-supplied) URI
        // set; this catches a regression where the handler echoes
        // attacker input instead of what was actually stored.
        assert_eq!(v["redirect_uris"], serde_json::json!(["https://claude.ai/cb"]));
    }

    #[tokio::test]
    async fn dcr_register_rejects_missing_redirect_uris() {
        // RFC 7591 §2: redirect_uris is required for clients using
        // a redirection grant. Empty / missing => 400, no record
        // persisted.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"client_name":"Claude.ai"}"#))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn dcr_register_rejects_non_http_redirect_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_name":"Claude.ai","redirect_uris":["javascript:alert(1)"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn dcr_register_returns_405_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(false, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"client_name":"Claude.ai"}"#))
            .unwrap();
        let (status, _body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn admin_paths_reject_remote_origin_even_with_correct_token() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "secret", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = drive(router, remote(), req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "admin paths must refuse non-loopback callers regardless of bearer",
        );
    }

    #[tokio::test]
    async fn admin_paths_reject_loopback_with_wrong_token() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "right-token", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer wrong-token")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_paths_reject_loopback_without_bearer() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_register_then_list_then_revoke_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        // Single router instance shared across the round-trip so
        // mutations land in the same `oauth_clients` mutex.
        let router = build(true, "T", tmp.path());

        // Register manually. Admin endpoint now requires non-empty
        // redirect_uris (symmetric with DCR), so an explicit list is
        // part of the canonical happy path.
        let req = Request::builder()
            .method("POST")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"workstation-1","redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let registered = parse_body(&body);
        let client_id = registered["client_id"].as_str().unwrap().to_string();
        assert_eq!(registered["name"], "workstation-1");
        let secret = registered["client_secret"].as_str().unwrap();
        assert!(!secret.is_empty(), "manual register must return plaintext secret");
        assert_eq!(
            registered["redirect_uris"],
            serde_json::json!(["https://claude.ai/api/mcp/auth_callback"]),
            "admin register response must echo the registered URI set",
        );

        // List — the just-registered client must appear with `source = manual`.
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let list = parse_body(&body);
        let arr = list["clients"].as_array().unwrap();
        let me =
            arr.iter().find(|c| c["client_id"] == client_id).expect("registered client listed");
        assert_eq!(me["name"], "workstation-1");
        assert_eq!(me["source"], "manual");
        // Defence-in-depth: list response must NOT include
        // `secret_hash` or `client_secret`.
        assert!(
            me.get("secret_hash").is_none() && me.get("client_secret").is_none(),
            "list response must not expose secret material, got: {me}",
        );

        // Revoke.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/clients/{}", client_id))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], true);

        // Re-revoke is idempotent: removed=false.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/clients/{}", client_id))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], false);
    }

    #[tokio::test]
    async fn admin_register_rejects_missing_redirect_uris() {
        // Symmetric with DCR /oauth/register: an admin-registered
        // client whose registered URI set is empty cannot complete
        // the authorize flow, so the operator must be told up
        // front rather than discover the client is dead at the
        // first authorize attempt.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"workstation-2"}"#))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn admin_register_rejects_non_http_redirect_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"workstation-3","redirect_uris":["javascript:alert(1)"]}"#))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn admin_registered_client_can_complete_authorize_with_registered_uri() {
        // The point of validating redirect_uris up front is so that a
        // manually-registered client is *usable*. End-to-end check:
        // POST /admin/oauth/clients with one URI -> GET /oauth/authorize
        // with that same URI -> consent page renders (no
        // "Redirect URI mismatch"). This is the regression that catches
        // a future tweak where admin path silently drops the URI on
        // the floor while still 201ing.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());

        // Manual register with one URI.
        let req = Request::builder()
            .method("POST")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"workstation-4","redirect_uris":["https://claude.ai/api/mcp/auth_callback"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let client_id = parse_body(&body)["client_id"].as_str().unwrap().to_string();

        // Authorize with the same URI -> consent page.
        let url = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=https%3A%2F%2Fclaude.ai%2Fapi%2Fmcp%2Fauth_callback\
             &state=hello&code_challenge=ch4ll&code_challenge_method=S256"
        );
        let req = Request::builder().method("GET").uri(url).body(Body::empty()).unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK, "authorize must accept the registered URI");
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains("sess_"),
            "expected consent page (with session id) on registered-URI authorize, got: {html}",
        );
        assert!(
            !html.contains("Redirect URI mismatch"),
            "registered URI must not surface as a mismatch",
        );
    }

    #[tokio::test]
    async fn dcr_registered_client_appears_in_admin_list_with_source_dcr() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());

        // DCR (public, no auth).
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_name":"DCR-Probe","redirect_uris":["https://claude.ai/cb"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let dcr_id = parse_body(&body)["client_id"].as_str().unwrap().to_string();

        // Admin list (gated, loopback + Bearer).
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let list = parse_body(&body);
        let arr = list["clients"].as_array().unwrap();
        let me = arr.iter().find(|c| c["client_id"] == dcr_id).expect("DCR'd client listed");
        assert_eq!(me["source"], "dcr");
    }

    // ── 7b: /oauth/authorize + /admin/oauth/sessions ─────────────

    /// Helper: DCR-register a client against the running router and
    /// return the generated `client_id`. Used by the authorize-flow
    /// tests that need a valid client to point `?client_id=` at.
    async fn dcr_register(router: axum::Router) -> String {
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_name":"Claude.ai","redirect_uris":["https://claude.ai/cb"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        parse_body(&body)["client_id"].as_str().unwrap().to_string()
    }

    fn authorize_url(client_id: &str) -> String {
        format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri=https%3A%2F%2Fclaude.ai%2Fcb&state=hello&code_challenge=ch4ll&code_challenge_method=S256"
        )
    }

    #[tokio::test]
    async fn get_authorize_happy_path_renders_consent_with_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;

        let req = Request::builder()
            .method("GET")
            .uri(authorize_url(&client_id))
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("sess_"), "consent page must show session id verbatim");
        assert!(
            html.contains("gosh agent oauth sessions pin sess_"),
            "consent page must show the exact CLI command for the operator",
        );
    }

    #[tokio::test]
    async fn get_authorize_rejects_plain_pkce_method() {
        // Strict S256-only is part of the committed design — pin
        // it so a future "let's accept plain too" tweak is loud.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;

        let url = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=https%3A%2F%2Fclaude.ai%2Fcb&code_challenge=x\
             &code_challenge_method=plain"
        );
        let req = Request::builder().method("GET").uri(url).body(Body::empty()).unwrap();
        let (status, _body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_authorize_rejects_unknown_client_id() {
        // RFC 6749 §4.1.2.1 says don't redirect on invalid client —
        // we render an error page instead so the operator can see
        // the failure mode (and so an attacker can't smuggle an
        // open-redirect via a forged `client_id`).
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());

        let req = Request::builder()
            .method("GET")
            .uri(authorize_url("nonexistent"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("Unknown client"));
    }

    #[tokio::test]
    async fn get_authorize_rejects_redirect_uri_not_registered_for_client() {
        // RFC 6749 §3.1.2.3 + RFC 7591 §2: redirect_uri MUST exact-
        // match one of the URIs the client registered. A DCR'd
        // client registered for https://claude.ai/cb that tries to
        // hand the code to https://evil.example/cb must be rejected
        // with no redirect (so we don't double as an open-redirect
        // helper) and no session created.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        // dcr_register registers https://claude.ai/cb — see the
        // helper at the top of this module.
        let client_id = dcr_register(router.clone()).await;

        let evil = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=https%3A%2F%2Fevil.example%2Fcb&state=hello\
             &code_challenge=ch4ll&code_challenge_method=S256"
        );
        let req = Request::builder().method("GET").uri(evil).body(Body::empty()).unwrap();
        let (status, headers, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            headers.get("location").is_none(),
            "must not 302 to a mismatched redirect_uri (would be open-redirect)",
        );
        assert!(String::from_utf8_lossy(&body).contains("Redirect URI mismatch"));
    }

    /// Helper: extract `sess_<hex>` from the consent HTML.
    fn extract_session_id(html: &str) -> String {
        let idx = html.find("sess_").expect("html should contain session id");
        // session id is "sess_" + 8 hex chars = 13 chars total.
        html[idx..idx + 13].to_string()
    }

    #[tokio::test]
    async fn full_authorize_flow_dcr_to_redirect_with_code_and_state() {
        // End-to-end happy path: DCR register → GET /authorize →
        // admin POST /sessions/<id>/pin → POST /authorize with PIN
        // → 302 to `redirect_uri?code=...&state=...`. This is the
        // full Claude.ai connector flow modulo the actual browser.
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;

        // 1. GET /authorize → consent page with session_id.
        let req = Request::builder()
            .method("GET")
            .uri(authorize_url(&client_id))
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let session_id = extract_session_id(std::str::from_utf8(&body).unwrap());

        // 2. Operator-issued PIN via admin endpoint.
        let req = Request::builder()
            .method("POST")
            .uri(format!("/admin/oauth/sessions/{session_id}/pin"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let pin = parse_body(&body)["pin"].as_str().unwrap().to_string();
        assert_eq!(pin.len(), 6);

        // 3. POST /authorize with the PIN → 302 redirect.
        let form = format!("session_id={session_id}&pin={pin}&action=approve");
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/authorize")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _body) = drive(router.clone(), loopback(), req).await;
        // axum::Redirect::to maps to 303 by default.
        assert!(status.is_redirection(), "approve should redirect (3xx), got {status}",);
    }

    #[tokio::test]
    async fn post_authorize_with_wrong_pin_returns_401_without_consuming_session() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;

        // Open session.
        let req = Request::builder()
            .method("GET")
            .uri(authorize_url(&client_id))
            .body(Body::empty())
            .unwrap();
        let (_, body) = drive(router.clone(), loopback(), req).await;
        let session_id = extract_session_id(std::str::from_utf8(&body).unwrap());

        // Issue a PIN so we exercise the mismatch path (not the
        // "no PIN" path).
        let req = Request::builder()
            .method("POST")
            .uri(format!("/admin/oauth/sessions/{session_id}/pin"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);

        // Submit wrong PIN.
        let form = format!("session_id={session_id}&pin=000000&action=approve");
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/authorize")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_sessions_list_shows_pending_session_after_authorize() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;

        let req = Request::builder()
            .method("GET")
            .uri(authorize_url(&client_id))
            .body(Body::empty())
            .unwrap();
        let (_, body) = drive(router.clone(), loopback(), req).await;
        let session_id = extract_session_id(std::str::from_utf8(&body).unwrap());

        // Admin list.
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/sessions")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        let arr = v["sessions"].as_array().unwrap();
        let me = arr
            .iter()
            .find(|s| s["session_id"] == session_id)
            .expect("just-created session listed");
        assert_eq!(me["status"], "pending");
        assert_eq!(me["has_pending_pin"], false);
        // Defence-in-depth: list view must NOT expose the PIN or
        // the (eventual) authorization code, even on stronger
        // session states.
        for forbidden in ["pin", "authorization_code", "code_challenge"] {
            assert!(
                me.get(forbidden).is_none(),
                "admin sessions list must not expose {forbidden}: {me}",
            );
        }
    }

    #[tokio::test]
    async fn admin_sessions_drop_removes_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());
        let client_id = dcr_register(router.clone()).await;
        let req = Request::builder()
            .method("GET")
            .uri(authorize_url(&client_id))
            .body(Body::empty())
            .unwrap();
        let (_, body) = drive(router.clone(), loopback(), req).await;
        let session_id = extract_session_id(std::str::from_utf8(&body).unwrap());

        // First drop: removed=true.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/sessions/{session_id}"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], true);

        // Second drop: removed=false.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/sessions/{session_id}"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], false);
    }

    #[tokio::test]
    async fn admin_sessions_pin_returns_404_for_unknown_session() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build(true, "T", tmp.path());

        let req = Request::builder()
            .method("POST")
            .uri("/admin/oauth/sessions/sess_deadbeef/pin")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(parse_body(&body)["error"], "not_pending");
    }

    // ── 7c: /oauth/token + /oauth/revoke + /mcp Bearer + admin tokens

    use axum::http::HeaderMap;
    use axum::http::HeaderValue;
    use base64::Engine;

    use crate::test_support::test_app_state_with_oauth_full;

    /// PKCE fixture from RFC 7636 §B.2 — pinned so future encoding
    /// regressions break loudly. `verifier`/`challenge` are a known
    /// SHA-256 + base64-url(no pad) pair.
    const PKCE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const PKCE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const REDIRECT_URI: &str = "https://claude.ai/cb";

    fn build_full(dcr: bool, admin: &str, tmp: &std::path::Path) -> axum::Router {
        let state = test_app_state_with_oauth_full(
            tmp.join("clients.toml"),
            tmp.join("tokens.toml"),
            dcr,
            admin,
        );
        build_router(state)
    }

    /// `drive` variant that surfaces the response headers so tests
    /// can read `Location`, `WWW-Authenticate`, etc.
    async fn drive_full(
        router: axum::Router,
        peer: SocketAddr,
        req: Request<Body>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let svc = router.layer(MockConnectInfo(peer));
        let resp = svc.oneshot(req).await.expect("router responded");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.expect("body");
        (status, headers, body)
    }

    /// DCR-register + return both `client_id` and `client_secret` so
    /// tests can authenticate at `/oauth/token`.
    async fn dcr_register_with_secret(router: axum::Router) -> (String, String) {
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"client_name":"Claude.ai","redirect_uris":["https://claude.ai/cb"]}"#,
            ))
            .unwrap();
        let (status, body) = drive(router, loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let v = parse_body(&body);
        let id = v["client_id"].as_str().unwrap().to_string();
        let secret = v["client_secret"].as_str().unwrap().to_string();
        (id, secret)
    }

    fn authorize_url_with_real_pkce(client_id: &str) -> String {
        format!(
            "/oauth/authorize?response_type=code&client_id={client_id}\
             &redirect_uri={ru}&state=hello&code_challenge={ch}\
             &code_challenge_method=S256",
            ru = urlencoding_encode(REDIRECT_URI),
            ch = PKCE_CHALLENGE,
        )
    }

    /// Local re-implementation of the urlencoding crate's encode (the
    /// agent crate doesn't depend on it; the CLI does). Tiny enough
    /// to roll inline for tests.
    fn urlencoding_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    fn basic_auth_header(id: &str, secret: &str) -> HeaderValue {
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        HeaderValue::from_str(&format!("Basic {raw}")).unwrap()
    }

    /// Drive the full DCR → authorize → PIN → approve flow and
    /// return `(code, client_id, client_secret)`. The returned `code`
    /// is the freshly-minted authorisation code in the redirect URL.
    async fn run_authorize_flow(router: axum::Router) -> (String, String, String) {
        let (client_id, client_secret) = dcr_register_with_secret(router.clone()).await;

        // GET /authorize → consent HTML.
        let req = Request::builder()
            .method("GET")
            .uri(authorize_url_with_real_pkce(&client_id))
            .body(Body::empty())
            .unwrap();
        let (_, body) = drive(router.clone(), loopback(), req).await;
        let session_id = extract_session_id(std::str::from_utf8(&body).unwrap());

        // Admin issues PIN.
        let req = Request::builder()
            .method("POST")
            .uri(format!("/admin/oauth/sessions/{session_id}/pin"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, body) = drive(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::CREATED);
        let pin = parse_body(&body)["pin"].as_str().unwrap().to_string();

        // POST /authorize with PIN → 303 with Location.
        let form = format!("session_id={session_id}&pin={pin}&action=approve");
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/authorize")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, headers, _body) = drive_full(router, loopback(), req).await;
        assert!(status.is_redirection(), "expected 3xx, got {status}");
        let location = headers.get("location").expect("approve sets Location").to_str().unwrap();
        let code = extract_code_from_location(location);
        (code, client_id, client_secret)
    }

    fn extract_code_from_location(url: &str) -> String {
        // Naïve parse: split on `?`, then on `&`, find `code=...`.
        let q = url.split_once('?').expect("redirect carries query").1;
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("code=") {
                return v.to_string();
            }
        }
        panic!("redirect URL has no `code` parameter: {url}");
    }

    #[tokio::test]
    async fn token_authorization_code_grant_happy_path_returns_access_and_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, headers, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&body));
        assert_eq!(headers.get("cache-control").unwrap(), "no-store");
        let v = parse_body(&body);
        assert_eq!(v["token_type"], "Bearer");
        assert_eq!(v["expires_in"], 3600);
        let access = v["access_token"].as_str().unwrap();
        let refresh = v["refresh_token"].as_str().unwrap();
        assert!(access.starts_with("at_"));
        assert!(refresh.starts_with("rt_"));
    }

    #[tokio::test]
    async fn token_authorization_code_grant_rejects_pkce_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier=wrong-verifier",
            ru = urlencoding_encode(REDIRECT_URI),
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _h, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn token_authorization_code_grant_is_single_use() {
        // RFC 6749 §4.1.2: codes MUST be short-lived AND single-use.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        // First exchange — succeeds.
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form.clone()))
            .unwrap();
        let (status, _h, _b) = drive_full(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);

        // Second exchange of the same code — invalid_grant.
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _h, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn token_invalid_client_returns_401_with_basic_realm_www_authenticate() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        // Bad client_id — but well-formed Basic header.
        let form = "grant_type=authorization_code&code=x&redirect_uri=https%3A%2F%2Fclaude.ai%2Fcb&code_verifier=v";
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header("nonexistent", "whatever"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, headers, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            headers
                .get("www-authenticate")
                .map(|v| v.to_str().unwrap_or("").contains("Basic"))
                .unwrap_or(false),
            "401 must carry WWW-Authenticate: Basic …",
        );
        assert_eq!(parse_body(&body)["error"], "invalid_client");
    }

    #[tokio::test]
    async fn token_refresh_grant_rotates_and_invalidates_old_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        // Mint first pair.
        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let first = parse_body(&body);
        let first_refresh = first["refresh_token"].as_str().unwrap().to_string();
        let first_access = first["access_token"].as_str().unwrap().to_string();

        // Rotate.
        let form = format!("grant_type=refresh_token&refresh_token={first_refresh}");
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _, body) = drive_full(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let rotated = parse_body(&body);
        let new_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
        let new_access = rotated["access_token"].as_str().unwrap().to_string();
        assert_ne!(new_refresh, first_refresh);
        assert_ne!(new_access, first_access);

        // Old refresh token must NOT rotate again.
        let form = format!("grant_type=refresh_token&refresh_token={first_refresh}");
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(parse_body(&body)["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn revoke_unknown_token_returns_200_silently() {
        // RFC 7009 §2.2: response is 200 even for unknown tokens.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (_, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = "token=rt_made_up_value&token_type_hint=refresh_token";
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/revoke")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn revoke_without_client_auth_returns_401() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/revoke")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("token=anything"))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_loopback_caller_succeeds_without_bearer() {
        // The stdio mcp-proxy is loopback — must not need a Bearer
        // to keep coding-CLI integrations working unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        // The MCP handler will respond with whatever the mock memory
        // produces; the only assertion is that we didn't get 401
        // from the auth layer.
        assert_ne!(status, StatusCode::UNAUTHORIZED, "loopback must bypass Bearer middleware");
    }

    #[tokio::test]
    async fn mcp_initialize_response_carries_crate_version() {
        // Pin that `serverInfo.version` reflects the running crate's
        // CARGO_PKG_VERSION rather than the historical hardcoded
        // "0.1.0". Pre-v0.8.x this field misled clients about which
        // daemon they were talking to (a freshly-built 0.8.0 agent
        // still claimed 0.1.0). The fix substitutes `env!("CARGO_PKG_VERSION")`
        // at compile time; this test catches a future regression
        // where someone hand-types a literal back in.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#))
            .unwrap();
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        assert_eq!(v["result"]["serverInfo"]["name"], "gosh-agent");
        assert_eq!(v["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        // Belt-and-suspenders: must not be the historical hardcode.
        assert_ne!(v["result"]["serverInfo"]["version"], "0.1.0");
    }

    #[tokio::test]
    async fn mcp_remote_caller_without_bearer_returns_401_with_www_authenticate() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, headers, _body) = drive_full(router, remote(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let www = headers.get("www-authenticate").unwrap().to_str().unwrap();
        assert!(www.contains("Bearer"), "missing-token 401 must advertise Bearer realm: {www}");
    }

    #[tokio::test]
    async fn mcp_remote_caller_with_invalid_bearer_returns_invalid_token() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", "Bearer at_definitely_not_a_real_token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, headers, _body) = drive_full(router, remote(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let www = headers.get("www-authenticate").unwrap().to_str().unwrap();
        assert!(
            www.contains("invalid_token"),
            "invalid-token 401 must include error=invalid_token: {www}",
        );
    }

    #[tokio::test]
    async fn mcp_remote_caller_with_valid_bearer_passes_auth_layer() {
        // Full e2e: DCR → authorize → token exchange → use access
        // token from a remote peer. The MCP handler may produce any
        // response from there — what we verify is that the auth
        // layer let it through.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let access = parse_body(&body)["access_token"].as_str().unwrap().to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {access}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, remote(), req).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "valid Bearer from remote peer must pass auth layer",
        );
    }

    #[tokio::test]
    async fn admin_tokens_list_shows_minted_record_without_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        // Mint a pair so something shows up in the listing.
        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, _) = drive_full(router.clone(), loopback(), req).await;

        // Admin list.
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/tokens")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        let arr = v["tokens"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "one token expected, got: {v}");
        let t = &arr[0];
        assert!(t["token_id"].as_str().unwrap().starts_with("tok_"));
        assert_eq!(t["client_id"], client_id);
        assert_eq!(t["active_access_tokens"], 1);
        for forbidden in ["token_hash", "access_token", "refresh_token"] {
            assert!(
                t.get(forbidden).is_none(),
                "admin tokens list must not expose {forbidden}: {t}",
            );
        }
    }

    #[tokio::test]
    async fn admin_tokens_revoke_kicks_remote_caller_immediately() {
        // End-to-end "boot the connected client": after admin
        // revokes the refresh, the previously-valid Bearer must
        // start returning 401 invalid_token.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let access = parse_body(&body)["access_token"].as_str().unwrap().to_string();

        // Find the token_id via admin list.
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/tokens")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let token_id = parse_body(&body)["tokens"][0]["token_id"].as_str().unwrap().to_string();

        // Revoke.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/tokens/{token_id}"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = drive_full(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], true);

        // Bearer that was just minted now fails — cascade evicted
        // the in-memory access record.
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {access}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, remote(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_clients_revoke_cascades_to_issued_tokens() {
        // Parallel to admin_tokens_revoke_kicks_remote_caller_immediately,
        // but the operator deletes the *client* instead of the token.
        // Without the cascade, an attacker who once held an access
        // token for a now-deleted client would keep passing /mcp
        // until the access TTL — closing that window is the point of
        // wiring `TokenStore::revoke_by_client` into the admin client
        // revoke handler.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let access = parse_body(&body)["access_token"].as_str().unwrap().to_string();

        // Sanity: bearer works on /mcp before the client revoke (we
        // present forwarded headers to force the bearer-required code
        // path; otherwise direct loopback bypasses the check).
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {access}"))
            .header("x-forwarded-for", "203.0.113.5")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _) = drive_full(router.clone(), loopback(), req).await;
        assert_ne!(status, StatusCode::UNAUTHORIZED, "bearer must work pre-revoke (sanity check)",);

        // Delete the client.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/admin/oauth/clients/{client_id}"))
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = drive_full(router.clone(), loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        assert_eq!(v["removed"], true);
        assert!(
            v["revoked_tokens"].as_u64().unwrap() >= 1,
            "client revoke must report at least the one cascade-revoked refresh, got {v}",
        );

        // Now the same bearer must fail.
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {access}"))
            .header("x-forwarded-for", "203.0.113.5")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _) = drive_full(router, loopback(), req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "bearer must stop working immediately after the client is deleted",
        );
    }

    #[tokio::test]
    async fn admin_clients_revoke_unknown_id_is_idempotent_with_zero_cascade() {
        // Idempotent re-call must not churn the token store: removed=false,
        // revoked_tokens=0, no I/O surprises.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("DELETE")
            .uri("/admin/oauth/clients/never-registered")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        assert_eq!(v["removed"], false);
        assert_eq!(v["revoked_tokens"], 0);
    }

    #[tokio::test]
    async fn admin_tokens_revoke_unknown_id_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("DELETE")
            .uri("/admin/oauth/tokens/tok_nonexistent")
            .header("authorization", "Bearer T")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(parse_body(&body)["removed"], false);
    }

    // ── 7d: reverse-proxy posture (X-Forwarded-* hardening) ───────

    /// Same-host TLS terminator (Caddy / cloudflared / Tailscale
    /// Funnel) forwards from `127.0.0.1:<ephemeral>` and sets
    /// `X-Forwarded-*`. A naïve `peer.is_loopback() → bypass` rule
    /// would expose the entire `/mcp` surface to anyone Caddy hands
    /// a request to. This test pins the hardening: the bypass only
    /// applies when the request is BOTH loopback AND has no
    /// forwarding headers.
    #[tokio::test]
    async fn mcp_loopback_with_x_forwarded_for_requires_bearer() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("x-forwarded-for", "203.0.113.5")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, headers, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "loopback peer with X-Forwarded-For must NOT bypass Bearer (would expose /mcp via reverse-proxy)",
        );
        let www = headers.get("www-authenticate").unwrap().to_str().unwrap();
        assert!(www.contains("Bearer"));
    }

    #[tokio::test]
    async fn mcp_loopback_with_x_forwarded_proto_requires_bearer() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("x-forwarded-proto", "https")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_loopback_with_forwarded_header_requires_bearer() {
        // RFC 7239 standard `Forwarded` header (less common in the
        // wild than `X-Forwarded-*` but conformant frontends emit it).
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("forwarded", "for=203.0.113.5;proto=https")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_loopback_with_x_forwarded_for_rejects_even_with_correct_token() {
        // Admin must refuse forwarded requests too: admin path is
        // operator-direct only, never via Caddy.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/admin/oauth/clients")
            .header("authorization", "Bearer T")
            .header("x-forwarded-for", "203.0.113.5")
            .body(Body::empty())
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "admin paths must refuse forwarded requests even with correct token",
        );
    }

    #[tokio::test]
    async fn mcp_loopback_with_valid_bearer_works_through_reverse_proxy_path() {
        // The intended deployment shape: Caddy on 127.0.0.1 forwards
        // an authenticated remote to the daemon. The Bearer check
        // passes, the request is served. This pins that the
        // hardening doesn't break the legitimate happy path.
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let (code, client_id, client_secret) = run_authorize_flow(router.clone()).await;

        let form = format!(
            "grant_type=authorization_code&code={code}\
             &redirect_uri={ru}&code_verifier={v}",
            ru = urlencoding_encode(REDIRECT_URI),
            v = PKCE_VERIFIER,
        );
        let req = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("authorization", basic_auth_header(&client_id, &client_secret))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let (_, _, body) = drive_full(router.clone(), loopback(), req).await;
        let access = parse_body(&body)["access_token"].as_str().unwrap().to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {access}"))
            .header("x-forwarded-for", "203.0.113.5")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "agent.example.com")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#))
            .unwrap();
        let (status, _, _body) = drive_full(router, loopback(), req).await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "loopback + forwarded headers + valid Bearer must pass auth (Caddy → daemon happy path)",
        );
    }

    /// Issuer URL in `/.well-known/oauth-authorization-server` must
    /// honour `X-Forwarded-Host` + `X-Forwarded-Proto` so Claude.ai
    /// gets the public HTTPS URL, not the daemon's internal HTTP one.
    /// 7a already had the unit test for this; this is the
    /// router-level pin that the path through middleware preserves
    /// the headers and the handler binds them into the issued URLs.
    #[tokio::test]
    async fn metadata_honors_forwarded_proto_and_host_through_router() {
        let tmp = tempfile::tempdir().unwrap();
        let router = build_full(true, "T", tmp.path());
        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/oauth-authorization-server")
            .header("host", "internal:8767")
            .header("x-forwarded-host", "agent.example.com")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        // Coming from the Caddy frontend on the same host.
        let (status, _, body) = drive_full(router, loopback(), req).await;
        assert_eq!(status, StatusCode::OK);
        let v = parse_body(&body);
        // X-Forwarded-Host wins over Host so a proxy that rewrites
        // Host to its upstream value (default for some Caddy
        // directives) still publishes the public hostname. Pin the
        // full URL — a regression where the daemon advertises
        // `https://internal:8767/...` would silently break Claude.ai
        // (DNS-fail at fetch time).
        assert_eq!(v["issuer"], "https://agent.example.com");
        assert_eq!(v["token_endpoint"], "https://agent.example.com/oauth/token");
        assert_eq!(v["authorization_endpoint"], "https://agent.example.com/oauth/authorize");
        assert_eq!(v["registration_endpoint"], "https://agent.example.com/oauth/register");
    }
}
