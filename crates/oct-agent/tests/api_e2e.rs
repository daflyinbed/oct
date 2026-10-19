//! Full-stack API e2e tests: a real oct-agent server on a random port with
//! its own SQLite file, driven purely over HTTP. The LLM is a mock
//! OpenAI-compatible server (see `support::llm`) behind a provider row, so
//! the entire production chain — routing, provider resolution from the DB,
//! reqwest, SSE decoding, the agent loop, real tools, persistence, and the
//! SSE event stream back out — runs as written.

mod support;

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use oct_agent::db::messages as msg_db;
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall};
use oct_mcp::McpCallResult;
use support::TestApp;
use support::anthropic_llm;
use support::llm::{self, LlmTurn, finish_chunk, text_chunk, tool_call_chunk};
use support::mcp::{FakeMcpConnection, fake_tool};
use support::sse_payloads;

fn event_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["type"].as_str().expect("event type"))
        .collect()
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "text_delta")
        .map(|e| e["data"].as_str().expect("text_delta data"))
        .collect()
}

async fn stored_messages(app: &TestApp, conv_id: &str) -> Vec<Value> {
    let resp = app
        .get_json(&format!("/api/conversations/{conv_id}/messages"))
        .await;
    assert_eq!(resp.status(), 200);
    resp.json::<Vec<Value>>().await.expect("messages json")
}

/// Persist the state a backend crash leaves behind: an assistant message
/// with a ToolCall whose tool result was never written (and the user turn
/// that asked for it, recording the provider so resume can re-resolve it).
async fn seed_dangling_tool_call(app: &TestApp, conv_id: &str) {
    msg_db::insert_message(
        &app.pool,
        conv_id,
        &Message::text(Role::User, "run it"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .expect("seed user message");

    let assistant = Message {
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        })],
    };
    msg_db::insert_message(
        &app.pool,
        conv_id,
        &assistant,
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .expect("seed dangling assistant message");
}

// --- CRUD --------------------------------------------------------------------

#[tokio::test]
async fn project_lifecycle_roundtrip() {
    let app = TestApp::new().await;

    let resp = app
        .post_json(
            "/api/projects",
            serde_json::json!({"name": "p1", "working_dir": app.dir.path().to_string_lossy()}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let project: Value = resp.json().await.unwrap();
    let id = project["id"].as_str().unwrap().to_string();
    assert_eq!(project["name"], "p1");

    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    assert_eq!(resp.status(), 200);

    let resp = app.get_json("/api/projects").await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);

    // Update returns a bare 200; verify the effect through GET.
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"name": "renamed"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["name"], "renamed");

    let resp = app.http_delete(&format!("/api/projects/{id}")).await;
    assert_eq!(resp.status(), 204);

    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn deleting_project_cascades_conversations() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app
        .get_json(&format!("/api/projects/{project_id}/conversations"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);

    let resp = app
        .http_delete(&format!("/api/projects/{project_id}"))
        .await;
    assert_eq!(resp.status(), 204);

    // Cascade delete (FK ON DELETE CASCADE): the conversation is gone.
    let resp = app.get_json(&format!("/api/conversations/{conv_id}")).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn provider_create_conflicts_on_duplicate_and_hides_api_key() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![]).await;

    app.register_llm_provider(&llm_server).await;

    // Duplicate id → 409.
    let resp = app
        .post_json(
            "/api/providers",
            serde_json::json!({
                "id": "mock-llm",
                "name": "dup",
                "base_url": llm_server.url,
                "api_key": "k",
                "doc_url": null,
            }),
        )
        .await;
    assert_eq!(resp.status(), 409);

    // Listing shows whether a key is set, never the key itself.
    let resp = app.get_json("/api/providers").await;
    let providers: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "mock-llm");
    assert_eq!(providers[0]["api_key_set"], true);
    assert!(providers[0].get("api_key").is_none());
}

#[tokio::test]
async fn provider_api_key_reveal_endpoint_roundtrip() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![]).await;

    app.register_llm_provider(&llm_server).await;

    // Reveal returns the stored plaintext (settings page "show" action).
    let resp = app.get_json("/api/providers/mock-llm/api-key").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["api_key"], "test-key");

    // Unknown provider → 404.
    let resp = app.get_json("/api/providers/nope/api-key").await;
    assert_eq!(resp.status(), 404);

    // Key cleared (empty string) → nothing to reveal → 404.
    let resp = app
        .put_json(
            "/api/providers/mock-llm",
            serde_json::json!({"api_key": ""}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json("/api/providers/mock-llm/api-key").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn provider_delete_removes_custom_only_and_cascades_models() {
    let app = TestApp::new().await;

    // Custom provider (via POST) with one model: deletable, models cascade.
    let resp = app
        .post_json(
            "/api/providers",
            serde_json::json!({
                "id": "custom-one",
                "name": "Custom One",
                "base_url": "http://127.0.0.1:9",
                "api_key": "k",
                "doc_url": null,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let resp = app
        .post_json(
            "/api/providers/custom-one/models",
            serde_json::json!({"model_id": "m1", "name": "M1"}),
        )
        .await;
    assert_eq!(resp.status(), 201);

    // Seeded rows are catalog-owned (seed recreates them): deletion is refused.
    sqlx::query(
        "INSERT INTO providers (id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at) \
         VALUES ('seeded-one', 'Seeded One', 'openai_compatible', NULL, 'k', NULL, 'seeded', datetime('now'), datetime('now'))",
    )
    .execute(&app.pool)
    .await
    .expect("insert seeded provider row");
    let resp = app.http_delete("/api/providers/seeded-one").await;
    assert_eq!(resp.status(), 404);

    // Custom delete: 204, provider + its models gone (FK ON DELETE CASCADE).
    let resp = app.http_delete("/api/providers/custom-one").await;
    assert_eq!(resp.status(), 204);
    let resp = app.get_json("/api/providers").await;
    let providers: Vec<Value> = resp.json().await.unwrap();
    assert!(providers.iter().all(|p| p["id"] != "custom-one"));
    let models = sqlx::query("SELECT model_id FROM models WHERE provider_id = 'custom-one'")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert!(models.is_empty());

    // Unknown id → 404.
    let resp = app.http_delete("/api/providers/missing").await;
    assert_eq!(resp.status(), 404);
}

// --- MCP servers CRUD ----------------------------------------------------------

#[tokio::test]
async fn mcp_server_lifecycle_roundtrip() {
    let app = TestApp::new().await;

    // Create a stdio server: command + args + env, http fields left null.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "filesystem",
                "transport": "stdio",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": {"FOO": "bar"},
                "url": null,
                "headers": null,
                "tool_timeout_ms": 30_000,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty(), "id is generated server-side");
    assert_eq!(server["name"], "filesystem");
    assert_eq!(server["transport"], "stdio");
    assert_eq!(server["command"], "npx");
    assert_eq!(
        server["args"],
        serde_json::json!(["-y", "@modelcontextprotocol/server-filesystem", "/tmp"])
    );
    assert_eq!(server["env"], serde_json::json!({"FOO": "bar"}));
    assert_eq!(server["url"], Value::Null);
    assert_eq!(server["headers"], Value::Null);
    assert_eq!(server["enabled"], true, "enabled defaults to true");
    assert_eq!(server["tool_timeout_ms"], 30_000);

    // List contains exactly the new server.
    let resp = app.get_json("/api/mcp/servers").await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], id);

    // Get by id round-trips the stored JSON columns.
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    assert_eq!(resp.status(), 200);
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["args"], server["args"]);
    assert_eq!(fetched["env"], server["env"]);

    // Update returns a bare 200; verify the effect through GET.
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({
                "enabled": false,
                "tool_timeout_ms": 5_000,
                "args": ["-y", "other-server"],
                "env": {"A": "1"},
            }),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["enabled"], false);
    assert_eq!(fetched["tool_timeout_ms"], 5_000);
    assert_eq!(fetched["args"], serde_json::json!(["-y", "other-server"]));
    assert_eq!(fetched["env"], serde_json::json!({"A": "1"}));
    // Untouched fields survive the patch.
    assert_eq!(fetched["name"], "filesystem");
    assert_eq!(fetched["command"], "npx");

    let resp = app.http_delete(&format!("/api/mcp/servers/{id}")).await;
    assert_eq!(resp.status(), 204);

    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn mcp_server_duplicate_name_conflicts() {
    let app = TestApp::new().await;

    let body = serde_json::json!({
        "name": "dup",
        "transport": "stdio",
        "command": "npx",
    });
    let resp = app.post_json("/api/mcp/servers", &body).await;
    assert_eq!(resp.status(), 201);

    // Same name again → 409.
    let resp = app.post_json("/api/mcp/servers", &body).await;
    assert_eq!(resp.status(), 409);
    let error: Value = resp.json().await.unwrap();
    assert!(error["error"].as_str().unwrap().contains("already exists"));

    // Renaming a second server onto an existing name → 409.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "other", "transport": "stdio", "command": "npx"}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let other: Value = resp.json().await.unwrap();
    let other_id = other["id"].as_str().unwrap().to_string();

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{other_id}"),
            serde_json::json!({"name": "dup"}),
        )
        .await;
    assert_eq!(resp.status(), 409);

    // Re-asserting a server's own name is a no-op, not a conflict.
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{other_id}"),
            serde_json::json!({"name": "other"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn mcp_server_validation_rejects_bad_requests() {
    let app = TestApp::new().await;

    // http without a url → 400 (http is supported now; the url is required).
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "remote", "transport": "http"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("http transport requires a url")
    );

    // http with a non-http(s) scheme → 400 (cheap structural guard).
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "remote", "transport": "http", "url": "ftp://example.com/mcp"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("http:// or https://")
    );

    // Unknown transport → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "grpc", "command": "x"}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // stdio without a command → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("requires a command")
    );

    // Blank command → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio", "command": "   "}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Blank name → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "  ", "transport": "stdio", "command": "npx"}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Non-positive tool timeouts → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio", "command": "npx", "tool_timeout_ms": 0}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio", "command": "npx", "tool_timeout_ms": -1}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // args/env carry their JSON-shape contract in the request types: a
    // non-array args or non-object env fails deserialization → 422.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio", "command": "npx", "args": "flat"}),
        )
        .await;
    assert_eq!(resp.status(), 422);
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "s", "transport": "stdio", "command": "npx", "env": ["A"]}),
        )
        .await;
    assert_eq!(resp.status(), 422);

    // Nothing from the rejected creates was stored.
    let resp = app.get_json("/api/mcp/servers").await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 0);

    // The same rules apply on update.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "valid", "transport": "stdio", "command": "npx"}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();

    // Switching to http WITHOUT providing (or already having) a url → 400:
    // the cross-check runs on the effective post-merge values.
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({"transport": "http"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("http transport requires a url")
    );

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({"command": ""}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({"tool_timeout_ms": 0}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Unknown id → 404 on get, update and delete alike.
    let missing = "00000000-0000-0000-0000-000000000000";
    let resp = app.get_json(&format!("/api/mcp/servers/{missing}")).await;
    assert_eq!(resp.status(), 404);
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{missing}"),
            serde_json::json!({"name": "x"}),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let resp = app
        .http_delete(&format!("/api/mcp/servers/{missing}"))
        .await;
    assert_eq!(resp.status(), 404);
}

// --- MCP http transport: full chain against a local Streamable HTTP mock ------

#[tokio::test]
async fn mcp_http_server_connects_and_surfaces_tools_with_bearer_headers() {
    let app = TestApp::new().await;
    let mock = support::mcp_http::McpHttpMock::spawn().await;
    let project_id = app.create_project().await;

    // Create an http row: url + bearer headers; stdio fields stay null.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "remote-http",
                "transport": "http",
                "url": mock.url,
                "headers": {"Authorization": support::mcp_http::TOKEN},
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();
    assert_eq!(server["transport"], "http");
    assert_eq!(server["url"], mock.url);
    assert_eq!(server["status"], "not_connected");

    // The project's ensure endpoint reconciles: the real backend connects
    // through oct-mcp's Streamable HTTP client, lists tools, and surfaces
    // both the live status and the tool catalog.
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let effective: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0]["id"], id);
    assert_eq!(effective[0]["status"], "connected");
    assert_eq!(effective[0]["tool_count"], 1);
    assert_eq!(
        effective[0]["tools"],
        serde_json::json!([{"name": "remote_tool", "description": "served over http"}]),
    );

    // The mock saw the full handshake chain, every request authorized with
    // the row's bearer header (initialize included).
    let requests = mock.requests();
    let methods: Vec<&str> = requests.iter().map(|r| r.method.as_str()).collect();
    assert!(methods.contains(&"initialize"), "saw {methods:?}");
    assert!(methods.contains(&"tools/list"));
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some(support::mcp_http::TOKEN),
            "request `{}` missed the bearer header",
            request.method
        );
    }

    // GET by id mirrors the connected state.
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["status"], "connected");
}

