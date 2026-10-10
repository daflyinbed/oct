//! Real API mutations must retire an MCP handshake that still holds old config.
//! The initialize gate fixes the race ordering without sleeps or real models.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oct_agent::mcp::ServerStatus;
use serde_json::{Value, json};
use support::TestApp;
use tokio_util::sync::CancellationToken;

struct HandshakeGate {
    entered: CancellationToken,
    release: CancellationToken,
    initializes: AtomicUsize,
    tool_name: &'static str,
}

struct GatedMcp {
    url: String,
    gate: Arc<HandshakeGate>,
}

impl GatedMcp {
    async fn spawn(tool_name: &'static str) -> Self {
        let gate = Arc::new(HandshakeGate {
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
            initializes: AtomicUsize::new(0),
            tool_name,
        });
        let router = axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(mcp_post)
                    .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                    .delete(|| async { StatusCode::OK }),
            )
            .with_state(gate.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, gate }
    }
}

async fn mcp_post(State(gate): State<Arc<HandshakeGate>>, Json(request): Json<Value>) -> Response {
    let method = request["method"].as_str().unwrap_or_default();
    let Some(id) = request.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method {
        "initialize" => {
            gate.initializes.fetch_add(1, Ordering::SeqCst);
            gate.entered.cancel();
            gate.release.cancelled().await;
            json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {},
                "serverInfo": {"name": "gated-mcp", "version": "1"},
            })
        }
        "tools/list" => json!({"tools": [{
            "name": gate.tool_name,
            "inputSchema": {"type": "object", "properties": {}},
        }]}),
        "prompts/list" => json!({"prompts": []}),
        "resources/list" => json!({"resources": []}),
        _ => json!({}),
    };
    let mut response = Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response();
    if method == "initialize" {
        response
            .headers_mut()
            .insert("mcp-session-id", "gated-session".parse().unwrap());
    }
    response
}

enum Change {
    Delete,
    Disable,
    ReplaceUrl,
    MoveScope,
}

async fn ensure(app: &TestApp, project: &str) -> Value {
    let response = app
        .post_json(&format!("/api/projects/{project}/mcp/ensure"), json!(null))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn mutation_during_handshake(change: Change) {
    let app = TestApp::new().await;
    let original = GatedMcp::spawn("original_tool").await;
    let replacement = GatedMcp::spawn("replacement_tool").await;
    replacement.gate.release.cancel();
    let project = app.create_project().await;
    let other_project = app.create_project().await;
    let created = app
        .post_json(
            "/api/mcp/servers",
            json!({
                "name": "mutable-mcp",
                "transport": "http",
                "url": original.url,
                "project_id": project,
            }),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let row: Value = created.json().await.unwrap();
    let id = row["id"].as_str().unwrap();
    let ensure_url = format!("{}/api/projects/{project}/mcp/ensure", app.base_url);
    let in_flight = tokio::spawn(async move {
        reqwest::Client::new()
            .post(ensure_url)
            .json(&json!(null))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), original.gate.entered.cancelled())
        .await
        .expect("initialize reached its gate");

    let mutation = async {
        match change {
            Change::Delete => app.http_delete(&format!("/api/mcp/servers/{id}")).await,
            _ => {
                let patch = match change {
                    Change::Disable => json!({"enabled": false}),
                    Change::ReplaceUrl => json!({"url": replacement.url}),
                    Change::MoveScope => json!({"project_id": other_project}),
                    Change::Delete => unreachable!(),
                };
                reqwest::Client::new()
                    .patch(format!("{}/api/mcp/servers/{id}", app.base_url))
                    .json(&patch)
                    .send()
                    .await
                    .unwrap()
            }
        }
    };
    let response = tokio::time::timeout(Duration::from_secs(2), mutation)
        .await
        .expect("mutation must not wait for the handshake gate");
    assert_eq!(
        response.status(),
        if matches!(change, Change::Delete) {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::OK
        }
    );
    original.gate.release.cancel();
    let response = tokio::time::timeout(Duration::from_secs(10), in_flight)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _: Value = response.json().await.unwrap();
    assert!(
        app.mcp.status_of(id).is_none(),
        "late old handshake must retire"
    );
    assert!(app.mcp.tools_for_run(Some(&project)).is_empty());

    ensure(&app, &project).await;
    match change {
        Change::Delete | Change::Disable => assert!(app.mcp.status_of(id).is_none()),
        Change::ReplaceUrl => {
            assert!(matches!(
                app.mcp.status_of(id),
                Some(ServerStatus::Connected { .. })
            ));
            assert_eq!(replacement.gate.initializes.load(Ordering::SeqCst), 1);
            let tools = app.mcp.tools_of(id);
            assert_eq!(tools[0].name, "replacement_tool");
        }
        Change::MoveScope => {
            assert!(app.mcp.status_of(id).is_none());
            ensure(&app, &other_project).await;
            assert!(matches!(
                app.mcp.status_of(id),
                Some(ServerStatus::Connected { .. })
            ));
            assert!(app.mcp.tools_for_run(Some(&project)).is_empty());
            assert_eq!(app.mcp.tools_for_run(Some(&other_project)).len(), 1);
        }
    }
    assert_eq!(
        original.gate.initializes.load(Ordering::SeqCst),
        if matches!(change, Change::MoveScope) {
            2
        } else {
            1
        }
    );
}

#[tokio::test]
async fn deleting_server_during_handshake_prevents_late_entry() {
    mutation_during_handshake(Change::Delete).await;
}

#[tokio::test]
async fn disabling_server_during_handshake_prevents_late_entry() {
    mutation_during_handshake(Change::Disable).await;
}

#[tokio::test]
async fn changing_url_during_handshake_discards_old_catalog() {
    mutation_during_handshake(Change::ReplaceUrl).await;
}

#[tokio::test]
async fn moving_scope_during_handshake_retires_old_project_connection() {
    mutation_during_handshake(Change::MoveScope).await;
}
