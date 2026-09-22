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

    // http is a known-but-future transport → explicit "not yet" 400.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({"name": "remote", "transport": "http", "url": "http://localhost:1"}),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let error: Value = resp.json().await.unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("not supported yet")
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

    let resp = app
        .patch_json(
            &format!("/api/mcp/servers/{id}"),
            serde_json::json!({"transport": "http"}),
        )
        .await;
    assert_eq!(resp.status(), 400);

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
        },
    ));
    app.mcp
        .insert_connection_for_test(&fake_id, "fake", None, fake.clone(), listing);

    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

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

    // Persistence: the tool message's parts_json carries the ToolResult.
    let stored = stored_messages(&app, &conv_id).await;
    let roles: Vec<&str> = stored.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant"]);
    assert!(
        stored[2]["parts_json"]
            .as_str()
            .unwrap()
            .contains("hello from mcp")
    );

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
    assert!(
        stored[1]["parts_json"]
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

    // A second run for the same conversation is rejected while one is live.
    let conflict = app.send_message(&conv_id, "again").await;
    assert_eq!(conflict.status(), 409);

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
    let parts = tool_row["parts_json"].as_str().unwrap();
    assert!(parts.contains("call-1"));
    assert!(parts.contains("is_error\":true"));
    assert!(
        tool_row["details_json"]
            .as_str()
            .unwrap()
            .contains("执行被中断")
    );
    assert!(stored[3]["parts_json"].as_str().unwrap().contains("已恢复"));

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

    // A fully answered conversation is not resumable either.
    let resp = app.send_message(&conv_id, "hi").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        *event_types(&app.read_sse(resp).await).last().unwrap(),
        "finish"
    );
    let resp = app.resume(&conv_id).await;
    assert_eq!(resp.status(), 409);

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
    assert!(
        stored[1]["parts_json"]
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