/// prompts→斜杠命令 与 resources 读侧的全链路:ensure 并入目录、渲染端点往返、
/// 资源读取(文本与 blob)、404 三态(行缺失/未连接/未知名字)。
#[tokio::test]
async fn mcp_http_prompts_and_resources_roundtrip() {
    let app = TestApp::new().await;
    let mock = support::mcp_http::McpHttpMock::spawn().await;
    let project_id = app.create_project().await;

    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "remote-http",
                "transport": "http",
                "url": mock.url,
                "headers": {"Authorization": support::mcp_http::TOKEN},
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();

    // ensure 建连后,prompts/resources 目录随 tools 一并入响应。
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!(null),
        )
        .await;
    let effective: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(
        effective[0]["prompts"],
        serde_json::json!([
            {"name": "status", "description": "report status", "arguments": []},
            {
                "name": "deploy",
                "description": "deploy checklist",
                "arguments": [{"name": "env", "description": null, "required": true}],
            },
        ]),
        "prompt catalog with argument metadata"
    );
    assert_eq!(
        effective[0]["resources"],
        serde_json::json!([
            {
                "uri": "memo://notes",
                "name": "notes",
                "description": "meeting notes",
                "mime_type": "text/markdown",
            },
            {
                "uri": "file://logo.png",
                "name": "logo",
                "description": null,
                "mime_type": "image/png",
            },
        ]),
    );

    // GET 详情同样并入目录。
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["prompts"].as_array().unwrap().len(), 2);
    assert_eq!(fetched["resources"].as_array().unwrap().len(), 2);

    // 无参 prompt:空对象 body(arguments 缺省),返回按序 messages。
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/prompts/status"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let rendered: Value = resp.json().await.unwrap();
    assert_eq!(
        rendered["messages"],
        serde_json::json!([{"role": "user", "text": "Report status"}]),
    );

    // 带参 prompt:arguments 原样抵达 server(mirror 成渲染文本)。
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/prompts/deploy"),
            serde_json::json!({"arguments": {"env": "prod"}}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let rendered: Value = resp.json().await.unwrap();
    assert_eq!(
        rendered["messages"],
        serde_json::json!([
            {"role": "user", "text": "Deploy to prod"},
            {"role": "assistant", "text": "Checklist ready"},
        ]),
    );
    // 渲染请求也带上了 bearer(同一个 mock 记录流)。
    assert!(
        mock.requests().iter().any(|r| r.method == "prompts/get"),
        "prompts/get must have hit the mock"
    );

    // 未知 prompt(目录非空,成员校验 404,不打 server)。
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/prompts/nope"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // resources/read:文本与 blob 各一。
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/resources/read"),
            serde_json::json!({"uri": "memo://notes"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let text: Value = resp.json().await.unwrap();
    assert_eq!(text["mime_type"], "text/markdown");
    assert_eq!(text["text"], "# Notes");
    assert_eq!(text["blob_base64"], Value::Null);

    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/resources/read"),
            serde_json::json!({"uri": "file://logo.png"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let blob: Value = resp.json().await.unwrap();
    assert_eq!(blob["blob_base64"], "aVBuZw==");
    assert_eq!(blob["text"], Value::Null);

    // 未知 URI(目录非空)→ 404。
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/resources/read"),
            serde_json::json!({"uri": "nope://x"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // 行不存在 → 404。
    let resp = app
        .post_json(
            "/api/mcp/servers/missing/prompts/status",
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // 存在但从未建连的行(manager 无 entry)→ 404,且不触发 spawn。
    let never_connected = create_mcp_row(&app, "never-connected").await;
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{never_connected}/prompts/status"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("not connected"),
        "error should say not connected: {body}"
    );
}

#[tokio::test]
async fn mcp_stdio_http_transport_switch_preserves_the_other_sides_fields() {
    let app = TestApp::new().await;
    let mock = support::mcp_http::McpHttpMock::spawn().await;

    // Start stdio, switch to http in one PATCH (transport + url together).
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "switcher",
                "transport": "stdio",
                "command": "oct-mcp-test-nonexistent-binary",
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({
                "transport": "http",
                "url": mock.url,
                "headers": {"Authorization": support::mcp_http::TOKEN},
            }),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["transport"], "http");
    assert_eq!(fetched["url"], mock.url);
    // The stale stdio command survives the switch (deliberate: switching
    // back and forth never destroys the other side's config).
    assert_eq!(fetched["command"], "oct-mcp-test-nonexistent-binary");

    // Switch back to stdio: the old command is still there, so the PATCH
    // goes through without re-sending it — and the stale url/headers stay.
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({"transport": "stdio"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["transport"], "stdio");
    assert_eq!(fetched["url"], mock.url, "http fields are kept, not wiped");

    // Switching to stdio on a row with NO command anywhere → 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "http-only",
                "transport": "http",
                "url": mock.url,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let http_only: Value = resp.json().await.unwrap();
    let http_only_id = http_only["id"].as_str().unwrap().to_string();
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{http_only_id}"),
            serde_json::json!({"transport": "stdio"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("stdio transport requires a command")
    );
}

#[tokio::test]
async fn mcp_http_unreachable_url_surfaces_a_failed_status() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;

    // Port 1 on loopback: nothing listens, the connect fails immediately
    // (ECONNREFUSED — no 15s handshake budget is spent waiting).
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "dead-remote",
                "transport": "http",
                "url": "http://127.0.0.1:1/mcp",
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);

    // ensure reconciles: a broken server reports `failed` with the error
    // string, never a 5xx.
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let effective: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0]["status"], "failed");
    let error = effective[0]["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("127.0.0.1:1"),
        "error should name the endpoint: {error}"
    );
}

// --- MCP servers in the chat loop (injected fake connection) -------------------

/// POST /api/mcp/servers with a stdio config; returns the created row's id.
async fn create_mcp_row(app: &TestApp, name: &str) -> String {
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": name,
                "transport": "stdio",
                "command": "oct-mcp-test-nonexistent-binary",
            }),
        )
        .await;
    assert_eq!(resp.status(), 201, "mcp row creation should succeed");
    resp.json::<Value>().await.expect("mcp server json")["id"]
        .as_str()
        .expect("mcp server id")
        .to_string()
}

/// POST /api/mcp/servers scoped to `project_id`; returns the created row's id.
async fn create_scoped_mcp_row(app: &TestApp, name: &str, project_id: &str) -> String {
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": name,
                "transport": "stdio",
                "command": "oct-mcp-test-nonexistent-binary",
                "project_id": project_id,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201, "scoped mcp row creation should succeed");
    resp.json::<Value>().await.expect("mcp server json")["id"]
        .as_str()
        .expect("mcp server id")
        .to_string()
}

/// POST /api/projects/{id}/mcp/ensure reconciles before listing: rows the
/// project can see get a connection attempt (Failed for the nonexistent test
/// binary), while disabled and out-of-scope rows stay out of the response.
#[tokio::test]
async fn mcp_ensure_lists_project_effective_set_after_reconcile() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;
    let other_project = app.create_project().await;

    let global_id = create_mcp_row(&app, "global").await;
    let scoped_id = create_scoped_mcp_row(&app, "scoped", &project_id).await;
    let foreign_id = create_scoped_mcp_row(&app, "foreign", &other_project).await;
    let disabled_id = create_mcp_row(&app, "off").await;
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{disabled_id}"),
            serde_json::json!({ "enabled": false }),
        )
        .await;
    assert_eq!(resp.status(), 200);

    let resp = app
        .post_json(
            "/api/projects/oct-mcp-missing-project/mcp/ensure",
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let mut ids: Vec<&str> = list
        .iter()
        .map(|server| server["id"].as_str().expect("server id"))
        .collect();
    ids.sort_unstable();
    let mut expected = vec![global_id.as_str(), scoped_id.as_str()];
    expected.sort_unstable();
    assert_eq!(
        ids, expected,
        "effective set = global + own project, enabled only"
    );

    // The reconcile actually ran: every listed row was attempted and its
    // failure recorded (the test command does not exist), not left as the
    // pre-connect `not_connected` the plain list endpoint reports.
    for server in &list {
        assert_eq!(server["status"], "failed");
        let error = server["error"].as_str().expect("failed carries an error");
        assert!(
            error.contains("oct-mcp-test-nonexistent-binary"),
            "error names the command: {error}"
        );
        assert_eq!(server["tool_count"], Value::Null);
    }

    // A repeat ensure stays 200 with the same shape (Failed entries retry by
    // contract), and another project's ensure lists only its own view.
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let relist: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(relist.len(), list.len());

    let resp = app
        .post_json(
            &format!("/api/projects/{other_project}/mcp/ensure"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let foreign: Vec<Value> = resp.json().await.unwrap();
    let mut foreign_ids: Vec<&str> = foreign
        .iter()
        .map(|server| server["id"].as_str().expect("server id"))
        .collect();
    foreign_ids.sort_unstable();
    let mut foreign_expected = vec![global_id.as_str(), foreign_id.as_str()];
    foreign_expected.sort_unstable();
    assert_eq!(
        foreign_ids, foreign_expected,
        "the global row serves the other project too"
    );
    assert!(
        foreign.iter().all(|server| server["status"] == "failed"),
        "every listed row was reconciled for this project as well"
    );
}

#[tokio::test]
async fn mcp_tools_join_the_chat_loop_and_status_reports_live_state() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk("call-1", "mcp__fake__echo", r#"{"a":1}"#),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;

    // Two configured rows: one will get a scripted connection injected, the
    // other stays unconnected (its command does not exist — the reconcile
    // attempt fails fast and is recorded as Failed, which is exactly the
    // no-real-binary contract of these tests).
    let fake_id = create_mcp_row(&app, "fake").await;
    let other_id = create_mcp_row(&app, "other").await;

    let listing = vec![fake_tool("echo", Some("say things"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "echo",
        McpCallResult {
            text: "hello from mcp".to_string(),
            structured_json: Some(serde_json::json!({"sum": 3})),
            is_error: false,
            images: Vec::new(),
        },
    ));
    app.mcp
        .insert_connection_for_test(&fake_id, "fake", None, fake.clone(), listing);

    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    // MCP tools ask under the default permission mode; this
    // test pins the tool-join mechanics, not the approval flow.
    app.set_permission_mode(&project_id, &conv_id, "bypass")
        .await;

    // The chat turn reconciles (spawning the missing binary fails fast),
    // keeps the injected connection, and advertises the MCP tool to the LLM.
    let resp = app.send_message(&conv_id, "use the mcp tool").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;

    let tool_result = events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .expect("tool_result in stream");
    assert_eq!(tool_result["data"]["is_error"], false);
    assert_eq!(tool_result["data"]["content"], "hello from mcp");
    assert_eq!(tool_result["data"]["details"]["server"], "fake");
    assert_eq!(tool_result["data"]["details"]["tool"], "echo");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // The fake saw the ORIGINAL tool name with verbatim arguments — the
    // exposed-name translation happened inside the adapter.
    assert_eq!(
        fake.calls(),
        vec![("echo".to_string(), serde_json::json!({"a": 1}))]
    );

    // The first LLM request advertised the MCP tool under its exposed name.
    let first = &llm_server.requests()[0];
    let advertised = first["tools"].as_array().expect("tools advertised");
    let echo = advertised
        .iter()
        .find(|t| t["function"]["name"] == "mcp__fake__echo")
        .expect("the mcp tool must be advertised to the LLM");
    assert_eq!(echo["function"]["description"], "[mcp:fake] say things");

    // The second LLM request carries the MCP tool result back.
    let second = &llm_server.requests()[1];
    let tool_message = second["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(tool_message["role"], "tool");
    assert_eq!(tool_message["tool_call_id"], "call-1");
    assert_eq!(tool_message["content"], "hello from mcp");

    // Persistence: the tool message's typed parts carry the ToolResult.
    let stored = stored_messages(&app, &conv_id).await;
    let roles: Vec<&str> = stored.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
    assert_eq!(stored[2]["parts"][0]["type"], "tool_result");
    assert_eq!(stored[2]["parts"][0]["content"], "hello from mcp");

    // Live status in the list endpoint: injected row connected, the
    // never-connected row reports its failed reconcile attempt.
    let resp = app.get_json("/api/mcp/servers").await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let by_id = |id: &str| {
        list.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("row {id} in list"))
    };
    let fake_row = by_id(&fake_id);
    assert_eq!(fake_row["status"], "connected");
    assert_eq!(fake_row["tool_count"], 1);
    assert_eq!(fake_row["error"], Value::Null);
    let other_row = by_id(&other_id);
    assert_eq!(other_row["status"], "failed");
    assert_eq!(other_row["tool_count"], Value::Null);
    assert!(
        other_row["error"]
            .as_str()
            .unwrap()
            .contains("oct-mcp-test-nonexistent-binary"),
        "error should name the command: {}",
        other_row["error"]
    );

    // GET by id carries the same merged status.
    let resp = app.get_json(&format!("/api/mcp/servers/{fake_id}")).await;
    assert_eq!(resp.status(), 200);
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["status"], "connected");
    assert_eq!(fetched["tool_count"], 1);
    // The cached tools/list is surfaced for the tools panel: raw server-side
    // name and description, no [mcp:...] prefix, no truncation.
    let tools = fetched["tools"].as_array().expect("tools while connected");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(tools[0]["description"], "say things");

    // Failed rows carry no tools payload.
    let resp = app.get_json(&format!("/api/mcp/servers/{other_id}")).await;
    assert_eq!(resp.status(), 200);
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["status"], "failed");
    assert_eq!(fetched["tools"], Value::Null);
}

#[tokio::test]
async fn mcp_list_changed_notification_refreshes_the_surfaced_catalog() {
    let app = TestApp::new().await;

    // A connected server whose live catalog moves while the connection stays
    // up: the fake announces `tools/list_changed` like a real server, and the
    // manager's debounced refresh must make the new catalog visible through
    // the API without any reconnect.
    let id = create_mcp_row(&app, "live").await;
    let listing = vec![fake_tool("echo", Some("say things"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()));
    app.mcp
        .insert_connection_for_test(&id, "live", None, fake.clone(), listing);

    // Baseline: the injected catalog is surfaced as-is (and the injected
    // listing did not go through the connection — list_call_count starts 0).
    let fetched = mcp_server_detail(&app, &id).await;
    assert_eq!(fetched["status"], "connected");
    assert_eq!(fetched["tool_count"], 1);
    assert_eq!(fake.list_call_count(), 0);

    // The server ships a second tool and announces the change twice in a
    // burst — the debounce must collapse the burst into ONE re-list.
    fake.set_tools(vec![
        fake_tool("echo", Some("say things")),
        fake_tool("spawn", None),
    ]);
    fake.notify_tool_list_changed();
    fake.notify_tool_list_changed();

    // The refresh task is a detached background job with no test-visible
    // channel, so its only observable effect is the API response — bounded
    // poll, same justification as wait_conversation (never a bare sleep).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let fetched = loop {
        let fetched = mcp_server_detail(&app, &id).await;
        if fetched["tool_count"] == 2 {
            break fetched;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the refreshed catalog, last: {fetched}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // The refreshed catalog reaches the tools panel payload under the raw
    // server-side names, and the burst really cost a single re-list.
    let tools = fetched["tools"].as_array().expect("tools while connected");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(tools[1]["name"], "spawn");
    assert_eq!(
        fake.list_call_count(),
        1,
        "a burst of notifications is one debounced re-list"
    );
}

/// GET /api/mcp/servers/{id} (the detail endpoint the tools panel reads).
async fn mcp_server_detail(app: &TestApp, id: &str) -> Value {
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    assert_eq!(resp.status(), 200);
    resp.json().await.expect("server json")
}

#[tokio::test]
async fn mcp_rows_never_reconciled_report_not_connected() {
    let app = TestApp::new().await;

    // A row exists but no chat turn ever ran: the manager has no entry for
    // it, and the merged response reports `not_connected` (with the storage
    // fields still intact).
    let id = create_mcp_row(&app, "idle").await;

    let resp = app.get_json("/api/mcp/servers").await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list[0]["status"], "not_connected");
    assert_eq!(list[0]["tool_count"], Value::Null);
    assert_eq!(list[0]["error"], Value::Null);

    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["status"], "not_connected");
    assert_eq!(fetched["name"], "idle");
    assert_eq!(fetched["command"], "oct-mcp-test-nonexistent-binary");
}

// --- MCP servers project scoping ------------------------------------------------

#[tokio::test]
async fn mcp_server_project_scope_roundtrip_and_effective_filter() {
    let app = TestApp::new().await;
    let project_a = app.create_project().await;
    let project_b = app.create_project().await;

    // Create scoped to A: the stored row (and the response) carry the id.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "a-only",
                "transport": "stdio",
                "command": "npx",
                "project_id": project_a,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let scoped: Value = resp.json().await.unwrap();
    assert_eq!(scoped["project_id"], serde_json::json!(project_a));

    // A global server: null project_id in the response.
    let shared_id = create_mcp_row(&app, "shared").await;
    let resp = app.get_json(&format!("/api/mcp/servers/{shared_id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["project_id"], Value::Null);

    // An unknown project id on create → 404, nothing stored.
    let missing = "00000000-0000-0000-0000-000000000000";
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "bad",
                "transport": "stdio",
                "command": "npx",
                "project_id": missing,
            }),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("Project not found")
    );

    // Effective-set filter: global + own project, each DTO still marking
    // its own project_id.
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_a}"))
        .await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let names: Vec<&str> = list.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a-only", "shared"], "creation order");
    assert_eq!(list[0]["project_id"], serde_json::json!(project_a));
    assert_eq!(list[1]["project_id"], Value::Null);

    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_b}"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    let names: Vec<&str> = list.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["shared"]);

    // Unknown project on the filter → 404.
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={missing}"))
        .await;
    assert_eq!(resp.status(), 404);

    // Disabled rows drop out of the effective set (but stay in the
    // unfiltered list).
    let scoped_id = scoped["id"].as_str().unwrap().to_string();
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{scoped_id}"),
            serde_json::json!({"enabled": false}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_a}"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    let names: Vec<&str> = list.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["shared"]);
    let resp = app.get_json("/api/mcp/servers").await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(
        list.len(),
        2,
        "the unfiltered list still shows the disabled row"
    );

    // Re-scoping via PATCH: absent leaves the scope unchanged, an unknown
    // project is rejected without changing it.
    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{shared_id}"),
            serde_json::json!({"project_id": project_b}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/mcp/servers/{shared_id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["project_id"], serde_json::json!(project_b));

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{shared_id}"),
            serde_json::json!({"project_id": missing}),
        )
        .await;
    assert_eq!(resp.status(), 404);
    let resp = app.get_json(&format!("/api/mcp/servers/{shared_id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["project_id"], serde_json::json!(project_b));

    // After the re-scope, A's effective set is empty and B sees `shared`.
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_a}"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 0);
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_b}"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    let names: Vec<&str> = list.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["shared"]);
}

