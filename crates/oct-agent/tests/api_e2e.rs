//! Full-stack API e2e tests: a real oct-agent server on a random port with
//! its own SQLite file, driven purely over HTTP. The LLM is a mock
//! OpenAI-compatible server (see `support::llm`) behind a provider row, so
//! the entire production chain — routing, provider resolution from the DB,
//! reqwest, SSE decoding, the agent loop, real tools, persistence, and the
//! SSE event stream back out — runs as written.

mod support;

use serde_json::Value;

use support::TestApp;
use support::llm::{self, LlmTurn, finish_chunk, text_chunk, tool_call_chunk};

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