#[tokio::test]
async fn deleting_project_cascades_scoped_mcp_servers() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;
    let scoped_id = create_scoped_mcp_row(&app, "scoped", &project_id).await;
    let global_id = create_mcp_row(&app, "global").await;

    let resp = app
        .http_delete(&format!("/api/projects/{project_id}"))
        .await;
    assert_eq!(resp.status(), 204);

    // Cascade delete (FK ON DELETE CASCADE): the scoped row is gone, the
    // global row survives.
    let resp = app.get_json(&format!("/api/mcp/servers/{scoped_id}")).await;
    assert_eq!(resp.status(), 404);
    let resp = app.get_json("/api/mcp/servers").await;
    let list: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = list.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![global_id.as_str()]);
}

#[tokio::test]
async fn mcp_project_scoped_tools_are_isolated_across_projects() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        // A's run: call the scoped MCP tool, then finish.
        LlmTurn::Complete(vec![
            tool_call_chunk("call-a", "mcp__scoped__echo", r#"{"a":1}"#),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("a done"), finish_chunk("stop")]),
        // B's run: plain text.
        LlmTurn::Complete(vec![text_chunk("b done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;

    let project_a = app.create_project().await;
    let project_b = app.create_project().await;
    let conv_a = app.create_conversation(&project_a).await;
    let conv_b = app.create_conversation(&project_b).await;
    // A's turn invokes an MCP tool — an Ask under the default permission
    // mode; scoping is under test here, not approvals.
    app.set_permission_mode(&project_a, &conv_a, "bypass").await;

    // One server row scoped to A, with a scripted connection injected under
    // the SAME scope: A's runs see the tool, B's never do.
    let scoped_id = create_scoped_mcp_row(&app, "scoped", &project_a).await;
    let listing = vec![fake_tool("echo", Some("say things"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "echo",
        McpCallResult {
            text: "hello from scoped".to_string(),
            structured_json: None,
            is_error: false,
            images: Vec::new(),
        },
    ));
    app.mcp.insert_connection_for_test(
        &scoped_id,
        "scoped",
        Some(&project_a),
        fake.clone(),
        listing,
    );

    // A's first turn: the tool is advertised on the LLM wire and the call
    // round-trips through the whole stack.
    let resp = app.send_message(&conv_a, "use the mcp tool").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    let tool_result = events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .expect("tool_result in A's stream");
    assert_eq!(tool_result["data"]["is_error"], false);
    assert_eq!(tool_result["data"]["content"], "hello from scoped");
    assert_eq!(
        fake.calls(),
        vec![("echo".to_string(), serde_json::json!({"a": 1}))]
    );

    let requests = llm_server.requests();
    assert_eq!(requests.len(), 2, "A's run: tool turn + final turn");
    let a_tools = requests[0]["tools"].as_array().expect("tools advertised");
    assert!(
        a_tools
            .iter()
            .any(|t| t["function"]["name"] == "mcp__scoped__echo"),
        "A must be advertised the scoped tool"
    );

    // B's first turn: no mcp__ tool is advertised (workspace tools only)
    // and the fake never sees a call from B.
    let resp = app.send_message(&conv_b, "hi from b").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(text_of(&events), "b done");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let requests = llm_server.requests();
    assert_eq!(requests.len(), 3);
    let b_tools = requests[2]["tools"].as_array().expect("tools advertised");
    assert!(
        b_tools.iter().all(|t| !t["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .starts_with("mcp__")),
        "B must not see A's scoped tool: {b_tools:?}"
    );
    assert_eq!(
        fake.calls().len(),
        1,
        "B must never dispatch through A's server"
    );

    // The list endpoint still reports the row (scoped to A) regardless of
    // live connections.
    let resp = app
        .get_json(&format!("/api/mcp/servers?project_id={project_b}"))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 0, "the scoped row is not in B's effective set");
}

// --- Chat: full stack through the mock LLM ------------------------------------

#[tokio::test]
async fn chat_streams_text_and_persists_over_http() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("你好"),
        text_chunk("，世界"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;

    assert_eq!(text_of(&events), "你好，世界");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // History endpoint shows both the user message and the assistant reply.
    let stored = stored_messages(&app, &conv_id).await;
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0]["role"], "user");
    assert_eq!(stored[1]["role"], "assistant");
    assert_eq!(stored[1]["parts"][0]["type"], "text");
    assert!(
        stored[1]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("你好，世界")
    );

    // The request the agent sent to the "LLM": system prompt first, then
    // the conversation history.
    let llm_requests = llm_server.requests();
    assert_eq!(llm_requests.len(), 1);
    let messages = &llm_requests[0]["messages"];
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hi");
}

// --- DB-driven model resolution (unified construction entry point) -----------

/// Insert a provider row with an arbitrary adapter_type. The POST endpoint
/// only creates `openai_compatible` rows, so the Anthropic acceptance path
/// writes its row straight into the database (runtime query — no sqlx
/// offline entry needed).
async fn insert_provider_row(
    app: &TestApp,
    id: &str,
    adapter_type: &str,
    base_url: &str,
    api_key: &str,
) {
    sqlx::query(
        "INSERT INTO providers (id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at) \
         VALUES ($1, $1, $2, $3, $4, NULL, 'custom', datetime('now'), datetime('now'))",
    )
    .bind(id)
    .bind(adapter_type)
    .bind(base_url)
    .bind(api_key)
    .execute(&app.pool)
    .await
    .expect("insert provider row");
}

/// Acceptance (backlog: 统一模型构造入口): an `anthropic` adapter row whose
/// base_url/api_key live ONLY in the database must drive the real adapter —
/// the request hits the row's endpoint with the row's key, with no
/// ANTHROPIC_API_KEY environment variable and no built-in URL involved.
/// Editing the row (new key AND new endpoint) then takes effect on the very
/// next request: resolution re-reads the DB per run.
#[tokio::test]
async fn anthropic_provider_row_drives_endpoint_and_key_from_db() {
    let app = TestApp::new().await;

    let first = anthropic_llm::AnthropicMock::spawn(vec![anthropic_llm::AnthropicTurn::Text(
        "hello from the first endpoint".to_string(),
    )])
    .await;
    insert_provider_row(&app, "anthropic", "anthropic", &first.url, "sk-db-first").await;

    let project_id = app.create_project().await;
    // Explicit title ⇒ title_source 'user' ⇒ no background title call to
    // script around; this test is purely about model resolution.
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app
        .send_message_with(&conv_id, "hi", "anthropic:claude-test")
        .await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(text_of(&events), "hello from the first endpoint");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // The whole production chain resolved from the DB row: the row's URL
    // (path `/messages` appended by the adapter), the row's key in the
    // Anthropic header, the version header, and the wire shape.
    let captured = first.requests();
    assert_eq!(
        captured.len(),
        1,
        "exactly one request to the first endpoint"
    );
    assert_eq!(captured[0].path, "/messages");
    assert_eq!(captured[0].api_key.as_deref(), Some("sk-db-first"));
    assert_eq!(captured[0].anthropic_version.as_deref(), Some("2023-06-01"));
    assert_eq!(captured[0].body["model"], "claude-test");
    assert_eq!(captured[0].body["stream"], true);
    assert_eq!(captured[0].body["messages"][0]["role"], "user");

    // The history records the row's provider/model for later turns.
    let stored = stored_messages(&app, &conv_id).await;
    assert_eq!(stored[0]["role"], "user");
    assert_eq!(stored[0]["provider_id"], "anthropic");
    assert_eq!(stored[0]["model_id"], "claude-test");

    // Settings change on the very row: a new key AND a new endpoint. The
    // next request must re-resolve from the database — the second mock
    // answers with the new key, and the first mock never hears from us
    // again.
    let second = anthropic_llm::AnthropicMock::spawn(vec![anthropic_llm::AnthropicTurn::Text(
        "hello from the second endpoint".to_string(),
    )])
    .await;
    let resp = app
        .put_json(
            "/api/providers/anthropic",
            serde_json::json!({"api_key": "sk-db-second", "base_url": second.url}),
        )
        .await;
    assert_eq!(resp.status(), 200, "provider update should succeed");

    let resp = app
        .send_message_with(&conv_id, "again", "anthropic:claude-test")
        .await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(text_of(&events), "hello from the second endpoint");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    assert_eq!(
        first.requests().len(),
        1,
        "the old endpoint must not be hit again"
    );
    let captured = second.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/messages");
    assert_eq!(captured[0].api_key.as_deref(), Some("sk-db-second"));
}

#[tokio::test]
async fn chat_tool_round_trip_over_http() {
    let app = TestApp::new().await;
    tokio::fs::write(app.dir.path().join("a.txt"), "hello\nworld\n")
        .await
        .unwrap();
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk("call-1", "read_file", r#"{"path":"a.txt"}"#),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "read a.txt").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    let types = event_types(&events);

    // Announce → execute → result → final text → finish, over the wire.
    let start = types
        .iter()
        .position(|t| *t == "tool_call_start")
        .expect("tool_call_start in stream");
    let result = types
        .iter()
        .position(|t| *t == "tool_result")
        .expect("tool_result in stream");
    assert!(start < result);
    let tool_result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(tool_result["data"]["is_error"], false);
    assert!(
        tool_result["data"]["content"]
            .as_str()
            .unwrap()
            .contains("1. hello")
    );
    assert_eq!(*types.last().unwrap(), "finish");

    // Persistence: user → assistant(tool call) → tool → assistant.
    let stored = stored_messages(&app, &conv_id).await;
    let roles: Vec<&str> = stored.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);

    // The second LLM request carries the tool result (the feed-back
    // contract, asserted on the real wire).
    let llm_requests = llm_server.requests();
    assert_eq!(llm_requests.len(), 2);
    let second_messages = llm_requests[1]["messages"].as_array().unwrap();
    let tool_message = second_messages.last().unwrap();
    assert_eq!(tool_message["role"], "tool");
    assert_eq!(tool_message["tool_call_id"], "call-1");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("1. hello")
    );
}

#[tokio::test]
async fn chat_reads_file_in_additional_dir_over_http() {
    let app = TestApp::new().await;
    let extra = tempfile::tempdir().expect("additional dir");
    tokio::fs::write(extra.path().join("shared.md"), "shared\nnotes\n")
        .await
        .unwrap();
    let extra_path = extra.path().display().to_string();
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-1",
                "read_file",
                &format!(r#"{{"path":"{extra_path}/shared.md"}}"#),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;

    // The project's sandbox spans the primary temp dir plus the extra dir.
    let resp = app
        .post_json(
            "/api/projects",
            serde_json::json!({
                "name": "multi",
                "working_dir": app.dir.path().to_string_lossy(),
                "additional_dirs": [extra_path],
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let project: Value = resp.json().await.unwrap();
    assert_eq!(
        project["additional_dirs"],
        serde_json::json!([extra_path]),
        "create must return the normalized list"
    );
    let project_id = project["id"].as_str().unwrap().to_string();
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "read the shared notes").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;

    let tool_result = events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .expect("tool_result in stream");
    assert_eq!(tool_result["data"]["is_error"], false);
    assert!(
        tool_result["data"]["content"]
            .as_str()
            .unwrap()
            .contains("1. shared")
    );

    // The system prompt announced the additional root and the absolute-path
    // rule for it.
    let llm_requests = llm_server.requests();
    let system = llm_requests[0]["messages"][0]["content"]
        .as_str()
        .expect("system message");
    assert!(system.contains(&extra_path));
    assert!(system.contains("absolute paths"));
}

#[tokio::test]
async fn project_additional_dirs_roundtrip_and_normalize() {
    let app = TestApp::new().await;
    let working = app.dir.path().to_string_lossy().to_string();
    let extra = tempfile::tempdir().expect("additional dir");
    let extra_path = extra.path().display().to_string();

    // Duplicates, blanks and the primary working dir itself are dropped.
    let resp = app
        .post_json(
            "/api/projects",
            serde_json::json!({
                "name": "p",
                "working_dir": working,
                "additional_dirs": [extra_path, "  ", extra_path, working],
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let project: Value = resp.json().await.unwrap();
    assert_eq!(project["additional_dirs"], serde_json::json!([extra_path]));
    let id = project["id"].as_str().unwrap().to_string();

    // Omitting the field on PATCH leaves the list unchanged.
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"name": "renamed"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["name"], "renamed");
    assert_eq!(fetched["additional_dirs"], serde_json::json!([extra_path]));

    // An explicit empty list clears it.
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"additional_dirs": []}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["additional_dirs"], serde_json::json!([]));

    // Relative entries are rejected loudly at the API boundary: a relative
    // path would anchor to the SERVER process's CWD, silently widening the
    // sandbox to a directory the user never named.
    let resp = app
        .post_json(
            "/api/projects",
            serde_json::json!({
                "name": "rel",
                "working_dir": working,
                "additional_dirs": ["../sibling"],
            }),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"additional_dirs": ["/abs", "relative/dir"]}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Changing working_dir re-normalizes the kept list: an entry that now
    // equals the new primary is dropped instead of lingering as a duplicate
    // root.
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"additional_dirs": [extra_path]}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app
        .patch_json(
            &format!("/api/projects/{id}"),
            serde_json::json!({"working_dir": extra_path}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app.get_json(&format!("/api/projects/{id}")).await;
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["working_dir"], extra_path);
    assert_eq!(fetched["additional_dirs"], serde_json::json!([]));
}

#[tokio::test]
async fn files_endpoints_serve_additional_dirs_via_absolute_path() {
    let app = TestApp::new().await;
    let extra = tempfile::tempdir().expect("additional dir");
    tokio::fs::write(extra.path().join("readme.txt"), "extra content")
        .await
        .unwrap();
    let outside = tempfile::tempdir().expect("outside dir");
    tokio::fs::write(outside.path().join("secret.txt"), "nope")
        .await
        .unwrap();
    let extra_path = extra.path().display().to_string();
    let outside_file = outside.path().join("secret.txt").display().to_string();

    let resp = app
        .post_json(
            "/api/projects",
            serde_json::json!({
                "name": "p",
                "working_dir": app.dir.path().to_string_lossy(),
                "additional_dirs": [extra_path],
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let project: Value = resp.json().await.unwrap();
    let id = project["id"].as_str().unwrap().to_string();

    // Listing the additional root: entries stay addressable by absolute path.
    let resp = app
        .get_json(&format!("/api/projects/{id}/files?path={extra_path}"))
        .await;
    assert_eq!(resp.status(), 200);
    let entries: Vec<Value> = resp.json().await.unwrap();
    let readme = entries
        .iter()
        .find(|e| e["name"] == "readme.txt")
        .expect("readme.txt in the additional root");
    assert_eq!(readme["path"], format!("{extra_path}/readme.txt"));

    // Reading a file inside the additional root works.
    let resp = app
        .get_json(&format!(
            "/api/projects/{id}/files/content?path={extra_path}/readme.txt"
        ))
        .await;
    assert_eq!(resp.status(), 200);
    let content: Value = resp.json().await.unwrap();
    assert_eq!(content["content"], "extra content");

    // Paths outside every root are rejected, list and content alike.
    let resp = app
        .get_json(&format!("/api/projects/{id}/files?path={outside_file}"))
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .get_json(&format!(
            "/api/projects/{id}/files/content?path={outside_file}"
        ))
        .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn chat_conflict_cancel_then_session_frees() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        // First run hangs mid-stream; the run must end via cancellation.
        LlmTurn::Hang(vec![text_chunk("part")]),
        LlmTurn::Complete(vec![finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Start a run: response headers arrive (status 200), stream stays open.
    let first = app.send_message(&conv_id, "slow one").await;
    assert_eq!(first.status(), 200);

    // A second run for the same conversation is rejected while one is live;
    // the stable code distinguishes it from "nothing to resume".
    let conflict = app.send_message(&conv_id, "again").await;
    assert_eq!(conflict.status(), 409);
    let body: Value = conflict.json().await.unwrap();
    assert_eq!(body["code"], "already_running");

    // Cancel, then drain the first stream: it must end with `cancelled`
    // (the stream EOFs because the session entry is dropped after the loop
    // returns).
    let cancel = app.cancel(&conv_id).await;
    assert_eq!(cancel.status(), 200);
    let events = app.read_sse(first).await;
    assert_eq!(*event_types(&events).last().unwrap(), "cancelled");

    // The session is free again: a new run is accepted and completes.
    let second = app.send_message(&conv_id, "after cancel").await;
    assert_eq!(second.status(), 200);
    let events = app.read_sse(second).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");
}

#[tokio::test]
async fn chat_error_paths_release_the_session() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Malformed provider spec → 400, and the session must not stay stuck.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({"content": "x", "provider_spec": "no-colon-here"}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // Unknown provider → 404.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({"content": "x", "provider_spec": "ghost:model"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // Unknown conversation → 404.
    let resp = app
        .post_json(
            "/api/conversations/00000000-0000-0000-0000-000000000000/messages",
            serde_json::json!({"content": "x", "provider_spec": "a:b"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // After all those failures the conversation accepts a real run.
    app.register_llm_provider(&llm_server).await;
    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(text_of(&events), "ok");
}

#[tokio::test]
async fn chat_surfaces_llm_failure_as_error_event() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Fail(500)]).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;

    let last = events.last().expect("at least one event");
    assert_eq!(last["type"], "error");
    assert!(last["data"].as_str().unwrap().contains("LLM error"));
}

// --- Resume of interrupted turns ----------------------------------------------

#[tokio::test]
async fn resume_repairs_dangling_tool_call_and_streams_the_turn() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("已恢复"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    seed_dangling_tool_call(&app, &conv_id).await;

    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;

    assert!(text_of(&events).contains("已恢复"));
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // The placeholder tool row was persisted between the dangling call and
    // the new assistant reply, carrying an error result + UI title.
    let stored = stored_messages(&app, &conv_id).await;
    let roles: Vec<&str> = stored.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
    let tool_row = &stored[2];
    assert_eq!(tool_row["parts"][0]["type"], "tool_result");
    assert_eq!(tool_row["parts"][0]["call_id"], "call-1");
    assert_eq!(tool_row["parts"][0]["is_error"], true);
    assert_eq!(tool_row["details"]["extra"]["title"], "执行被中断");
    assert_eq!(stored[3]["parts"][0]["type"], "text");
    assert!(
        stored[3]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("已恢复")
    );

    // The LLM request carried a legal conversation: the tool call is paired
    // with a tool result, and no new user message was appended for a resume.
    let llm_requests = llm_server.requests();
    assert_eq!(llm_requests.len(), 1);
    let msgs = llm_requests[0]["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);
    assert_eq!(msgs[3]["tool_call_id"], "call-1");
}

#[tokio::test]
async fn send_message_repairs_dangling_history_before_calling_the_llm() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("done"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    seed_dangling_tool_call(&app, &conv_id).await;

    // Not resuming — just sending a new message must not hand the provider
    // the dangling (illegal) history either.
    let resp = app.send_message(&conv_id, "next question").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let llm_requests = llm_server.requests();
    assert_eq!(llm_requests.len(), 1);
    let msgs = llm_requests[0]["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
    let tool_message = &msgs[3];
    assert_eq!(tool_message["tool_call_id"], "call-1");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("工具执行被中断")
    );
}

#[tokio::test]
async fn resume_conflicts_when_nothing_to_resume() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![text_chunk("again"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Empty conversation: nothing to resume.
    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "nothing_to_resume");

    // A fully answered conversation is not resumable either.
    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "nothing_to_resume");

    // The rejected resumes did not leave the session stuck.
    let resp = app.send_message(&conv_id, "again").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
}

#[tokio::test]
async fn resume_conflicts_while_a_run_is_in_progress() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Hang(vec![text_chunk("part")]),
        LlmTurn::Complete(vec![finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // The history would be resumable (trailing unanswered user message), but
    // a run is already live for the conversation.
    let first = app.send_message(&conv_id, "slow one").await;
    assert_eq!(first.status(), 200);
    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "already_running");

    // The live run is unaffected: cancel still ends it and frees the session.
    let cancel = app.cancel(&conv_id).await;
    assert_eq!(cancel.status(), 200);
    let events = app.read_sse(first).await;
    assert_eq!(*event_types(&events).last().unwrap(), "cancelled");
}

#[tokio::test]
async fn resume_without_recorded_provider_spec_is_a_bad_request() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Resumable history (trailing user message) but no provider/model ever
    // recorded on any message.
    msg_db::insert_message(
        &app.pool,
        &conv_id,
        &Message::text(Role::User, "orphan turn"),
        None,
        None,
        None,
    )
    .await
    .expect("seed user message");

    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 400);
}

// --- 重连重放（前端刷新后 attach 运行中的 run） ----------------------------------

/// Take the next event from an incremental SSE stream with a timeout
/// backstop; None means the stream reached EOF.
async fn next_event(
    stream: &mut (impl futures_util::Stream<Item = Value> + Unpin),
) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("timed out waiting for sse event (run stalled?)")
}

#[tokio::test]
async fn run_status_and_events_when_idle() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.run_status(&conv_id).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["running"], false);

    // No run → 404 (the frontend reads this as "already finished").
    let resp = app.run_events(&conv_id).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reconnect_replays_from_run_start_and_continues_live() {
    let app = TestApp::new().await;
    let (gate_tx, gate_rx) = llm::gate();
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Gated {
        pre: vec![text_chunk("你好")],
        gate: gate_rx,
        post: vec![text_chunk("，世界"), finish_chunk("stop")],
    }])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // 原连接：run 启动，先吐一段 delta，随后被 gate 挂住。
    let first = app.send_message(&conv_id, "hi").await;
    assert_eq!(first.status(), 200);
    let mut original = sse_payloads(first);
    // 确定性地等 pre delta 已发布：原连接收到它 ⟺ hub 日志已记录它。
    let pre = next_event(&mut original)
        .await
        .expect("original stream should deliver the pre delta");
    assert_eq!(pre["type"], "text_delta");
    assert_eq!(pre["data"], "你好");

    // gate 仍关着：run 确实还在跑。
    let resp = app.run_status(&conv_id).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["running"], true);

    // 前端刷新后重连。响应头到达 ⟹ 服务端订阅已建立，快照已定。
    let reconnected_resp = app.run_events(&conv_id).await;
    assert_eq!(reconnected_resp.status(), 200);
    let mut reconnected = sse_payloads(reconnected_resp);

    // 重放部分：run_meta（锚定本次 run 的 user 消息行）+ 已发布的 delta。
    let meta = next_event(&mut reconnected).await.expect("run_meta first");
    assert_eq!(meta["type"], "run_meta");
    let user_id = stored_messages(&app, &conv_id).await[0]["id"].clone();
    assert_eq!(meta["data"]["start_message_id"], user_id);
    let replayed = next_event(&mut reconnected).await.expect("replayed delta");
    assert_eq!(
        replayed, pre,
        "replay must contain exactly the published delta"
    );

    // 放行 gate：增量续播到 finish，随后 run 结束 ⟹ hub drop ⟹ 流 EOF。
    gate_tx.send(true).expect("gate receiver alive");

    let mut reconnected_events = vec![meta, replayed];
    while let Some(e) = next_event(&mut reconnected).await {
        reconnected_events.push(e);
    }
    assert_eq!(
        event_types(&reconnected_events),
        vec!["run_meta", "text_delta", "text_delta", "finish"],
        "快照 + 增量无重复、无缺口"
    );
    assert_eq!(text_of(&reconnected_events), "你好，世界");

    // 原连接与重连并存（多订阅者），各自完整地收到增量，且原连接不含 run_meta。
    let mut original_events = vec![pre];
    while let Some(e) = next_event(&mut original).await {
        original_events.push(e);
    }
    assert_eq!(
        event_types(&original_events),
        vec!["text_delta", "text_delta", "finish"]
    );
    assert_eq!(text_of(&original_events), "你好，世界");

    // 持久化完整，session 已释放。
    let stored = stored_messages(&app, &conv_id).await;
    let roles: Vec<&str> = stored.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant"]);
    assert_eq!(stored[1]["parts"][0]["type"], "text");
    assert!(
        stored[1]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("你好，世界")
    );

    let resp = app.run_status(&conv_id).await;
    assert_eq!(resp.json::<Value>().await.unwrap()["running"], false);
}

#[tokio::test]
async fn reconnect_supports_multiple_concurrent_event_subscribers() {
    let app = TestApp::new().await;
    let (gate_tx, gate_rx) = llm::gate();
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Gated {
        pre: vec![text_chunk("部分")],
        gate: gate_rx,
        post: vec![text_chunk("回答"), finish_chunk("stop")],
    }])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let first = app.send_message(&conv_id, "hi").await;
    assert_eq!(first.status(), 200);
    let mut original = sse_payloads(first);
    let pre = next_event(&mut original).await.expect("pre delta");
    assert_eq!(pre["data"], "部分");

    // 两个先后到达的重连订阅者（都在 gate 关闭期间）。
    let mut sub2 = sse_payloads(app.run_events(&conv_id).await);
    let mut sub3 = sse_payloads(app.run_events(&conv_id).await);

    gate_tx.send(true).expect("gate receiver alive");

    let mut sub2_events = Vec::new();
    while let Some(e) = next_event(&mut sub2).await {
        sub2_events.push(e);
    }
    let mut sub3_events = Vec::new();
    while let Some(e) = next_event(&mut sub3).await {
        sub3_events.push(e);
    }

    // 各自独立收全量：run_meta + 完整重放 + 增量，序列一致。
    assert_eq!(sub2_events, sub3_events);
    assert_eq!(text_of(&sub2_events), "部分回答");
    assert_eq!(*event_types(&sub2_events).last().unwrap(), "finish");

    // 原连接也完整结束。
    let mut original_events = vec![pre];
    while let Some(e) = next_event(&mut original).await {
        original_events.push(e);
    }
    assert_eq!(*event_types(&original_events).last().unwrap(), "finish");
}

#[tokio::test]
async fn reconnect_stream_receives_cancelled() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Hang(vec![text_chunk("part")])]).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let first = app.send_message(&conv_id, "slow one").await;
    assert_eq!(first.status(), 200);
    let mut original = sse_payloads(first);
    let pre = next_event(&mut original).await.expect("pre delta");
    assert_eq!(pre["data"], "part");

    // 重连后再取消：取消事件要出现在重连后的流上。
    let mut reconnected = sse_payloads(app.run_events(&conv_id).await);
    let meta = next_event(&mut reconnected).await.expect("run_meta");
    assert_eq!(meta["type"], "run_meta");
    let replayed = next_event(&mut reconnected).await.expect("replayed delta");
    assert_eq!(replayed, pre);

    let cancel = app.cancel(&conv_id).await;
    assert_eq!(cancel.status(), 200);

    let mut reconnected_events = vec![meta, replayed];
    while let Some(e) = next_event(&mut reconnected).await {
        reconnected_events.push(e);
    }
    assert_eq!(
        *event_types(&reconnected_events).last().unwrap(),
        "cancelled"
    );

    // 原连接同样以 cancelled 收尾，session 释放。
    let mut original_events = vec![pre];
    while let Some(e) = next_event(&mut original).await {
        original_events.push(e);
    }
    assert_eq!(*event_types(&original_events).last().unwrap(), "cancelled");

    let resp = app.run_status(&conv_id).await;
    assert_eq!(resp.json::<Value>().await.unwrap()["running"], false);
}

// --- Title generation (background, one-shot) ----------------------------------

/// POST a conversation WITHOUT a title — the state title generation fires
/// for (an explicit title marks the row user-named and skips generation).
async fn create_untitled_conversation(app: &TestApp, project_id: &str) -> String {
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/conversations"),
            serde_json::json!({"title": null}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let conv: Value = resp.json().await.expect("conversation json");
    assert_eq!(conv["title"], "New conversation");
    assert_eq!(conv["title_source"], "default");
    conv["id"].as_str().expect("conversation id").to_string()
}

async fn get_conversation(app: &TestApp, project_id: &str, conv_id: &str) -> Value {
    let resp = app
        .get_json(&format!(
            "/api/projects/{project_id}/conversations/{conv_id}"
        ))
        .await;
    assert_eq!(resp.status(), 200);
    resp.json().await.expect("conversation json")
}

/// Bounded poll of the conversation row until `pred` holds. The title task
/// is a detached background job with no test-visible channel, so its only
/// observable effects are the row and (when the run still lives) the SSE
/// stream.
async fn wait_conversation(
    app: &TestApp,
    project_id: &str,
    conv_id: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let conv = get_conversation(app, project_id, conv_id).await;
        if pred(&conv) {
            return conv;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for conversation state, last: {conv}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn first_message_generates_title_and_streams_event() {
    let app = TestApp::new().await;
    let (gate_tx, gate_rx) = llm::gate();
    // The run parks mid-stream so the title task is guaranteed to finish
    // while the run's hub (and its SSE subscribers) is still alive.
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Gated {
            pre: vec![text_chunk("working")],
            gate: gate_rx,
            post: vec![finish_chunk("stop")],
        },
        // 第二轮(下方 one-shot 断言用)正常完成。
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    llm_server.push_title_turns([llm::TitleTurn::Text(
        "  \"修复登录按钮\"  \n(unused second line)".to_string(),
    )]);
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = create_untitled_conversation(&app, &project_id).await;

    let resp = app.send_message(&conv_id, "登录按钮坏了,帮我修一下").await;
    assert_eq!(resp.status(), 200);
    let mut stream = sse_payloads(resp);

    // Title lands while the run is parked: quoted/multiline reply cleaned up.
    let conv = wait_conversation(&app, &project_id, &conv_id, |c| c["title_source"] == "ai").await;
    assert_eq!(conv["title"], "修复登录按钮");

    // The published event is buffered on the already-open subscriber; drain
    // until it appears, then release the run.
    let mut saw_title_event = false;
    let mut events = Vec::new();
    while let Some(event) = next_event(&mut stream).await {
        saw_title_event |=
            event["type"] == "title_updated" && event["data"]["title"] == "修复登录按钮";
        events.push(event);
        if saw_title_event {
            break;
        }
    }
    assert!(saw_title_event, "title_updated must reach live subscribers");

    gate_tx.send(true).unwrap();
    while let Some(event) = next_event(&mut stream).await {
        events.push(event);
    }
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // The title request was tool-less, two messages, capped output.
    let title_request = llm_server
        .requests()
        .into_iter()
        .find(|r| r.get("stream") != Some(&serde_json::json!(true)))
        .expect("a non-streaming title request");
    // An empty tool list is omitted from the wire request entirely.
    assert!(
        title_request
            .get("tools")
            .is_none_or(|t| t.as_array().is_none_or(Vec::is_empty))
    );
    assert_eq!(title_request["max_tokens"], 100);
    let roles: Vec<&str> = title_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["system", "user"]);

    // One-shot: a second message must not touch the title again.
    let resp = app.send_message(&conv_id, "thanks").await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;
    let conv = get_conversation(&app, &project_id, &conv_id).await;
    assert_eq!(conv["title"], "修复登录按钮");
    assert_eq!(conv["title_source"], "ai");
}

/// A fork copies the source's history, so the plain "first user message"
/// guard can never fire for it — and regenerating at FORK time would be
/// pointless anyway: the fork's content is then identical to the source's
/// and the model would just re-derive the same title. The fork therefore
/// starts with the「（分支）」placeholder over a `default` title source, and
/// the one-shot fires on its first OWN message — the moment the branch
/// actually diverges and a new title can say something new.
#[tokio::test]
async fn forked_conversation_regenerates_title_on_its_first_own_message() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        // The source's first run.
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
        // The fork's first own-message run.
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
        // A later fork message (one-shot assertion below).
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
    ])
    .await;
    llm_server.push_title_turns([
        llm::TitleTurn::Text("  \"修复登录按钮\"  ".to_string()),
        llm::TitleTurn::Text("改问 2+2 等于几".to_string()),
    ]);
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;

    // Source: a normal first exchange, auto-titled as usual.
    let conv_id = create_untitled_conversation(&app, &project_id).await;
    let resp = app.send_message(&conv_id, "登录按钮坏了,帮我修一下").await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;
    let source =
        wait_conversation(&app, &project_id, &conv_id, |c| c["title_source"] == "ai").await;
    assert_eq!(source["title"], "修复登录按钮");

    // Fork at head: placeholder keeps the「（分支）」disambiguator, but the row
    // is default — eligible for the one-shot despite the copied history.
    let revision: i64 = app
        .get_json(&format!("/api/conversations/{conv_id}/messages"))
        .await
        .headers()["X-Context-Revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let fork_resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/fork"),
            serde_json::json!({
                "operation_id": uuid::Uuid::new_v4().to_string(),
                "expected_revision": revision,
                "cut": {"kind": "head"},
                "title": null,
            }),
        )
        .await;
    assert_eq!(fork_resp.status(), 201);
    let fork: Value = fork_resp.json().await.unwrap();
    let fork_id = fork["conversation"]["id"].as_str().unwrap().to_string();
    assert_eq!(fork["conversation"]["title"], "修复登录按钮（分支）");
    assert_eq!(fork["conversation"]["title_source"], "default");

    // The fork's first own message is the divergence point: regenerate.
    let resp = app.send_message(&fork_id, "不对，改成问 2+2 等于几").await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;
    let forked =
        wait_conversation(&app, &project_id, &fork_id, |c| c["title_source"] == "ai").await;
    assert_eq!(forked["title"], "改问 2+2 等于几");

    // The title model was fed the fork's own message, not the copied first.
    let title_request = llm_server
        .requests()
        .into_iter()
        .rfind(|r| r.get("stream") != Some(&serde_json::json!(true)))
        .expect("fork title request");
    assert_eq!(
        title_request["messages"][1]["content"],
        "不对，改成问 2+2 等于几"
    );

    // Still one-shot: a later fork message doesn't touch the title again.
    let resp = app.send_message(&fork_id, "thanks").await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;
    let conv = get_conversation(&app, &project_id, &fork_id).await;
    assert_eq!(conv["title"], "改问 2+2 等于几");
    assert_eq!(conv["title_source"], "ai");
}

#[tokio::test]
async fn manual_rename_blocks_title_generation() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = create_untitled_conversation(&app, &project_id).await;

    // Rename BEFORE the first message flips title_source to user.
    let resp = app
        .patch_json(
            &format!("/api/projects/{project_id}/conversations/{conv_id}"),
            serde_json::json!({"title": "我的名字"}),
        )
        .await;
    assert_eq!(resp.status(), 200);

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;

    let conv = get_conversation(&app, &project_id, &conv_id).await;
    assert_eq!(conv["title"], "我的名字");
    assert_eq!(conv["title_source"], "user");
    // No title request was made (only the run's streaming call).
    assert_eq!(llm_server.requests().len(), 1);
}

#[tokio::test]
async fn title_failure_falls_back_to_first_message() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("done"),
        finish_chunk("stop"),
    ])])
    .await;
    llm_server.push_title_turns([llm::TitleTurn::Fail(500)]);
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = create_untitled_conversation(&app, &project_id).await;

    let long_content = "这条用户消息故意写得超过五十个字符,这样回退标题就必须走截断分支并追加省略号,从而验证按字符边界截断的行为";
    assert!(long_content.chars().count() > 50);
    let resp = app.send_message(&conv_id, long_content).await;
    assert_eq!(resp.status(), 200);
    let _ = app.read_sse(resp).await;

    // Fallback: whitespace-collapsed first message capped at 50 chars + ….
    let conv = wait_conversation(&app, &project_id, &conv_id, |c| c["title_source"] == "ai").await;
    let title = conv["title"].as_str().unwrap();
    let chars: Vec<char> = title.chars().collect();
    assert_eq!(chars.last(), Some(&'…'));
    assert_eq!(chars.len(), 51);
    assert!(title.starts_with("这条用户消息故意写得超过五十个字符"));
}

// --- Context compaction -------------------------------------------------------

use oct_agent::agent::compact;

/// Seed alternating user/assistant rounds recording the provider, so the
/// conversation has a resolvable provider_spec and enough history to compact.
async fn seed_compactable_rounds(app: &TestApp, conv_id: &str, rounds: usize, pad_chars: usize) {
    let pad = "x".repeat(pad_chars);
    for i in 0..rounds {
        for (role, label) in [(Role::User, "user"), (Role::Assistant, "assistant")] {
            msg_db::insert_message(
                &app.pool,
                conv_id,
                &Message::text(role, format!("seed-{label}-{i} {pad}")),
                Some("mock-llm"),
                Some("test-model"),
                None,
            )
            .await
            .expect("seed history row");
        }
    }
}

/// POST /api/providers/{id}/models with a context limit; auto compaction is
/// driven by this number (custom providers carry no in-code limits).
async fn create_model_row_with_window(app: &TestApp, limit_context: i64) {
    let resp = app
        .post_json(
            "/api/providers/mock-llm/models",
            serde_json::json!({
                "model_id": "test-model",
                "name": "Test Model",
                "limit_context": limit_context,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201, "model row creation should succeed");
}

#[tokio::test]
async fn manual_compact_replaces_history_for_next_request() {
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("hello"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    // No model row → unknown window → the default 16k tail budget applies.
    // The huge second-to-last message pushes the budget cut back to a legal
    // boundary with ≥6 rows in the compactable region.
    seed_compactable_rounds(&app, &conv, 3, 200).await;
    let huge = "y".repeat(70_000);
    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, format!("seed-user-3 {huge}")),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::Assistant, "seed-assistant-3 small reply"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();

    llm.push_compact_turns([llm::TitleTurn::Text(
        "MANUAL-SUMMARY of the work".to_string(),
    )]);

    let resp = app.compact(&conv).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("compact result json");
    assert_eq!(body["summary"], "MANUAL-SUMMARY of the work");
    assert_eq!(body["triggered_by"], serde_json::json!(null)); // not part of the REST body
    assert!(
        body["compacted_messages"].as_u64().unwrap() >= 6,
        "most of the history is compacted: {body}"
    );
    assert!(
        body["summary_message_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(
        body["est_tokens_before"].as_u64().unwrap() > body["est_tokens_after"].as_u64().unwrap()
    );

    // The marker sits at the CUT — between the summarized region and the
    // tail — so the last row is still the seeded tail assistant, and the old
    // rows remain in the timeline (append-only).
    let stored = stored_messages(&app, &conv).await;
    let last = stored.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(stored.len(), 8 + 1, "seeded rows kept + marker");
    let marker = stored
        .iter()
        .find(|m| m["details"]["kind"] == "compaction")
        .expect("marker row");
    assert_eq!(marker["details"]["compaction"]["triggered_by"], "manual");
    assert_eq!(
        marker["ordering"].as_i64(),
        Some(8),
        "marker takes over the first tail slot"
    );
    assert_eq!(last["ordering"].as_i64(), Some(9), "tail shifted up");

    // The next send's wire request must carry [system, marker, tail assistant,
    // user] — the seeded region is gone from the model's view, the tail
    // survives.
    let resp = app.send_message(&conv, "next question").await;
    let events = app.read_sse(resp).await;
    assert!(event_types(&events).contains(&"finish"));

    let stream_req = llm
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.get("stream") == Some(&serde_json::json!(true)))
        .expect("streaming request captured");
    let serialized = serde_json::to_string(&stream_req["messages"]).unwrap();
    assert!(serialized.contains("MANUAL-SUMMARY of the work"));
    assert!(serialized.contains("[Context compaction]"));
    assert!(serialized.contains("seed-assistant-3"), "tail survives");
    assert!(!serialized.contains("seed-user-0"), "old history excluded");
}

#[tokio::test]
async fn manual_compact_conflicts_with_active_run() {
    let (gate_tx, gate_rx) = llm::gate();
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Gated {
        pre: vec![text_chunk("par")],
        gate: gate_rx,
        post: vec![text_chunk("tial"), finish_chunk("stop")],
    }])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    let send = app.send_message(&conv, "hello").await;
    let mut payloads = sse_payloads(send);
    // Park mid-stream: the run holds the session entry.
    while payloads.next().await.unwrap()["type"] != "text_delta" {}

    let resp = app.compact(&conv).await;
    assert_eq!(resp.status(), 409);

    gate_tx.send(true).unwrap();
    // Drain to EOF, not just to the finish event: the supervisor persists
    // the run's terminal state BEFORE releasing occupancy, so the stream
    // ending IS the deterministic release signal (the finish event itself
    // may still race the terminal write).
    while payloads.next().await.is_some() {}
    // The session entry is released by the run; compact is possible again.
    let resp = app.compact(&conv).await;
    assert_eq!(
        resp.status(),
        400,
        "run over → no 409; conversation too short to compact instead"
    );
}

// --- Frozen prompt environment (repo instructions reload) --------------------

#[tokio::test]
async fn instructions_reload_returns_manifest_and_refreshes_the_next_prompt() {
    let llm = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![text_chunk("ok"), finish_chunk("stop")]),
    ])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    let agents_md = app.dir.path().join("AGENTS.md");
    std::fs::write(&agents_md, "RULE-v1").unwrap();

    // First send freezes the environment with the v1 text in the prompt head.
    let resp = app.send_message(&conv, "hello").await;
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    let system1 = llm.requests()[0]["messages"][0]["content"]
        .as_str()
        .expect("system message")
        .to_string();
    assert!(system1.contains("RULE-v1"));
    assert!(system1.contains("<repo-instructions>"));

    // The reload reports exactly what is now frozen: the file manifest with
    // budgeted byte counts, and the (empty) MCP composition.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv}/instructions/reload"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let manifest: Value = resp.json().await.expect("reload manifest json");
    let files = manifest["repo_files"].as_array().expect("repo_files array");
    assert_eq!(files.len(), 1);
    assert!(files[0]["path"].as_str().unwrap().ends_with("AGENTS.md"));
    assert_eq!(files[0]["bytes"].as_u64(), Some("RULE-v1".len() as u64));
    assert_eq!(files[0]["truncated"], serde_json::json!(false));
    assert_eq!(manifest["mcp_tools"], serde_json::json!([]));

    // Editing WITHOUT a reload keeps the frozen block (prefix-cache
    // guarantee)…
    std::fs::write(&agents_md, "RULE-v2-EDITED").unwrap();
    let resp = app.send_message(&conv, "again").await;
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    let system2 = llm.requests()[1]["messages"][0]["content"]
        .as_str()
        .expect("system message")
        .to_string();
    assert!(
        system2.contains("RULE-v1"),
        "no refresh point → frozen block"
    );
    assert!(!system2.contains("RULE-v2-EDITED"));

    // …while a reload after the edit swaps the block for the next run.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv}/instructions/reload"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let manifest: Value = resp.json().await.expect("reload manifest json");
    assert_eq!(
        manifest["repo_files"][0]["bytes"].as_u64(),
        Some("RULE-v2-EDITED".len() as u64)
    );
    llm.push_turns([LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])]);
    let resp = app.send_message(&conv, "once more").await;
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    let system3 = llm.requests()[2]["messages"][0]["content"]
        .as_str()
        .expect("system message")
        .to_string();
    assert!(
        system3.contains("RULE-v2-EDITED"),
        "reload replaced the frozen block"
    );

    // Unknown conversation → 404.
    let resp = app
        .post_json(
            "/api/conversations/does-not-exist/instructions/reload",
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn instructions_reload_rejects_managed_subagent_conversation() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    // A managed subagent conversation row (as the task tool would create it).
    let mut tx = app.pool.begin().await.unwrap();
    let child = oct_agent::db::conversations::create_subagent_conversation_in_tx(
        &mut tx,
        oct_agent::db::conversations::NewSubagentConversation {
            project_id: &project,
            parent_conversation_id: &conv,
            root_conversation_id: &conv,
            agent_type: "worker",
            title: "child task",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let resp = app
        .post_json(
            &format!("/api/conversations/{}/instructions/reload", child.id),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "managed_conversation");
}

#[tokio::test]
async fn auto_compaction_end_to_end_via_tiny_model_window() {
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("done"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    create_model_row_with_window(&app, 500).await; // threshold 425
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    seed_compactable_rounds(&app, &conv, 4, 200).await;
    llm.push_compact_turns([llm::TitleTurn::Text("AUTO-SUMMARY".to_string())]);

    let resp = app.send_message(&conv, "keep going").await;
    let events = app.read_sse(resp).await;
    let types = event_types(&events);
    let compacting = types
        .iter()
        .position(|t| *t == "compacting")
        .expect("compacting event");
    let compacted = types
        .iter()
        .position(|t| *t == "compacted")
        .expect("compacted event");
    let text = types.iter().position(|t| *t == "text_delta").unwrap();
    assert!(compacting < compacted && compacted < text);
    assert_eq!(types.last().unwrap(), &"finish");

    let compacted_ev = &events[compacted];
    assert_eq!(compacted_ev["data"]["triggered_by"], "auto");
    assert_eq!(compacted_ev["data"]["summary"], "AUTO-SUMMARY");
    assert_eq!(compacted_ev["data"]["context_window_tokens"], 500);
    assert!(
        compacted_ev["data"]["summary_message_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    // The streamed request was built from the compacted view.
    let stream_req = llm
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.get("stream") == Some(&serde_json::json!(true)))
        .expect("streaming request captured");
    let serialized = serde_json::to_string(&stream_req["messages"]).unwrap();
    assert!(serialized.contains("AUTO-SUMMARY"));
    assert!(!serialized.contains("seed-user-0"));
}

#[tokio::test]
async fn dangling_tool_call_before_marker_is_not_repaired() {
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("hi"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    // A crashed run left a dangling tool call, then a compaction marker moved
    // the LLM view past it.
    seed_dangling_tool_call(&app, &conv).await;
    let marker = compact::build_marker_message("already summarized");
    let details = compact::marker_details(oct_agent::agent::CompactionTrigger::Manual, 2, 100, 20);
    msg_db::insert_message(&app.pool, &conv, &marker, None, None, Some(&details))
        .await
        .unwrap();

    let resp = app.send_message(&conv, "continue").await;
    let events = app.read_sse(resp).await;
    assert_eq!(event_types(&events).last().unwrap(), &"finish");

    // No repair placeholder was appended (it would land after the marker and
    // become an orphan inside the view).
    let stored = stored_messages(&app, &conv).await;
    assert!(
        !stored.iter().any(|m| serde_json::to_string(&m)
            .unwrap()
            .contains("工具执行被中断")),
        "pre-marker dangling call left alone: {stored:?}"
    );

    // And the wire request carries neither the dangling call nor an orphan
    // result for it.
    let stream_req = llm
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.get("stream") == Some(&serde_json::json!(true)))
        .expect("streaming request captured");
    let serialized = serde_json::to_string(&stream_req["messages"]).unwrap();
    assert!(
        !serialized.contains("call-1"),
        "dangling call excluded: {serialized}"
    );
}

// --- Microcompact: original-message endpoint --------------------------------

use oct_llm_provider::core::ToolResult;

#[tokio::test]
async fn microcompact_original_endpoint_serves_superseded_content() {
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    // Window 10k: the seeded bulk (~8.2k) sits between the 70% waterline and
    // the 85% summary threshold — microcompact fires, compaction does not.
    create_model_row_with_window(&app, 10_000).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "go"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    for i in 0..8 {
        msg_db::insert_message(
            &app.pool,
            &conv,
            &Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            },
            Some("mock-llm"),
            Some("test-model"),
            None,
        )
        .await
        .unwrap();
        msg_db::insert_message(
            &app.pool,
            &conv,
            &Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: format!("c{i}"),
                    content: serde_json::json!(format!("p{i} ") + &"x".repeat(4_000)),
                    is_error: false,
                    images: Vec::new(),
                })],
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "next"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();

    let resp = app.send_message(&conv, "keep going").await;
    let events = app.read_sse(resp).await;
    assert!(event_types(&events).contains(&"finish"));
    let micro = events
        .iter()
        .find(|e| e["type"] == "microcompacted")
        .expect("microcompacted event");
    assert_eq!(micro["data"]["triggered_by"], "waterline");
    let replacements = micro["data"]["replacements"]
        .as_array()
        .expect("replacements");
    assert_eq!(replacements.len(), 3, "8 results − 5 protected");

    // The endpoint serves the ORIGINAL content through the copy's
    // membership-scoped supersedes hop.
    let copy_id = replacements[0]["copy_message_id"].as_str().unwrap();
    let resp = app
        .get_json(&format!(
            "/api/conversations/{conv}/messages/{copy_id}/original"
        ))
        .await;
    assert_eq!(resp.status(), 200);
    let original: Value = resp.json().await.expect("original json");
    assert_eq!(original["role"], "tool");
    assert_eq!(original["parts"][0]["type"], "tool_result");
    assert!(
        original["parts"][0]["content"]
            .as_str()
            .expect("string content")
            .contains("p0 ")
    );

    // Scope checks: a non-copy row (plain user message) and an unknown id
    // both 404 — the join through supersedes/membership rejects them.
    let user_row = &stored_messages(&app, &conv).await[0];
    let user_id = user_row["id"].as_str().unwrap();
    let resp = app
        .get_json(&format!(
            "/api/conversations/{conv}/messages/{user_id}/original"
        ))
        .await;
    assert_eq!(resp.status(), 404, "non-copy row has no original");
    let resp = app
        .get_json(&format!(
            "/api/conversations/{conv}/messages/00000000-0000-0000-0000-000000000000/original"
        ))
        .await;
    assert_eq!(resp.status(), 404, "unknown id");

    // Cross-conversation scope: another conversation cannot read this copy.
    let conv2 = app.create_conversation(&project).await;
    let resp = app
        .get_json(&format!(
            "/api/conversations/{conv2}/messages/{copy_id}/original"
        ))
        .await;
    assert_eq!(
        resp.status(),
        404,
        "copy not in that conversation's timeline"
    );
}

/// The idle entry must fire through the real API path: `last_activity` is
/// captured BEFORE the new user message is inserted, so a conversation whose
/// stored history is old enough clears on send — and a fresh one does not.
#[tokio::test]
async fn idle_microcompact_triggers_via_api_after_backdated_activity() {
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    create_model_row_with_window(&app, 10_000).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;

    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "go"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    for i in 0..8 {
        msg_db::insert_message(
            &app.pool,
            &conv,
            &Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            },
            Some("mock-llm"),
            Some("test-model"),
            None,
        )
        .await
        .unwrap();
        msg_db::insert_message(
            &app.pool,
            &conv,
            &Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: format!("c{i}"),
                    content: serde_json::json!(format!("p{i} ") + &"x".repeat(4_000)),
                    is_error: false,
                    images: Vec::new(),
                })],
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    msg_db::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "next"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();

    // Backdate the whole history: the send path must observe the idle gap
    // (captured before its own writes), not "just now".
    sqlx::query("UPDATE messages SET created_at = '2020-01-01 00:00:00'")
        .execute(&app.pool)
        .await
        .unwrap();

    let resp = app.send_message(&conv, "keep going").await;
    let events = app.read_sse(resp).await;
    let micro = events
        .iter()
        .find(|e| e["type"] == "microcompacted")
        .expect("idle microcompact fired via the API path");
    assert_eq!(micro["data"]["triggered_by"], "idle");

    // Fresh conversation, recent activity: no microcompact at all.
    let conv2 = app.create_conversation(&project).await;
    msg_db::insert_message(
        &app.pool,
        &conv2,
        &Message::text(Role::User, "hello"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    let resp = app.send_message(&conv2, "again").await;
    let events = app.read_sse(resp).await;
    assert!(
        !events.iter().any(|e| e["type"] == "microcompacted"),
        "recent activity ⇒ no idle clear"
    );
}

// --- agent_runs lifecycle rows (PR1: unified run supervision) -------------------

use oct_agent::db::runs as run_db;

/// (status, final_message_id, error, input_tokens, output_tokens) for each
/// run row of a conversation, oldest first (rowid = insertion order).
async fn agent_run_rows(
    app: &TestApp,
    conv_id: &str,
) -> Vec<(
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
)> {
    sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ),
    >(
        "SELECT status, final_message_id, error, input_tokens, output_tokens \
         FROM agent_runs WHERE conversation_id = ? ORDER BY rowid",
    )
    .bind(conv_id)
    .fetch_all(&app.pool)
    .await
    .expect("agent_runs rows")
}

#[tokio::test]
async fn model_failure_settles_run_row_failed_and_frees_conversation() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        // First attempt: the model's HTTP call itself fails.
        LlmTurn::Fail(500),
        // Second attempt (after the re-send below) succeeds.
        LlmTurn::Complete(vec![text_chunk("recovered"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "error");

    // SSE EOF ⟺ the supervisor released occupancy. The failed attempt
    // settled its run row: failed, error text persisted, no final message.
    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "failed");
    assert_eq!(rows[0].1, None);
    let error = rows[0].2.as_deref().expect("error text persisted");
    assert!(
        error.contains("LLM error"),
        "error should name the LLM failure: {error}"
    );

    // The conversation accepts a new send immediately (no stuck occupancy).
    let resp = app.send_message(&conv_id, "again").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // Every attempt owns its own run row.
    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, "completed");
}

#[tokio::test]
async fn cancelled_run_settles_row_and_re_send_works() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        // First run hangs mid-stream; it must end via cancellation.
        LlmTurn::Hang(vec![text_chunk("part")]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let first = app.send_message(&conv_id, "slow one").await;
    assert_eq!(first.status(), 200);
    let mut stream = sse_payloads(first);
    // Observe the first mock turn before cancellation, so a later send cannot
    // accidentally consume the still-queued hanging response.
    let pre = next_event(&mut stream).await.expect("first text delta");
    assert_eq!(pre["type"], "text_delta");
    let cancel = app.cancel(&conv_id).await;
    assert_eq!(cancel.status(), 200);
    let mut events = vec![pre];
    while let Some(event) = next_event(&mut stream).await {
        events.push(event);
    }
    assert_eq!(*event_types(&events).last().unwrap(), "cancelled");

    // The interrupted attempt settled cancelled; no assistant row had been
    // persisted at cancel time (the message lands only at stream end).
    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(rows[0].1, None);
    assert_eq!(rows[0].2, None, "a cancel is not an error");

    // A re-send succeeds on the same conversation.
    let second = app.send_message(&conv_id, "after cancel").await;
    assert_eq!(second.status(), 200);
    let events = app.read_sse(second).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "cancelled");
    assert_eq!(rows[1].0, "completed");
}

#[tokio::test]
async fn completed_run_row_points_at_final_assistant_message_and_usage() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("the answer"),
        finish_chunk("stop"),
        // The OpenAI usage trailer: aggregated into the run row.
        llm::usage_chunk(120, 30),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");
    // The usage event also reached the SSE stream (the aggregation source).
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "usage" && e["data"]["input_tokens"] == 120)
    );

    // The final assistant row is what the run row must point at.
    let stored = stored_messages(&app, &conv_id).await;
    let assistant = stored.last().expect("assistant row");
    assert_eq!(assistant["role"], "assistant");

    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "completed");
    assert_eq!(
        rows[0].1.as_deref(),
        Some(assistant["id"].as_str().unwrap())
    );
    assert_eq!(rows[0].2, None);
    assert_eq!(rows[0].3, Some(120), "input tokens aggregated");
    assert_eq!(rows[0].4, Some(30), "output tokens aggregated");

    // Full row shape through the typed accessor as well.
    let latest = run_db::latest_run_for_conversation(&app.pool, &conv_id)
        .await
        .expect("latest run query")
        .expect("one run row");
    assert_eq!(latest.status, "completed");
    assert_eq!(latest.kind, "root");
    assert_eq!(latest.provider_id.as_deref(), Some("mock-llm"));
    assert_eq!(latest.model_id.as_deref(), Some("test-model"));
    assert_eq!(latest.input_tokens, Some(120));
    assert_eq!(latest.output_tokens, Some(30));
    assert!(latest.finished_at.is_some());
    assert_eq!(latest.execution_epoch, 1);
}

#[tokio::test]
async fn completed_run_without_usage_persists_null_not_zero() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("no accounting"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );

    // No Usage event ever arrived ⇒ unknown (NULL), never 0.
    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].3, None);
    assert_eq!(rows[0].4, None);
}

#[tokio::test]
async fn send_and_resume_each_create_their_own_run_rows() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![text_chunk("first"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![text_chunk("resumed"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    assert_eq!(agent_run_rows(&app, &conv_id).await.len(), 1);

    // Simulate a crashed turn, then resume it: a second, independent run.
    seed_dangling_tool_call(&app, &conv_id).await;
    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );

    let rows = agent_run_rows(&app, &conv_id).await;
    assert_eq!(rows.len(), 2, "send and resume are separate runs");
    assert!(rows.iter().all(|r| r.0 == "completed"));

    // The resume run's final message is the conversation's last assistant
    // row (the reply the resumed turn produced).
    let stored = stored_messages(&app, &conv_id).await;
    let last_assistant = stored
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .expect("assistant row");
    assert_eq!(
        rows[1].1.as_deref(),
        Some(last_assistant["id"].as_str().unwrap())
    );
}

#[tokio::test]
async fn bootstrap_marks_leftover_running_runs_lost() {
    // One "process" leaves a running run row behind (crash); a fresh server
    // booting on the same database file must sweep it to lost — the
    // persisted history itself stays untouched.
    let dir = tempfile::tempdir().expect("temp dir");
    let db_url = format!("sqlite://{}", dir.path().join("test.db").display());
    let pool = oct_agent::db::init_pool(&db_url)
        .await
        .expect("migrate scratch db");
    let conv_id = support::new_conversation(&pool, dir.path()).await;
    // Persisted history the "crashed" run leaves behind — the sweep must
    // not touch it.
    msg_db::insert_message(
        &pool,
        &conv_id,
        &Message::text(Role::User, "interrupted"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .expect("seed history row");

    let run_id = "00000000-0000-0000-0000-000000000001".to_string();
    sqlx::query(
        "INSERT INTO agent_runs (id, conversation_id, kind, status) VALUES (?, ?, 'root', 'running')",
    )
    .bind(&run_id)
    .bind(&conv_id)
    .execute(&pool)
    .await
    .expect("seed running run row");
    pool.close().await;

    // "Restart": TestApp's constructor runs the same runtime::bootstrap the
    // real server's run_server does.
    let app = TestApp::new_with_dir(dir).await;
    let status: (String,) = sqlx::query_as("SELECT status FROM agent_runs WHERE id = ?")
        .bind(&run_id)
        .fetch_one(&app.pool)
        .await
        .expect("seeded run row survived");
    assert_eq!(status.0, "lost");

    // The swept row is settled (carries a finish time) and the conversation's
    // history is intact — `lost` recovers run state, it does not delete data.
    let settled: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_runs WHERE id = ? AND status = 'lost' AND finished_at IS NOT NULL",
    )
    .bind(&run_id)
    .fetch_one(&app.pool)
    .await
    .expect("settled check");
    assert_eq!(settled.0, 1, "the lost row carries a finish time");

    let messages: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?")
            .bind(&conv_id)
            .fetch_one(&app.pool)
            .await
            .expect("history check");
    assert_eq!(messages.0, 1, "persisted history untouched by the sweep");
}

// --- Tool permissions --------------------------------------------------

/// Read events from an in-flight SSE stream until one of `stop`'s kind
/// arrives (inclusive); asserts the run does not end first.
async fn next_of(
    events: &mut (impl futures_util::Stream<Item = Value> + Unpin),
    r#type: &str,
) -> Value {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .expect("timed out waiting for SSE event")
            .expect("stream ended before the expected event");
        let kind = ev["type"].as_str().expect("event type");
        assert!(
            kind != "finish" && kind != "error" && kind != "cancelled",
            "run ended before a `{}` event: {}",
            r#type,
            ev
        );
        if kind == r#type {
            return ev;
        }
    }
}

/// A scripted first turn that writes `file` under `outside` (an
/// outside-workspace directory) plus the closing "done" turn.
fn outside_write_turns(outside: &std::path::Path, file: &str) -> Vec<LlmTurn> {
    vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-1",
                "write_file",
                &serde_json::json!({
                    "path": outside.join(file).display().to_string(),
                    "content": "e2e-perm",
                })
                .to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ]
}

#[tokio::test]
async fn approval_flow_allow_via_http_continues_run() {
    let app = TestApp::new().await;
    let outside = tempfile::tempdir().expect("outside dir");
    let target = outside.path().join("http-out.txt");
    let llm_server = llm::spawn_llm_mock(outside_write_turns(outside.path(), "http-out.txt")).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let mut events = sse_payloads(app.send_message(&conv_id, "write outside").await);
    let requested = next_of(&mut events, "approval_requested").await;
    let data = &requested["data"];
    assert_eq!(data["call_id"], "call-1");
    assert_eq!(data["tool"], "write_file");
    assert_eq!(data["kind"], "write");
    assert_eq!(data["outside"], true);
    // The write target does not exist yet — the expectation is the
    // canonical OUTSIDE DIR plus the (not-yet-created) file name.
    assert_eq!(
        data["path"].as_str().unwrap(),
        outside
            .path()
            .canonicalize()
            .unwrap()
            .join("http-out.txt")
            .display()
            .to_string()
    );
    assert_eq!(
        data["rule_suggestion"].as_str().unwrap(),
        format!("{}/**", outside.path().canonicalize().unwrap().display())
    );
    let approval_id = data["approval_id"].as_str().unwrap().to_string();

    // The pending list agrees while the run is parked.
    let resp = app
        .get_json(&format!("/api/conversations/{conv_id}/approvals"))
        .await;
    assert_eq!(resp.status(), 200);
    let pending: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["approval_id"], approval_id);
    assert_eq!(pending[0]["outside"], true);

    // Decide allow over HTTP: 200, resolved=true.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "allow"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["resolved"], true);

    // The run continues: resolution event → real write result → finish.
    let rest: Vec<Value> = events.collect().await;
    let types = event_types(&rest);
    let resolved_at = types
        .iter()
        .position(|t| *t == "approval_resolved")
        .expect("approval_resolved in stream");
    let result_at = types
        .iter()
        .position(|t| *t == "tool_result")
        .expect("tool_result in stream");
    assert!(
        resolved_at < result_at,
        "the resolution precedes the result"
    );
    assert_eq!(rest[resolved_at]["data"]["outcome"], "approved");
    assert_eq!(rest[resolved_at]["data"]["rule_id"], Value::Null);
    assert_eq!(rest[result_at]["data"]["is_error"], false);
    assert_eq!(*types.last().unwrap(), "finish");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "e2e-perm",
        "the approved write really landed outside the workspace"
    );
    // The pending list is empty after settlement.
    let resp = app
        .get_json(&format!("/api/conversations/{conv_id}/approvals"))
        .await;
    let pending: Vec<Value> = resp.json().await.unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn approval_allow_always_persists_rule_and_next_run_skips_asking() {
    let app = TestApp::new().await;
    let outside = tempfile::tempdir().expect("outside dir");
    let llm_server = llm::spawn_llm_mock(outside_write_turns(outside.path(), "first.txt")).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let mut events = sse_payloads(app.send_message(&conv_id, "write outside").await);
    let requested = next_of(&mut events, "approval_requested").await;
    let approval_id = requested["data"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    let suggestion = requested["data"]["rule_suggestion"]
        .as_str()
        .unwrap()
        .to_string();

    // allow_always with no edited pattern: the suggestion becomes the rule.
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "allow_always"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let rest: Vec<Value> = events.collect().await;
    assert!(event_types(&rest).contains(&"approval_resolved"));
    assert_eq!(
        std::fs::read_to_string(outside.path().join("first.txt")).unwrap(),
        "e2e-perm"
    );

    // The rule landed in the project's list with source=approval.
    let resp = app
        .get_json(&format!("/api/projects/{project_id}/permission-rules"))
        .await;
    assert_eq!(resp.status(), 200);
    let rules: Vec<Value> = resp.json().await.unwrap();
    let rule = rules
        .iter()
        .find(|r| r["source"] == "approval")
        .expect("the approval-sourced rule in the project list");
    assert_eq!(rule["effect"], "allow");
    assert_eq!(rule["tool"], "write_file");
    assert_eq!(rule["pattern"], suggestion);
    assert_eq!(rule["project_id"], project_id);
    let rule_id = rule["id"].as_str().unwrap().to_string();
    // The resolution event named the persisted rule.
    let resolved = rest
        .iter()
        .find(|e| e["type"] == "approval_resolved")
        .unwrap();
    assert_eq!(resolved["data"]["rule_id"], rule_id);

    // A SECOND run writing into the same directory never asks: the run
    // assembly loaded the persisted rule.
    llm_server.push_turns(outside_write_turns(outside.path(), "second.txt"));
    let events = app
        .read_sse(app.send_message(&conv_id, "write again").await)
        .await;
    assert!(
        !event_types(&events).contains(&"approval_requested"),
        "the persisted rule suppresses the second ask"
    );
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .expect("tool result");
    assert_eq!(result["data"]["is_error"], false);
    assert_eq!(*event_types(&events).last().unwrap(), "finish");
    assert_eq!(
        std::fs::read_to_string(outside.path().join("second.txt")).unwrap(),
        "e2e-perm"
    );
}

#[tokio::test]
async fn approval_deny_refuses_and_mode_patch_to_bypass_skips_asking() {
    let app = TestApp::new().await;
    let outside = tempfile::tempdir().expect("outside dir");
    let target = outside.path().join("denied.txt");
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-1",
                "write_file",
                &serde_json::json!({"path": target.display().to_string(), "content": "x"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        // After the refusal the model answers in text (it sees the error
        // result and the next turn streams).
        LlmTurn::Complete(vec![text_chunk("refused"), finish_chunk("stop")]),
        // The bypass round.
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-2",
                "write_file",
                &serde_json::json!({"path": outside.path().join("bypassed.txt").display().to_string(), "content": "x"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Decide deny: the refusal rides the normal tool-result path.
    let mut events = sse_payloads(app.send_message(&conv_id, "write outside").await);
    let requested = next_of(&mut events, "approval_requested").await;
    let approval_id = requested["data"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "deny"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let rest: Vec<Value> = events.collect().await;
    let resolved = rest
        .iter()
        .find(|e| e["type"] == "approval_resolved")
        .unwrap();
    assert_eq!(resolved["data"]["outcome"], "denied");
    let result = rest.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["data"]["is_error"], true);
    assert!(
        result["data"]["content"]
            .as_str()
            .unwrap()
            .contains("denied by the user")
    );
    assert_eq!(*event_types(&rest).last().unwrap(), "finish");
    assert!(!target.exists());

    // PATCH the mode to bypass (mid-conversation, between runs): the next
    // run reads it from the row and never asks.
    app.set_permission_mode(&project_id, &conv_id, "bypass")
        .await;
    let conv = get_conversation(&app, &project_id, &conv_id).await;
    assert_eq!(conv["permission_mode"], "bypass");

    let events = app
        .read_sse(app.send_message(&conv_id, "write again").await)
        .await;
    assert!(!event_types(&events).contains(&"approval_requested"));
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["data"]["is_error"], false);
    assert_eq!(*event_types(&events).last().unwrap(), "finish");
    assert!(outside.path().join("bypassed.txt").exists());
}

#[tokio::test]
async fn approval_decide_is_idempotent_and_scoped() {
    let app = TestApp::new().await;
    let outside = tempfile::tempdir().expect("outside dir");
    let llm_server = llm::spawn_llm_mock(outside_write_turns(outside.path(), "idem.txt")).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    let other_conv = app.create_conversation(&project_id).await;

    let mut events = sse_payloads(app.send_message(&conv_id, "write outside").await);
    let requested = next_of(&mut events, "approval_requested").await;
    let approval_id = requested["data"]["approval_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Unknown conversation → 404 even for a real approval id...
    let resp = app
        .post_json(
            &format!("/api/conversations/{other_conv}/approvals/{approval_id}"),
            serde_json::json!({"decision": "allow"}),
        )
        .await;
    assert_eq!(resp.status(), 404);
    // ...and unknown-conversation pending lists 404.
    let resp = app
        .get_json("/api/conversations/no-such-conv/approvals")
        .await;
    assert_eq!(resp.status(), 404);
    // An unknown approval id is the same 404...
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/apr_nope"),
            serde_json::json!({"decision": "allow"}),
        )
        .await;
    assert_eq!(resp.status(), 404);
    // ...as does an invalid decision value (400).
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "maybe"}),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // First real decide wins; the second is a 404 (already resolved).
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "allow"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/approvals/{approval_id}"),
            serde_json::json!({"decision": "deny"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    let rest: Vec<Value> = events.collect().await;
    assert_eq!(*event_types(&rest).last().unwrap(), "finish");
    // The approve went through despite the probing requests above.
    assert_eq!(
        std::fs::read_to_string(outside.path().join("idem.txt")).unwrap(),
        "e2e-perm"
    );
}

#[tokio::test]
async fn permission_deny_rule_blocks_without_asking() {
    let app = TestApp::new().await;
    let outside = tempfile::tempdir().expect("outside dir");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "hidden").unwrap();
    let pattern = format!("{}/**", outside.path().canonicalize().unwrap().display());

    // A user-created project rule denying reads of the outside directory.
    let resp = app
        .post_json(
            "/api/permission-rules",
            serde_json::json!({"effect": "deny", "tool": "read_file", "pattern": pattern}),
        )
        .await;
    assert_eq!(resp.status(), 201, "a global-scope rule is creatable too");
    let global_rule: Value = resp.json().await.unwrap();
    assert_eq!(global_rule["project_id"], Value::Null);
    assert_eq!(global_rule["source"], "user");

    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-1",
                "read_file",
                &serde_json::json!({"path": secret.display().to_string()}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("denied, moving on"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // The deny rule short-circuits: no approval, straight refusal, and the
    // persisted tool row carries the permission_denied marker.
    let events = app
        .read_sse(app.send_message(&conv_id, "read the secret").await)
        .await;
    assert!(!event_types(&events).contains(&"approval_requested"));
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["data"]["is_error"], true);
    assert!(
        result["data"]["content"]
            .as_str()
            .unwrap()
            .contains("Permission denied by rule")
    );
    assert_eq!(result["data"]["details"]["kind"], "permission_denied");
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let messages = stored_messages(&app, &conv_id).await;
    let tool_row = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("persisted tool row");
    assert_eq!(tool_row["details"]["kind"], "permission_denied");

    // Cleanup through the delete endpoint (also covered below, but the
    // global row must not leak into other suites — each TestApp has its own
    // DB, this is just exercising the delete).
    let resp = app
        .http_delete(&format!(
            "/api/permission-rules/{}",
            global_rule["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn permission_rules_crud_roundtrip() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;

    // Project-scoped create: full DTO echo.
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/permission-rules"),
            serde_json::json!({"effect": "ask", "tool": "execute_command", "pattern": "rm -rf"}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let rule: Value = resp.json().await.unwrap();
    let id = rule["id"].as_str().unwrap().to_string();
    assert_eq!(rule["effect"], "ask");
    assert_eq!(rule["tool"], "execute_command");
    assert_eq!(rule["pattern"], "rm -rf");
    assert_eq!(rule["source"], "user");
    assert_eq!(rule["project_id"], project_id);
    assert_eq!(rule["conversation_id"], Value::Null);

    // Global create lands with a null project scope.
    let resp = app
        .post_json(
            "/api/permission-rules",
            serde_json::json!({"effect": "allow", "tool": "glob", "pattern": None::<String>}),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let global: Value = resp.json().await.unwrap();
    let global_id = global["id"].as_str().unwrap().to_string();
    assert_eq!(global["project_id"], Value::Null);

    // The project list sees both (own + global); the global list sees only
    // the global row.
    let resp = app
        .get_json(&format!("/api/projects/{project_id}/permission-rules"))
        .await;
    let listed: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(listed.len(), 2);
    let resp = app.get_json("/api/permission-rules").await;
    let globals: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(globals.len(), 1);
    assert_eq!(globals[0]["id"], global_id);

    // Validation: unknown effect and empty tool are 400s, unknown project
    // a 404.
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/permission-rules"),
            serde_json::json!({"effect": "maybe", "tool": "read_file"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/permission-rules"),
            serde_json::json!({"effect": "allow", "tool": "  "}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .post_json(
            "/api/projects/no-such-project/permission-rules",
            serde_json::json!({"effect": "allow", "tool": "read_file"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // Delete: one-shot.
    let resp = app
        .http_delete(&format!("/api/permission-rules/{id}"))
        .await;
    assert_eq!(resp.status(), 204);
    let resp = app
        .http_delete(&format!("/api/permission-rules/{id}"))
        .await;
    assert_eq!(resp.status(), 404);
    let resp = app
        .get_json(&format!("/api/projects/{project_id}/permission-rules"))
        .await;
    let listed: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(listed.len(), 1, "only the global rule remains");
}

#[tokio::test]
async fn patch_permission_mode_accept_edits_and_validation() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "call-1",
                "write_file",
                &serde_json::json!({"path": "inside.txt", "content": "inner"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // Invalid mode values never reach the DB.
    let resp = app
        .patch_json(
            &format!("/api/projects/{project_id}/conversations/{conv_id}"),
            serde_json::json!({"permission_mode": "yolo"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    // An empty PATCH (no fields) is a 400; unknown conversation a 404.
    let resp = app
        .patch_json(
            &format!("/api/projects/{project_id}/conversations/{conv_id}"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let resp = app
        .patch_json(
            "/api/projects/x/conversations/no-such-conv",
            serde_json::json!({"permission_mode": "bypass"}),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // accept_edits: an INSIDE-workspace write runs without asking.
    app.set_permission_mode(&project_id, &conv_id, "accept_edits")
        .await;
    let conv = get_conversation(&app, &project_id, &conv_id).await;
    assert_eq!(conv["permission_mode"], "accept_edits");

    let events = app
        .read_sse(app.send_message(&conv_id, "write inside").await)
        .await;
    assert!(!event_types(&events).contains(&"approval_requested"));
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["data"]["is_error"], false);
    assert_eq!(*event_types(&events).last().unwrap(), "finish");
    assert_eq!(
        std::fs::read_to_string(app.dir.path().join("inside.txt")).unwrap(),
        "inner"
    );

    // The title PATCH still works alongside the new field.
    let resp = app
        .patch_json(
            &format!("/api/projects/{project_id}/conversations/{conv_id}"),
            serde_json::json!({"title": "renamed", "permission_mode": "default"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let conv = get_conversation(&app, &project_id, &conv_id).await;
    assert_eq!(conv["title"], "renamed");
    assert_eq!(conv["permission_mode"], "default");
}

// --- LLM wire log settings toggle --------------------------------------------

/// Every wire line across the (process-shared) sink dir that belongs to the
/// given conversation — the sink splits files by conversation id, but the
/// test only needs the lines, not the exact file name.
fn wire_lines_for(conversation_id: &str) -> Vec<Value> {
    let dir = oct_llm_provider::wire::testing::sink_dir().expect("wire sink installed");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read sink dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let parsed: Value = serde_json::from_str(line).expect("valid JSONL line");
            if parsed["conversation_id"].as_str() == Some(conversation_id) {
                out.push(parsed);
            }
        }
    }
    out
}

/// Bounded poll for the writer thread (a plain std thread — no event to
/// await). The handoff is fast; this only guards scheduling.
fn wait_for_wire_lines(conversation_id: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let lines = wire_lines_for(conversation_id);
        if pred(&lines) {
            return lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wire lines for {conversation_id} never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The settings switch flips the wire sink LIVE (same request, no restart)
/// and persists: with the switch off a full mock conversation produces zero
/// wire lines; turning it back on records the next conversation with the
/// full correlation envelope (run_kind=root, purpose=chat). Causality makes
/// the "zero lines while off" assertion sleep-free: the OFF run's events
/// were emitted BEFORE the ON run's, and the ON run's all arrived — so any
/// strayed OFF event would have arrived before them.
#[tokio::test]
async fn llm_wire_log_settings_toggle_controls_sink_live() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![text_chunk("while off"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![text_chunk("while on"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_off = app.create_conversation(&project_id).await;
    let conv_on = app.create_conversation(&project_id).await;

    // Default is on; PUT flips live and persists.
    let current = app
        .get_json("/api/settings")
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(current["llm_wire_log"], true);
    let updated = app
        .put_json(
            "/api/settings",
            serde_json::json!({ "llm_wire_log": false }),
        )
        .await;
    assert_eq!(updated.status(), 200);
    assert_eq!(
        updated.json::<Value>().await.unwrap()["llm_wire_log"],
        false
    );
    assert_eq!(
        app.get_json("/api/settings")
            .await
            .json::<Value>()
            .await
            .unwrap()["llm_wire_log"],
        false
    );
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'llm_wire_log'")
            .fetch_optional(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("off"));

    // A full conversation while OFF: runs to completion…
    let resp = app.send_message(&conv_off, "hi").await;
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // …back ON, another full conversation…
    app.put_json("/api/settings", serde_json::json!({ "llm_wire_log": true }))
        .await;
    let resp = app.send_message(&conv_on, "hi").await;
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    // …whose wire lines all arrive (writer alive and prompt)…
    let lines = wait_for_wire_lines(&conv_on, |ls| {
        ls.iter().any(|l| l["kind"] == "llm_stream_end")
    });

    // …which proves the OFF conversation produced nothing.
    assert!(
        wire_lines_for(&conv_off).is_empty(),
        "no wire lines may exist while the switch is off"
    );

    // The ON lines carry the full correlation envelope from the run tags.
    let request = lines
        .iter()
        .find(|l| l["kind"] == "llm_request")
        .expect("llm_request line");
    assert_eq!(request["stream"], true);
    assert_eq!(request["run_kind"], "root");
    assert_eq!(request["purpose"], "chat");
    assert!(request["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(request["provider"].as_str().is_some_and(|p| !p.is_empty()));
    assert!(
        lines
            .iter()
            .any(|l| l["kind"] == "llm_sse_event" && l["seq"].is_u64()),
        "SSE events recorded with seq"
    );
    let end = lines
        .iter()
        .find(|l| l["kind"] == "llm_stream_end")
        .unwrap();
    assert_eq!(end["outcome"], "eof");
}

// --- Reasoning levels (models.dev reasoning_options) end to end -------------

/// effort 档位经 send 请求 → agent loop → openai_compatible wire。
#[tokio::test]
async fn reasoning_effort_reaches_openai_compatible_wire() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("done"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = app
        .send_message_reasoning(
            &conv_id,
            "hi",
            "mock-llm:test-model",
            serde_json::json!({"type": "effort", "effort": "high"}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let requests = llm_server.requests();
    assert!(!requests.is_empty(), "the mock saw the chat request");
    let body = &requests[0];
    assert_eq!(body["reasoning_effort"], serde_json::json!("high"));
}

/// 档位校验：models 行 reasoning=false 或 effort 不在 values 里 → 400，
/// 不发任何请求。
#[tokio::test]
async fn reasoning_level_validated_against_models_row() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // mock-llm 行没有 models 行（custom provider 不自动建）：校验放行——
    // 这里给带 reasoning_options 的行造一条，走严格路径。
    let resp = app
        .post_json(
            "/api/providers/mock-llm/models",
            serde_json::json!({
                "model_id": "picky",
                "name": "Picky",
                "reasoning": true,
                "reasoning_options": r#"[{"type":"effort","values":["low","high"]}]"#,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);

    // 不在 values 里的档位 → 400
    let resp = app
        .send_message_reasoning(
            &conv_id,
            "hi",
            "mock-llm:picky",
            serde_json::json!({"type": "effort", "effort": "medium"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    assert!(llm_server.requests().is_empty(), "no request may leave");

    // reasoning=false 的模型拒绝任何档位
    let resp = app
        .post_json(
            "/api/providers/mock-llm/models",
            serde_json::json!({
                "model_id": "dumb",
                "name": "Dumb",
                "reasoning": false,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let resp = app
        .send_message_reasoning(
            &conv_id,
            "hi",
            "mock-llm:dumb",
            serde_json::json!({"type": "enabled"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    assert!(llm_server.requests().is_empty());

    // 即便目录声明了这些控制，OpenAI-compatible 适配器也不能静默忽略。
    let resp = app
        .post_json(
            "/api/providers/mock-llm/models",
            serde_json::json!({
                "model_id":"switchable", "name":"Switchable", "reasoning":true,
                "reasoning_options":r#"[{"type":"toggle"},{"type":"budget_tokens","min":1024}]"#,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    for reasoning in [
        serde_json::json!({"type":"enabled"}),
        serde_json::json!({"type":"budget_tokens","budget":4096}),
    ] {
        let resp = app
            .send_message_reasoning(&conv_id, "hi", "mock-llm:switchable", reasoning)
            .await;
        assert_eq!(resp.status(), 400);
    }
    assert!(stored_messages(&app, &conv_id).await.is_empty());
}

#[tokio::test]
async fn reasoning_toggle_uses_adaptive_thinking_for_current_anthropic_models() {
    for model_id in ["claude-sonnet-5", "claude-haiku-5-5"] {
        let app = TestApp::new().await;
        let mock = anthropic_llm::AnthropicMock::spawn(vec![anthropic_llm::AnthropicTurn::Text(
            "done".into(),
        )])
        .await;
        insert_provider_row(&app, "anthropic", "anthropic", &mock.url, "sk-test").await;
        let response = app
            .post_json(
                "/api/providers/anthropic/models",
                serde_json::json!({
                    "model_id":model_id, "name":model_id, "reasoning":true,
                    "reasoning_options":r#"[{"type":"toggle"},{"type":"effort","values":["low","medium","high","xhigh","max"]}]"#,
                    "limit_output":128000,
                }),
            )
            .await;
        assert_eq!(response.status(), 201);
        let project_id = app.create_project().await;
        let conv_id = app.create_conversation(&project_id).await;

        let response = app
            .send_message_reasoning(
                &conv_id,
                "hi",
                &format!("anthropic:{model_id}"),
                serde_json::json!({"type":"enabled"}),
            )
            .await;
        assert_eq!(response.status(), 200);
        let events = app.read_sse(response).await;
        assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].body["thinking"],
            serde_json::json!({"type":"adaptive", "display":"summarized"}),
            "{model_id}",
        );
    }
}

/// budget_tokens 档位 → anthropic thinking 块；思考开启时 temperature 被丢弃。
#[tokio::test]
async fn reasoning_budget_reaches_anthropic_thinking_wire() {
    let app = TestApp::new().await;
    let mock = anthropic_llm::AnthropicMock::spawn(vec![anthropic_llm::AnthropicTurn::Text(
        "thoughtful".to_string(),
    )])
    .await;
    insert_provider_row(&app, "anthropic", "anthropic", &mock.url, "sk-test").await;

    let resp = app
        .post_json(
            "/api/providers/anthropic/models",
            serde_json::json!({
                "model_id":"claude-test", "name":"Claude test", "reasoning":true,
                "reasoning_options":r#"[{"type":"budget_tokens","min":1024}]"#,
                "limit_output":32768,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);

    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    for budget in [512, 32768] {
        let response = app
            .send_message_reasoning(
                &conv_id,
                "invalid budget",
                "anthropic:claude-test",
                serde_json::json!({"type":"budget_tokens","budget":budget}),
            )
            .await;
        assert_eq!(response.status(), 400);
    }
    assert!(mock.requests().is_empty());

    let resp = app
        .send_message_reasoning(
            &conv_id,
            "hi",
            "anthropic:claude-test",
            serde_json::json!({"type": "budget_tokens", "budget": 16384}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let events = app.read_sse(resp).await;
    assert_eq!(*event_types(&events).last().unwrap(), "finish");

    let captured = mock.requests();
    assert!(!captured.is_empty());
    let body = &captured[0].body;
    assert_eq!(body["thinking"]["type"], serde_json::json!("enabled"));
    assert_eq!(body["thinking"]["budget_tokens"], serde_json::json!(16384));
    assert_eq!(body["max_tokens"], 32768);
    assert!(body.get("temperature").is_none());
}

#[tokio::test]
async fn reasoning_signed_blocks_survive_anthropic_tool_loop_and_history_reload() {
    let app = TestApp::new().await;
    let content = vec![
        serde_json::json!({"type":"thinking","thinking":"Inspect the directory", "signature":"signature-first"}),
        serde_json::json!({"type":"redacted_thinking","data":"encrypted-redaction"}),
        serde_json::json!({"type":"text","text":"I will list the files."}),
        serde_json::json!({"type":"thinking","thinking":"", "signature":"signature-omitted"}),
        serde_json::json!({"type":"tool_use","id":"call-1","name":"list_dir","input":{"path":"."}}),
    ];
    let mock = anthropic_llm::AnthropicMock::spawn(vec![
        anthropic_llm::AnthropicTurn::Blocks {
            content: content.clone(),
            stop_reason: "tool_use".into(),
        },
        anthropic_llm::AnthropicTurn::Text("done".into()),
        anthropic_llm::AnthropicTurn::Text("another turn".into()),
    ])
    .await;
    insert_provider_row(&app, "anthropic", "anthropic", &mock.url, "sk-test").await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    app.set_permission_mode(&project_id, &conv_id, "bypass")
        .await;

    let response = app
        .send_message_reasoning(
            &conv_id,
            "inspect",
            "anthropic:claude-sonnet-4-6",
            serde_json::json!({"type":"effort","effort":"low"}),
        )
        .await;
    let events = app.read_sse(response).await;
    assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
    assert_eq!(text_of(&events), "I will list the files.done");
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["output_config"]["effort"], "low");
    assert!(requests[0].body["thinking"].get("effort").is_none());
    let assistant = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], serde_json::json!(content));

    // A new run rebuilds context from SQLite; opaque blocks must survive
    // serialization too, while the presentation API shows only readable text.
    let history = stored_messages(&app, &conv_id).await;
    let presented = serde_json::to_string(&history).unwrap();
    assert!(!presented.contains("signature-first"));
    assert!(!presented.contains("encrypted-redaction"));
    assert_eq!(history[1]["parts"][0]["text"], "Inspect the directory");
    assert_eq!(history[1]["parts"][1]["text"], "[redacted]");
    let response = app
        .send_message_with(&conv_id, "again", "anthropic:claude-sonnet-4-6")
        .await;
    assert_eq!(
        app.read_sse(response).await.last().unwrap()["type"],
        "finish"
    );
    let requests = mock.requests();
    let assistant = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], serde_json::json!(content));
}

#[tokio::test]
async fn reasoning_replay_survives_instruction_prefix_changes() {
    assert_reasoning_replay_after_prompt_change(false).await;
}

#[cfg(feature = "code-mode-tests")]
#[tokio::test]
async fn reasoning_replay_survives_code_mode_new_runs() {
    assert!(
        oct_codemode::worker_path().is_some(),
        "Build oct-codemode-worker with --features runtime before enabling code-mode-tests"
    );
    assert_reasoning_replay_after_prompt_change(true).await;
}

async fn assert_reasoning_replay_after_prompt_change(code_mode: bool) {
    // Default thinking on newer models must be as resumable as an explicit
    // effort selection. Each case starts with an unchanged tool-use loop.
    for reasoning in [
        Some(serde_json::json!({"type":"effort", "effort":"high"})),
        None,
    ] {
        let app = TestApp::new().await;
        let content = vec![
            serde_json::json!({"type":"thinking", "thinking":"Inspect the workspace", "signature":"bound-signature"}),
            serde_json::json!({"type":"redacted_thinking", "data":"bound-redaction"}),
            serde_json::json!({"type":"tool_use", "id":"list-1", "name":"list_dir", "input":{"path":"."}}),
        ];
        let next_content = vec![
            serde_json::json!({"type":"thinking", "thinking":"Continue with the new prefix", "signature":"new-bound-signature"}),
            serde_json::json!({"type":"tool_use", "id":"list-2", "name":"list_dir", "input":{"path":"."}}),
        ];
        let mock = anthropic_llm::AnthropicMock::spawn_with_prefix_binding(vec![
            anthropic_llm::AnthropicTurn::Blocks {
                content: content.clone(),
                stop_reason: "tool_use".into(),
            },
            anthropic_llm::AnthropicTurn::Text("done".into()),
            anthropic_llm::AnthropicTurn::Blocks {
                content: next_content.clone(),
                stop_reason: "tool_use".into(),
            },
            anthropic_llm::AnthropicTurn::Text("another turn".into()),
        ])
        .await;
        insert_provider_row(&app, "anthropic", "anthropic", &mock.url, "sk-test").await;
        let project = app.create_project().await;
        let conversation = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conversation, "bypass")
            .await;
        if code_mode {
            assert_eq!(
                app.put_json("/api/settings", serde_json::json!({"mcp_tool_mode":"code"}))
                    .await
                    .status(),
                200
            );
        }
        let mut body = serde_json::json!({
            "content":"inspect", "provider_spec":"anthropic:claude-opus-5-5",
        });
        if let Some(reasoning) = reasoning {
            body["reasoning"] = reasoning;
        }
        let path = format!("/api/conversations/{conversation}/messages");
        let response = app.post_json(&path, &body).await;
        assert_eq!(response.status(), 200);
        let events = app.read_sse(response).await;
        assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body["system"], requests[1].body["system"]);
        assert_eq!(
            requests[1].body["messages"][1]["content"],
            serde_json::json!(content)
        );

        if !code_mode {
            std::fs::write(
                app.dir.path().join("AGENTS.md"),
                "Use the new project conventions.",
            )
            .unwrap();
            assert_eq!(
                app.post_json(
                    &format!("/api/conversations/{conversation}/instructions/reload"),
                    serde_json::json!({}),
                )
                .await
                .status(),
                200
            );
        }
        body["content"] = serde_json::json!("continue");
        let response = app.post_json(&path, &body).await;
        assert_eq!(response.status(), 200);
        let events = app.read_sse(response).await;
        assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        assert_ne!(requests[0].body["system"], requests[2].body["system"]);
        assert_eq!(requests[2].body["system"], requests[3].body["system"]);
        if code_mode {
            for request in &requests {
                assert!(request.body["system"].as_str().unwrap().contains("/sdk"));
            }
        }
        let assistant = requests[2].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["content"], serde_json::json!([content[2]]));
        let next_assistant = requests[3].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(next_assistant["content"], serde_json::json!(next_content));
        // Filtering affects only the wire: immutable persisted history
        // retains the original blocks and their durable request provenance.
        let history = msg_db::list_messages(&app.pool, &conversation)
            .await
            .unwrap();
        assert!(history[1].parts_json.contains("bound-signature"));
        assert!(history[1].parts_json.contains("bound-redaction"));
        assert!(history[1].parts_json.contains("prefix_fingerprint"));
    }
}

#[tokio::test]
async fn reasoning_replay_survives_compaction_prefix_changes() {
    let app = TestApp::new().await;
    let mock = anthropic_llm::AnthropicMock::spawn_with_prefix_binding(vec![
        anthropic_llm::AnthropicTurn::Blocks {
            content: vec![
                serde_json::json!({"type":"thinking", "thinking":"task state", "signature":"before-compaction"}),
                serde_json::json!({"type":"redacted_thinking", "data":"redacted-before-compaction"}),
                serde_json::json!({"type":"text", "text":"keep this answer"}),
            ],
            stop_reason: "end_turn".into(),
        },
        anthropic_llm::AnthropicTurn::Text("summary of the earlier history".into()),
        anthropic_llm::AnthropicTurn::Text("continued after compaction".into()),
    ])
    .await;
    insert_provider_row(&app, "anthropic", "anthropic", &mock.url, "sk-test").await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    // Enough history for a real manual compaction, with the signed answer
    // fitting in the verbatim tail rather than being summarized away.
    for index in 0..12 {
        msg_db::insert_message(
            &app.pool,
            &conversation,
            &Message::text(
                if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                "x".repeat(32_000),
            ),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let response = app
        .send_message_with(&conversation, "work", "anthropic:claude-opus-5-5")
        .await;
    let events = app.read_sse(response).await;
    assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
    let compacted = app
        .post_json(
            &format!("/api/conversations/{conversation}/compact"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(compacted.status(), 200);
    assert!(
        compacted.json::<Value>().await.unwrap()["compacted_messages"]
            .as_u64()
            .unwrap()
            >= 6
    );
    let response = app
        .send_message_with(&conversation, "continue", "anthropic:claude-opus-5-5")
        .await;
    let events = app.read_sse(response).await;
    assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let parts: Vec<_> = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .collect();
    assert!(parts.iter().any(|part| part["text"] == "keep this answer"));
    assert!(
        parts
            .iter()
            .all(|part| part["type"] != "thinking" && part["type"] != "redacted_thinking")
    );
    let history = msg_db::list_messages(&app.pool, &conversation)
        .await
        .unwrap();
    assert!(
        history
            .iter()
            .any(|message| message.parts_json.contains("before-compaction"))
    );
}

#[tokio::test]
async fn reasoning_resume_restores_real_user_choice_past_compaction_marker() {
    let app = TestApp::new().await;
    let mock = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("resumed"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&mock).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;
    msg_db::insert_message(
        &app.pool,
        &conv_id,
        &Message::text(Role::User, "continue the task"),
        Some("mock-llm"),
        Some("test-model"),
        Some(&serde_json::json!({"reasoning":{"type":"effort","effort":"high"}})),
    )
    .await
    .unwrap();
    let assistant = msg_db::insert_message(
        &app.pool,
        &conv_id,
        &Message::new(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCall {
                id: "interrupted-call".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            })],
        ),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let marker = oct_agent::agent::compact::build_marker_message("the task so far");
    let details = oct_agent::agent::compact::marker_details(
        oct_agent::agent::CompactionTrigger::Auto,
        7,
        100,
        50,
    );
    msg_db::insert_message_at_ordering(
        &app.pool,
        &conv_id,
        &marker,
        None,
        None,
        Some(&details),
        assistant.ordering,
    )
    .await
    .unwrap();

    let response = app.resume(&conv_id).await;
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    assert_eq!(events.last().unwrap()["type"], "finish", "{events:?}");
    assert_eq!(mock.requests()[0]["reasoning_effort"], "high");

    // A later real user with no choice must reset to default, not inherit
    // an older request just because its details are absent.
    msg_db::insert_message(
        &app.pool,
        &conv_id,
        &Message::text(Role::User, "new task"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    assert!(
        msg_db::get_last_user_details(&app.pool, &conv_id)
            .await
            .unwrap()
            .is_none()
    );
}
