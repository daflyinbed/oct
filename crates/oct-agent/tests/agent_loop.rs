//! Agent loop integration tests. The LLM is replaced by a scripted model
//! (see `support`); everything else — tools, SQLite persistence, event
//! broadcast, cancellation — runs the production code paths.

mod support;

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use oct_agent::agent::AgentEvent;
use oct_agent::db;
use oct_agent::mcp::{McpManager, McpTool};
use oct_agent::tools::{self, AgentTool, ToolContext, ToolOutput};
use oct_llm_provider::core::{
    ContentPart, FinishReason, ModelError, Role, ToolCall, ToolResult, ToolResultImage,
};
use oct_mcp::McpCallResult;
use oct_mcp::McpImage;

use support::mcp::{FakeMcpConnection, fake_tool};
use support::{
    ScriptedModel, TestEnv, Turn, collect_events, finish, is_terminal, new_conversation, reasoning,
    spawn_agent, text, tool_call, usage,
};

/// Decode a stored message's parts_json back into content parts.
fn parts_of(stored: &db::messages::StoredMessage) -> Vec<ContentPart> {
    serde_json::from_str(&stored.parts_json).expect("parts_json should deserialize")
}

fn text_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// A tool that never finishes; used to exercise cancellation while tool
/// futures are in flight.
struct NeverEndingTool;

#[async_trait]
impl AgentTool for NeverEndingTool {
    fn name(&self) -> &str {
        "never_ends"
    }

    fn description(&self) -> &str {
        "never completes; used to test cancellation"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        futures_util::future::pending::<()>().await;
        unreachable!("never_ends tool must never complete")
    }
}

#[tokio::test]
async fn plain_text_turn_streams_and_persists_assistant_message() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("你好"),
        text("，世界"),
        usage(3, 5),
        finish(FinishReason::Stop),
    ])]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(text_of(&events), "你好，世界");
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    let usage_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Usage { .. }))
        .collect();
    assert_eq!(usage_events.len(), 1);

    // The loop prepends the system prompt and sends the user message as-is.
    let requests = observer.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, Role::System);
    assert_eq!(requests[0].messages[1].role, Role::User);

    // The user message is NOT persisted by the loop (the API layer owns
    // that); only the fully-assembled assistant message is.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].role, "assistant");
    assert_eq!(
        parts_of(&stored[0]),
        vec![ContentPart::Text("你好，世界".to_string())]
    );
    // Usage is persisted onto the assistant message.
    assert_eq!(stored[0].input_tokens, Some(3));
    assert_eq!(stored[0].output_tokens, Some(5));
}

#[tokio::test]
async fn reasoning_deltas_persist_and_feed_back_into_next_request() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            reasoning("想一想"),
            reasoning("，再想想"),
            tool_call("call-1", "list_dir", json!({"path": "."})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "look around",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // Reasoning deltas are forwarded to the SSE stream verbatim.
    let reasoning_of: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ReasoningDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_of, "想一想，再想想");

    // The assistant turn persists Reasoning (first) + ToolCall; the thinking
    // duration lands in details_json, never inside parts_json.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(stored[0].role, "assistant");
    assert_eq!(
        parts_of(&stored[0]),
        vec![
            ContentPart::Reasoning("想一想，再想想".to_string()),
            ContentPart::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "list_dir".to_string(),
                arguments: r#"{"path":"."}"#.to_string(),
            }),
        ]
    );
    let details: serde_json::Value = serde_json::from_str(
        stored[0]
            .details_json
            .as_deref()
            .expect("assistant details"),
    )
    .expect("details_json should deserialize");
    let ms = details["reasoning_duration_ms"]
        .as_u64()
        .expect("reasoning_duration_ms should be a number");
    assert!(
        ms < 30_000,
        "scripted stream finishes instantly, got {ms}ms"
    );

    // The persisted reasoning flows back into the next round's request.
    let second = &observer.requests()[1];
    assert!(matches!(
        &second.messages[2].parts[0],
        ContentPart::Reasoning(text) if text == "想一想，再想想"
    ));
}

#[tokio::test]
async fn tool_call_round_trip_executes_tool_and_feeds_result_back() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    tokio::fs::write(env.dir.path().join("a.txt"), "hello\nworld\n")
        .await
        .unwrap();

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "read_file", json!({"path": "a.txt"})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "read a.txt",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The call is announced before its result arrives.
    let start_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCallStart { .. }))
        .expect("ToolCallStart should be emitted");
    let result_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .expect("ToolResult should be emitted");
    assert!(start_idx < result_idx);
    match &events[start_idx] {
        AgentEvent::ToolCallStart {
            id, name, title, ..
        } => {
            assert_eq!(id, "call-1");
            assert_eq!(name, "read_file");
            assert_eq!(title, "a.txt");
        }
        other => panic!("expected ToolCallStart, got {other:?}"),
    }
    match &events[result_idx] {
        AgentEvent::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(call_id, "call-1");
            assert!(!is_error);
            // read_file formats output as numbered lines.
            assert!(content.contains("1. hello"));
            assert!(content.contains("2. world"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // The second turn receives system + user + assistant(tool call) + tool
    // result — the loop's feed-back contract.
    let requests = observer.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages.len(), 4);
    assert_eq!(second.messages[2].role, Role::Assistant);
    assert!(matches!(
        &second.messages[2].parts[0],
        ContentPart::ToolCall(tc) if tc.id == "call-1"
    ));
    assert_eq!(second.messages[3].role, Role::Tool);
    match &second.messages[3].parts[0] {
        ContentPart::ToolResult(ToolResult {
            call_id,
            content,
            is_error,
            ..
        }) => {
            assert_eq!(call_id, "call-1");
            assert_eq!(
                content,
                &json!("1. hello\n2. world"),
                "model-visible tool result must be the read_file output"
            );
            assert!(!is_error);
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }

    // Persistence: assistant(tool call) → tool result → final assistant
    // text; the tool message carries UI-only details in their own column.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[0].role, "assistant");
    assert_eq!(stored[1].role, "tool");
    assert_eq!(stored[2].role, "assistant");
    assert_eq!(
        parts_of(&stored[2]),
        vec![ContentPart::Text("done".to_string())]
    );
    assert!(stored[1].details_json.is_some());
}

#[tokio::test]
async fn tool_reads_additional_dir_file_via_absolute_path() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    // A second temp dir plays the role of the project's additional working
    // directory: reachable only through its absolute path.
    let extra = tempfile::tempdir().expect("additional temp dir");
    tokio::fs::write(extra.path().join("notes.md"), "extra\ncontext\n")
        .await
        .unwrap();
    let abs = extra.path().join("notes.md").display().to_string();

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "read_file", json!({"path": abs})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![extra.path().to_path_buf()],
        )),
        env.pool.clone(),
        &conv_id,
        "read the notes in the additional dir",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let (content, is_error) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } if call_id == "call-1" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("tool result for the additional-dir read");
    assert!(!is_error);
    assert_eq!(content, "1. extra\n2. context");
}

#[tokio::test]
async fn tool_call_without_tool_calls_finish_reason_ends_loop_without_executing() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // The model emits a tool call but finishes with Stop: the loop must NOT
    // execute the call and must end the turn.
    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        tool_call("call-1", "read_file", json!({"path": "a.txt"})),
        finish(FinishReason::Stop),
    ])]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStart { .. }))
    );
    assert_eq!(observer.requests().len(), 1);

    // The unexecuted call is still persisted as part of the assistant turn.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(matches!(
        &parts_of(&stored[0])[0],
        ContentPart::ToolCall(tc) if tc.id == "call-1"
    ));
}

#[tokio::test]
async fn unknown_tool_reports_error_back_to_model() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-9", "no_such_tool", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult {
            content, is_error, ..
        }) => {
            assert!(*is_error);
            assert!(content.contains("Unknown tool: no_such_tool"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // The error reaches the model as an error tool result.
    let second = &observer.requests()[1];
    assert!(matches!(
        &second.messages[3].parts[0],
        ContentPart::ToolResult(ToolResult { is_error: true, .. })
    ));
}

#[tokio::test]
async fn parallel_tool_calls_all_execute_and_feed_back() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    tokio::fs::write(env.dir.path().join("a.txt"), "aaa\n")
        .await
        .unwrap();
    tokio::fs::write(env.dir.path().join("b.txt"), "bbb\n")
        .await
        .unwrap();

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-a", "read_file", json!({"path": "a.txt"})),
            tool_call("call-b", "read_file", json!({"path": "b.txt"})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let results: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 2, "both parallel calls must produce results");

    // The second turn gets both tool results (completion order may vary).
    let second = &observer.requests()[1];
    let mut fed_back: Vec<&str> = second
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| match &m.parts[0] {
            ContentPart::ToolResult(ToolResult { call_id, .. }) => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    fed_back.sort_unstable();
    assert_eq!(fed_back, vec!["call-a", "call-b"]);

    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(
        stored.iter().filter(|m| m.role == "tool").count(),
        2,
        "both tool results must be persisted"
    );
}

#[tokio::test]
async fn cancel_before_loop_starts_sends_no_request() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::StreamThenHang(vec![])]);
    let observer = model.observer();

    let (handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);
    assert!(observer.requests().is_empty());
}

#[tokio::test]
async fn cancel_during_streaming_emits_cancelled_without_finish() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // The stream emits one delta, then hangs; the loop must observe the
    // cancellation token while pending on the next event.
    let model = ScriptedModel::new(vec![Turn::StreamThenHang(vec![text("part")])]);
    let observer = model.observer();

    let (handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;

    let _ = collect_events(&mut rx, |e| matches!(e, AgentEvent::TextDelta(_))).await;
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Finish)));
    assert_eq!(observer.requests().len(), 1);
}

#[tokio::test]
async fn cancel_during_tool_execution_persists_cancel_placeholders() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        tool_call("call-1", "never_ends", json!({})),
        finish(FinishReason::ToolCalls),
    ])]);
    let observer = model.observer();

    let (handle, mut rx) = spawn_agent(
        model,
        vec![Box::new(NeverEndingTool)],
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;

    // Wait until the call is dispatched, then cancel while it is running.
    let _ = collect_events(&mut rx, |e| matches!(e, AgentEvent::ToolCallStart { .. })).await;
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);
    // No ToolResult event for the unfinished call…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { call_id, .. } if call_id == "call-1"))
    );

    // …but an error placeholder tool message IS persisted so the
    // conversation history stays consistent for the next turn.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    let tool_messages: Vec<_> = stored.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tool_messages.len(), 1);
    match &parts_of(tool_messages[0])[0] {
        ContentPart::ToolResult(ToolResult {
            call_id,
            content,
            is_error,
            ..
        }) => {
            assert_eq!(call_id, "call-1");
            assert_eq!(content, &json!("用户取消了执行"));
            assert!(*is_error);
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }
    assert_eq!(observer.requests().len(), 1);
}

#[tokio::test]
async fn stream_start_failure_emits_error_event() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Fail(ModelError::transport(
        "connection refused",
    ))]);

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(
        events.last().unwrap(),
        &AgentEvent::Error("LLM error: transport error: connection refused".to_string())
    );
}

#[tokio::test]
async fn mid_stream_error_emits_error_event_and_stops() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("partial answer"),
        Err(ModelError::provider("boom")),
    ])]);

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "hi",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let last = events.last().unwrap();
    let AgentEvent::Error(msg) = last else {
        panic!("expected Error event, got {last:?}")
    };
    assert!(msg.starts_with("Stream error:"), "got: {msg}");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Finish)));
}

/// A project with additional working directories: the tools must read (by
/// absolute path) inside the additional root, mirroring how the API layer
/// builds WorkspaceRoots from the project row.
#[tokio::test]
async fn tools_read_files_inside_additional_working_dirs() {
    let env = TestEnv::new().await;
    let extra = tempfile::tempdir().expect("create additional dir");
    tokio::fs::write(extra.path().join("notes.md"), "extra context\n")
        .await
        .unwrap();

    // Persist the project with the additional dir so the row reflects what
    // the API would hand to the agent loop (normalization drops nothing here).
    let project = db::projects::create_project(
        &env.pool,
        &db::projects::CreateProjectRequest {
            name: "multi-root".to_string(),
            working_dir: env.dir.path().to_string_lossy().into_owned(),
            additional_dirs: Some(vec![extra.path().to_string_lossy().into_owned()]),
        },
    )
    .await
    .expect("create multi-root project");
    assert_eq!(
        project.additional_dirs.0,
        vec![extra.path().to_string_lossy().into_owned()]
    );

    let conv = db::conversations::create_conversation(&env.pool, &project.id, Some("test"))
        .await
        .expect("create conversation");

    let extra_file = extra.path().join("notes.md");
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "read_file", json!({ "path": extra_file })),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);

    let (_handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![extra.path().to_path_buf()],
        )),
        env.pool.clone(),
        &conv.id,
        "read the notes",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult {
            content, is_error, ..
        }) => {
            assert!(!*is_error);
            assert!(content.contains("extra context"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
}

// --- MCP tools (injected fake connections; zero real servers/binaries) -------

/// Extract the ToolResult events as (call_id, content, is_error, details).
fn tool_results(events: &[AgentEvent]) -> Vec<(String, String, bool, Option<serde_json::Value>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                call_id,
                content,
                is_error,
                details,
                ..
            } => Some((call_id.clone(), content.clone(), *is_error, details.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn mcp_tool_round_trip_translates_names_and_feeds_result_back() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // One injected server, two tools: one answering text+structured, one
    // structured-only (no text at all).
    let listing = vec![
        fake_tool("echo", Some("say things")),
        fake_tool("stats", None),
    ];
    let fake = Arc::new(
        FakeMcpConnection::new(listing.clone())
            .with_result(
                "echo",
                McpCallResult {
                    text: "hello from mcp".to_string(),
                    structured_json: Some(json!({"sum": 3})),
                    is_error: false,
                    images: Vec::new(),
                },
            )
            .with_result(
                "stats",
                McpCallResult {
                    text: String::new(),
                    structured_json: Some(json!({"rows": [], "count": 0})),
                    is_error: false,
                    images: Vec::new(),
                },
            ),
    );
    let manager = McpManager::default();
    manager.insert_connection_for_test("srv-fake", "fake", None, fake.clone(), listing);

    let mut all_tools = tools::default_tools(tools::workspace::WorkspaceRoots::new(
        env.dir.path().to_path_buf(),
        vec![],
    ));
    all_tools.extend(manager.tools_for_run(None));

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "mcp__fake__echo", json!({"a": 1})),
            tool_call("call-2", "mcp__fake__stats", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        all_tools,
        env.pool.clone(),
        &conv_id,
        "use the mcp tools",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The first request advertised both MCP tools under their exposed names
    // with the [mcp:server] description prefix.
    let advertised = &observer.requests()[0].tools;
    let names: Vec<&str> = advertised.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"mcp__fake__echo"), "advertised: {names:?}");
    assert!(names.contains(&"mcp__fake__stats"), "advertised: {names:?}");
    for spec in advertised.iter().filter(|t| t.name.starts_with("mcp__")) {
        let description = spec.description.as_deref().unwrap_or_default();
        assert!(
            description.starts_with("[mcp:fake] "),
            "description must carry the server prefix, got: {description}"
        );
    }

    // The UI title pairs server name with the ORIGINAL tool name.
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolCallStart { name, .. } if name == "mcp__fake__echo"))
    {
        Some(AgentEvent::ToolCallStart { title, .. }) => {
            assert_eq!(title, "fake / echo");
        }
        other => panic!("expected ToolCallStart for echo, got {other:?}"),
    }

    // Both results arrive as non-errors: echo's text verbatim, stats'
    // structuredContent rendered as pretty JSON (it sent no text).
    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    let echo = results
        .iter()
        .find(|(id, ..)| id == "call-1")
        .expect("echo result");
    assert_eq!(echo.1, "hello from mcp");
    assert!(!echo.2);
    assert_eq!(echo.3.as_ref().unwrap()["server"], "fake");
    assert_eq!(echo.3.as_ref().unwrap()["tool"], "echo");
    assert_eq!(echo.3.as_ref().unwrap()["structured"], json!({"sum": 3}));
    let stats = results
        .iter()
        .find(|(id, ..)| id == "call-2")
        .expect("stats result");
    assert!(!stats.2);
    assert!(
        stats.1.contains("\"count\": 0"),
        "pretty JSON expected, got: {}",
        stats.1
    );

    // Dispatch used the ORIGINAL tool names with verbatim arguments
    // (completion order between the parallel calls is not fixed).
    let mut calls = fake.calls();
    calls.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        calls,
        vec![
            ("echo".to_string(), json!({"a": 1})),
            ("stats".to_string(), json!({})),
        ]
    );

    // The second turn receives both tool results back (feed-back contract).
    let second = &observer.requests()[1];
    let tool_messages: Vec<&oct_llm_provider::core::Message> = second
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(tool_messages.len(), 2);

    // Persistence: both tool results land as tool messages carrying
    // ToolResult parts (parts_json) with UI-only details in their own column.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    let tool_rows: Vec<_> = stored.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tool_rows.len(), 2);
    for row in &tool_rows {
        let parts = parts_of(row);
        assert!(matches!(
            &parts[0],
            ContentPart::ToolResult(ToolResult {
                is_error: false,
                // Text-only results must carry no images; a vec cannot be
                // pattern-matched with Vec::new(), so bind and guard.
                images,
                ..
            }) if images.is_empty()
        ));
        assert!(
            row.details_json
                .as_deref()
                .unwrap()
                .contains("\"server\":\"fake\"")
        );
    }
    assert!(
        tool_rows
            .iter()
            .any(|r| r.parts_json.contains("hello from mcp"))
    );
}

#[tokio::test]
async fn tool_event_overflow_stops_before_compaction_or_another_model_request() {
    use oct_agent::agent::loop_runner::run_agent_loop;
    use oct_agent::agent::{AgentContext, RunStatus};

    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    for message in compact_history(4) {
        db::messages::insert_message(&env.pool, &conv_id, &message, None, None, None)
            .await
            .unwrap();
    }
    let view = db::messages::load_llm_view(&env.pool, &conv_id)
        .await
        .unwrap();

    // Eight permitted 4 MiB base64 images plus event metadata exceed the
    // 32 MiB event budget. The usage anchor would also trigger compaction
    // at the next turn top if the loop continued past the overflow.
    let listing = vec![fake_tool("shot", Some("takes screenshots"))];
    let fake = Arc::new(
        FakeMcpConnection::new(listing.clone()).with_result(
            "shot",
            McpCallResult {
                text: "screenshots captured".into(),
                images: (0..8)
                    .map(|_| McpImage {
                        mime_type: "image/png".into(),
                        data: "A".repeat(4 * 1024 * 1024),
                    })
                    .collect(),
                structured_json: None,
                is_error: false,
            },
        ),
    );
    let manager = McpManager::default();
    manager.insert_connection_for_test("srv-fake", "fake", None, fake, listing);
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            text("Capturing screenshots"),
            tool_call("call-1", "mcp__fake__shot", json!({})),
            usage(20_000, 5),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::StreamThenHang(vec![]),
    ]);
    let observer = model.observer();
    let handle = RunHandle {
        hub: Arc::new(oct_agent::agent::EventHub::new()),
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let ctx = AgentContext {
        model: Arc::new(model),
        tools: Arc::new(
            manager
                .tools_for_run(None)
                .into_iter()
                .map(Arc::from)
                .collect(),
        ),
        pool: env.pool.clone(),
        system_prompt: "test system prompt".into(),
        context_window: Some(8192),
        idle: None,
        reasoning: None,
        input_modalities: oct_agent::modality::InputModalities::Unknown,
        max_output_tokens: None,
        attachments: None,
        permission: oct_agent::permissions::gate::PermissionGate::permissive(),
        prompt_env_refresh: None,
        prompt_generation: 0,
    };

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_agent_loop(ctx, conv_id.clone(), view, handle.clone()),
    )
    .await
    .expect("overflow must settle the run");

    assert!(handle.hub.is_overflowed());
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.error.as_deref(), Some("Run event budget exceeded"));
    assert!(outcome.partial);
    assert_eq!(outcome.final_text.as_deref(), Some("Capturing screenshots"));
    assert_eq!(outcome.usage.unwrap().input_tokens, 20_000);
    assert_eq!(observer.requests().len(), 1, "no second chat request");
    assert!(
        observer.generate_requests().is_empty(),
        "no compaction request"
    );

    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    match &parts_of(stored.last().unwrap())[0] {
        ContentPart::ToolResult(result) => {
            assert_eq!(result.call_id, "call-1");
            assert_eq!(result.images.len(), 8, "overflowing result is persisted");
        }
        other => panic!("expected persisted tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_tool_image_result_flows_through_to_the_model_and_persistence() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // One tool whose result carries text plus two image content blocks.
    let listing = vec![fake_tool("shot", Some("takes a screenshot"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "shot",
        McpCallResult {
            text: "screenshot captured".to_string(),
            images: vec![
                McpImage {
                    mime_type: "image/png".to_string(),
                    data: "c2NyZWVuc2hvdDE=".to_string(),
                },
                McpImage {
                    mime_type: "image/jpeg".to_string(),
                    data: "c2NyZWVuc2hvdDI=".to_string(),
                },
            ],
            structured_json: None,
            is_error: false,
        },
    ));
    let manager = McpManager::default();
    manager.insert_connection_for_test("srv-fake", "fake", None, fake.clone(), listing);

    let mut all_tools = tools::default_tools(tools::workspace::WorkspaceRoots::new(
        env.dir.path().to_path_buf(),
        vec![],
    ));
    all_tools.extend(manager.tools_for_run(None));

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "mcp__fake__shot", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("seen"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        all_tools,
        env.pool.clone(),
        &conv_id,
        "take the screenshot",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The ToolResult event carries the images next to the text content.
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult {
            content, images, ..
        }) => {
            assert_eq!(content, "screenshot captured");
            assert_eq!(images.len(), 2);
            assert_eq!(images[0].mime_type, "image/png");
            assert_eq!(images[0].data, "c2NyZWVuc2hvdDE=");
            assert_eq!(images[1].mime_type, "image/jpeg");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // The feed-back request to the model keeps the images on the ToolResult
    // part of the tool message.
    let second = &observer.requests()[1];
    let tool_message = second
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("tool message in the second request");
    match &tool_message.parts[0] {
        ContentPart::ToolResult(ToolResult { images, .. }) => {
            assert_eq!(
                images,
                &vec![
                    ToolResultImage {
                        mime_type: "image/png".to_string(),
                        data: "c2NyZWVuc2hvdDE=".to_string(),
                    },
                    ToolResultImage {
                        mime_type: "image/jpeg".to_string(),
                        data: "c2NyZWVuc2hvdDI=".to_string(),
                    },
                ]
            );
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }

    // Persistence replay: parts_json round-trips the images, so a later
    // turn (load_messages_for_llm) still sees them.
    let replayed = db::messages::load_messages_for_llm(&env.pool, &conv_id)
        .await
        .unwrap();
    let stored_tool = replayed
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("persisted tool message");
    match &stored_tool.parts[0] {
        ContentPart::ToolResult(ToolResult { images, .. }) => {
            assert_eq!(images.len(), 2);
            assert_eq!(images[1].data, "c2NyZWVuc2hvdDI=");
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }
}

#[tokio::test]
async fn text_only_model_gates_tool_images_out_of_the_request_not_the_history() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let listing = vec![fake_tool("shot", Some("takes a screenshot"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "shot",
        McpCallResult {
            text: "screenshot captured".to_string(),
            images: vec![McpImage {
                mime_type: "image/png".to_string(),
                data: "c2NyZWVuc2hvdDE=".to_string(),
            }],
            structured_json: None,
            is_error: false,
        },
    ));
    let manager = McpManager::default();
    manager.insert_connection_for_test("srv-fake", "fake", None, fake.clone(), listing);

    let mut all_tools = tools::default_tools(tools::workspace::WorkspaceRoots::new(
        env.dir.path().to_path_buf(),
        vec![],
    ));
    all_tools.extend(manager.tools_for_run(None));

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "mcp__fake__shot", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![
            text("seen"),
            usage(500, 50),
            finish(FinishReason::Stop),
        ]),
    ]);
    let observer = model.observer();

    let view = db::messages::LlmView {
        messages: vec![oct_llm_provider::core::Message::text(
            Role::User,
            "take the screenshot",
        )],
        orderings: vec![1],
        anchor: None,
    };
    let (_handle, mut rx) = spawn_agent_with_gate(
        model,
        all_tools,
        env.pool.clone(),
        &conv_id,
        view,
        None,
        None,
        oct_agent::permissions::gate::PermissionGate::permissive(),
        oct_agent::modality::InputModalities::Known(vec![oct_agent::modality::Modality::Text]),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The live event and the persisted row keep the image: gating is a
    // request-time view, storage and UI must not lose data.
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult { images, .. }) => assert_eq!(images.len(), 1),
        other => panic!("expected ToolResult, got {other:?}"),
    }
    let replayed = db::messages::load_messages_for_llm(&env.pool, &conv_id)
        .await
        .unwrap();
    let stored_tool = replayed
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("persisted tool message");
    match &stored_tool.parts[0] {
        ContentPart::ToolResult(ToolResult { images, .. }) => assert_eq!(images.len(), 1),
        other => panic!("expected ToolResult part, got {other:?}"),
    }

    // The feed-back request to the model replaced the image with a note.
    let second = &observer.requests()[1];
    let tool_message = second
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("tool message in the second request");
    match &tool_message.parts[0] {
        ContentPart::ToolResult(ToolResult {
            content, images, ..
        }) => {
            assert!(images.is_empty(), "image must not reach a text-only model");
            assert_eq!(
                content,
                &serde_json::json!(
                    "screenshot captured\n[1 image attachment omitted: this model does not accept image input]"
                )
            );
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }

    let reloaded = db::messages::load_llm_view(&env.pool, &conv_id)
        .await
        .unwrap();
    assert_eq!(
        reloaded.anchor.unwrap().input_modalities,
        oct_agent::modality::InputModalities::Known(vec![oct_agent::modality::Modality::Text])
    );
}

#[tokio::test]
async fn mcp_tool_error_result_maps_to_error_tool_output_and_loop_continues() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let listing = vec![fake_tool("boom", Some("always fails"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "boom",
        McpCallResult {
            text: "exploded".to_string(),
            structured_json: None,
            is_error: true,
            images: Vec::new(),
        },
    ));
    let manager = McpManager::default();
    manager.insert_connection_for_test("srv-fake", "fake", None, fake.clone(), listing);

    let mut all_tools = tools::default_tools(tools::workspace::WorkspaceRoots::new(
        env.dir.path().to_path_buf(),
        vec![],
    ));
    all_tools.extend(manager.tools_for_run(None));

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "mcp__fake__boom", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("recovered"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        all_tools,
        env.pool.clone(),
        &conv_id,
        "break something",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The tool-level isError surfaces as an error ToolResult with the
    // server's message verbatim.
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "call-1");
    assert_eq!(results[0].1, "exploded");
    assert!(results[0].2);

    // The error reaches the model as an error tool result, and the run
    // still finishes normally afterwards.
    let second = &observer.requests()[1];
    assert!(matches!(
        &second.messages[3].parts[0],
        ContentPart::ToolResult(ToolResult { is_error: true, .. })
    ));
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);

    // The persisted tool message keeps the error flag.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    let tool_row = stored.iter().find(|m| m.role == "tool").unwrap();
    assert!(tool_row.parts_json.contains("is_error\":true"));
}

#[tokio::test]
async fn mcp_tool_timeout_returns_error_and_loop_continues() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // The tool is built directly (not through the manager) so the test can
    // bake in a 50ms budget; the fake's call never resolves.
    let slow_info = fake_tool("slow", None);
    let fake = Arc::new(FakeMcpConnection::new(vec![slow_info.clone()]).with_hang("slow"));
    let tool = McpTool::new(
        "mcp__fake__slow".to_string(),
        slow_info,
        "fake".to_string(),
        fake,
        Duration::from_millis(50),
    );

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-1", "mcp__fake__slow", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (_handle, mut rx) = spawn_agent(
        model,
        vec![Box::new(tool)],
        env.pool.clone(),
        &conv_id,
        "call the slow one",
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].2, "a timed-out call must be an error result");
    let content = &results[0].1;
    assert!(content.contains("timed out"), "got: {content}");
    assert!(content.contains("`slow`"), "must name the tool: {content}");
    assert!(
        content.contains("`fake`"),
        "must name the server: {content}"
    );

    // The timeout error is fed back to the model and the run finishes.
    let second = &observer.requests()[1];
    assert!(matches!(
        &second.messages[3].parts[0],
        ContentPart::ToolResult(ToolResult { is_error: true, .. })
    ));
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
}

/// A cancelled in-flight tool lands as exactly one tool message: either the
/// loop's "用户取消了执行" placeholder (future dropped unfinished) or the
/// tool's own cancelled output (select raced the loop's break) — never both,
/// never none. Returns the (content, is_error) of that message.
async fn single_cancelled_tool_result(
    pool: &sqlx::SqlitePool,
    conv_id: &str,
    call_id: &str,
) -> (serde_json::Value, bool) {
    let stored = db::messages::list_messages(pool, conv_id).await.unwrap();
    let tool_messages: Vec<_> = stored.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(
        tool_messages.len(),
        1,
        "exactly one tool message must cover the cancelled call, got {}",
        tool_messages.len()
    );
    match &parts_of(tool_messages[0])[0] {
        ContentPart::ToolResult(ToolResult {
            call_id: id,
            content,
            is_error,
            ..
        }) => {
            assert_eq!(id, call_id);
            assert!(
                *is_error,
                "a cancelled call must persist as an error result: {content}"
            );
            (content.clone(), *is_error)
        }
        other => panic!("expected ToolResult part, got {other:?}"),
    }
}

// --- Context compaction -------------------------------------------------------

use oct_agent::agent::compact;
use oct_agent::db::messages as msg_db;
use oct_llm_provider::core::Message;

/// Alternating user/assistant exchanges plus a trailing user message — the
/// shape a send produces. Each message carries enough padding that a tiny
/// test window (500 tokens → threshold 425) is clearly exceeded.
fn compact_history(rounds: usize) -> Vec<Message> {
    let pad = "x".repeat(200);
    let mut msgs = Vec::new();
    for i in 0..rounds {
        msgs.push(Message::text(Role::User, format!("old-user-{i} {pad}")));
        msgs.push(Message::text(
            Role::Assistant,
            format!("old-assistant-{i} {pad}"),
        ));
    }
    msgs.push(Message::text(Role::User, "the current request"));
    msgs
}

#[tokio::test]
async fn summary_request_fits_unicode_history_and_rejects_impossible_windows() {
    let model = ScriptedModel::new(Vec::new());
    let observer = model.observer();
    let region: Vec<_> = (0..10)
        .map(|_| Message::text(Role::User, "汉😀".repeat(4_000)))
        .collect();
    assert!(
        compact::generate_summary(&model, &region, Some(128))
            .await
            .is_err()
    );
    assert!(
        observer.generate_requests().is_empty(),
        "do not send an impossible request"
    );

    for window in [500, 8_000] {
        model.push_generation(Ok("summary".to_string()));
        compact::generate_summary(&model, &region, Some(window))
            .await
            .unwrap();
        let request = observer.generate_requests().pop().unwrap();
        let output = request.options.max_output_tokens.unwrap();
        assert!(
            compact::estimate_messages(&request.messages) + u64::from(output) <= u64::from(window),
            "the complete request and output must fit window {window}"
        );
        assert!(output <= window / 4);
    }
}

#[tokio::test]
async fn auto_compaction_replaces_history_persists_marker_and_streams_events() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("done"),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Ok("SUMMARY: fixed the login bug".to_string()));
    let observer = model.observer();

    let view = db::messages::LlmView {
        messages: compact_history(4), // 9 messages
        orderings: (1..=9).collect(),
        anchor: None,
    };
    let (_handle, mut rx) = support::spawn_agent_with(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(500),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // Event order: compacting < compacted < the turn's text.
    let idx = |name: &str| {
        events
            .iter()
            .position(|e| event_name(e) == name)
            .unwrap_or_else(|| panic!("missing event {name} in {events:?}"))
    };
    assert_eq!(event_name(&events[idx("compacting")]), "compacting");
    assert!(idx("compacting") < idx("compacted"));
    assert!(idx("compacted") < idx("text_delta"));
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);

    let AgentEvent::Compacted {
        triggered_by,
        compacted_messages,
        summary,
        summary_message_id,
        context_window_tokens,
        ..
    } = &events[idx("compacted")]
    else {
        unreachable!()
    };
    assert_eq!(*triggered_by, oct_agent::agent::CompactionTrigger::Auto);
    assert_eq!(*context_window_tokens, Some(500));
    assert_eq!(
        *compacted_messages, 8,
        "summarizes everything but the final user message (the assistant \
         round alone exceeds the 50-token tail budget of a 500-token window)"
    );
    assert!(summary.contains("login bug"));
    assert!(!summary_message_id.is_empty());

    // The summary call received the compacted region as serialized text.
    let summary_req = &observer.generate_requests()[0];
    let serialized = serde_json::to_string(&summary_req.messages).unwrap();
    assert!(
        serialized.contains("old-user-0"),
        "region passed to summary"
    );

    // The next (and only) stream request carries [system, marker, tail]:
    // the old history is gone, the tail is verbatim.
    let req = &observer.requests()[0];
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[0].role, Role::System);
    assert_eq!(req.messages[1].role, Role::User);
    let ContentPart::Text(marker_text) = &req.messages[1].parts[0] else {
        panic!("marker is a text message");
    };
    assert!(marker_text.contains("[Context compaction]"));
    assert!(marker_text.contains("SUMMARY: fixed the login bug"));
    assert!(marker_text.contains("<summary>"));
    assert_eq!(req.messages[2].role, Role::User);
    let tail_serialized = serde_json::to_string(&req.messages[2..]).unwrap();
    assert!(tail_serialized.contains("the current request"));
    let full = serde_json::to_string(&req.messages).unwrap();
    assert!(!full.contains("old-user-0"), "compacted region excluded");

    // The marker row is persisted with UI metadata in details_json.
    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    let marker_row = stored
        .iter()
        .find(|m| {
            m.details_json
                .as_deref()
                .is_some_and(|d| d.contains("\"kind\":\"compaction\""))
        })
        .expect("marker row persisted");
    assert_eq!(marker_row.role, "user");
    assert!(marker_row.parts_json.contains("login bug"));
    assert_eq!(marker_row.id, *summary_message_id);
}

#[tokio::test]
async fn image_attachment_budget_triggers_compaction_before_the_first_request() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    for _ in 0..3 {
        for role in [Role::User, Role::Assistant] {
            db::messages::insert_message(
                &env.pool,
                &conv,
                &Message::text(role, "x".repeat(2000)),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
    }
    let images = Message {
        role: Role::User,
        parts: (0..8)
            .map(|_| ContentPart::ImageUrl {
                url: oct_agent::attachments::attachment_url(&uuid::Uuid::new_v4().to_string()),
            })
            .collect(),
    };
    db::messages::insert_message(&env.pool, &conv, &images, None, None, None)
        .await
        .unwrap();
    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("done"),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Ok("Earlier discussion summarized".into()));
    let observer = model.observer();
    let (_handle, mut events) = support::spawn_agent_with(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv,
        db::messages::load_llm_view(&env.pool, &conv).await.unwrap(),
        Some(12_000),
    )
    .await;
    let events = collect_events(&mut events, is_terminal).await;
    let compacted = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Compacted { .. }))
        .expect("image budget must trigger compaction");
    let output = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TextDelta(_)))
        .unwrap();
    assert!(compacted < output);
    assert_eq!(observer.generate_requests().len(), 1);
    assert_eq!(
        observer.requests()[0].messages.last().unwrap().parts,
        images.parts
    );
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
}

/// Map an event to its serde tag for ordering assertions.
fn event_name(e: &AgentEvent) -> &'static str {
    match e {
        AgentEvent::Compacting { .. } => "compacting",
        AgentEvent::Compacted { .. } => "compacted",
        AgentEvent::Microcompacted { .. } => "microcompacted",
        AgentEvent::TextDelta(_) => "text_delta",
        AgentEvent::Finish => "finish",
        _ => "other",
    }
}

#[tokio::test]
async fn cancel_while_mcp_tool_in_flight_ends_run_and_keeps_connection_alive() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // A hanging MCP call under a 60s budget: without cancel propagation the
    // run would sit out the whole budget after cancellation.
    let slow_info = fake_tool("slow", None);
    let fake = Arc::new(FakeMcpConnection::new(vec![slow_info.clone()]).with_hang("slow"));
    let tool = McpTool::new(
        "mcp__fake__slow".to_string(),
        slow_info,
        "fake".to_string(),
        fake.clone(),
        Duration::from_secs(60),
    );

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        tool_call("call-1", "mcp__fake__slow", json!({})),
        finish(FinishReason::ToolCalls),
    ])]);
    let observer = model.observer();

    let (handle, mut rx) = spawn_agent(
        model,
        vec![Box::new(tool)],
        env.pool.clone(),
        &conv_id,
        "call the slow one",
    )
    .await;

    // Cancel once the call is dispatched (ToolCallStart is published before
    // the future is first polled; the token makes this a sync handoff).
    let _ = collect_events(&mut rx, |e| matches!(e, AgentEvent::ToolCallStart { .. })).await;
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);
    assert_eq!(
        observer.requests().len(),
        1,
        "no second LLM turn after cancel"
    );

    // History stays consistent: the dangling call is covered exactly once.
    let (content, _) = single_cancelled_tool_result(&env.pool, &conv_id, "call-1").await;
    let placeholder = content == json!("用户取消了执行");
    let cancelled_output = content
        .as_str()
        .is_some_and(|text| text.contains("cancelled"));
    assert!(
        placeholder || cancelled_output,
        "expected the loop placeholder or the tool's cancelled output, got: {content}"
    );

    // The shared connection is NEVER shut down by cancelling one run: it is
    // keyed by server id and must survive for the next conversation.
    assert_eq!(fake.shutdown_count(), 0);
}

#[tokio::test]
async fn cancel_while_execute_runs_kills_child_and_ends_run_promptly() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        tool_call(
            "call-1",
            "execute_command",
            json!({ "command": "sleep 30", "timeout_secs": 60 }),
        ),
        finish(FinishReason::ToolCalls),
    ])]);
    let observer = model.observer();

    let started = Instant::now();
    let (handle, mut rx) = spawn_agent(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        "run something long",
    )
    .await;

    let _ = collect_events(&mut rx, |e| matches!(e, AgentEvent::ToolCallStart { .. })).await;
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;

    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);
    // The child must be killed through the cancel path, not waited out: the
    // budget is 60s and the command sleeps 30s, so a run that honoured either
    // would blow past this bound (collect_events' own 30s timeout is the
    // hard backstop).
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "cancelled execute must end promptly, took {:?}",
        started.elapsed()
    );
    assert_eq!(observer.requests().len(), 1);

    let (content, _) = single_cancelled_tool_result(&env.pool, &conv_id, "call-1").await;
    let placeholder = content == json!("用户取消了执行");
    let cancelled_output = content
        .as_str()
        .is_some_and(|text| text.contains("Command cancelled by the user"));
    assert!(
        placeholder || cancelled_output,
        "expected the loop placeholder or the tool's cancelled output, got: {content}"
    );
}

#[tokio::test]
async fn auto_compaction_failure_does_not_break_the_run() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("still works"),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Err(ModelError::provider("summary exploded")));
    let observer = model.observer();

    let view = db::messages::LlmView {
        messages: compact_history(4),
        orderings: (1..=9).collect(),
        anchor: None,
    };
    let (_handle, mut rx) = support::spawn_agent_with(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(500),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // Compacting was announced, compaction never landed, the turn continued
    // with the UN-compacted history and finished normally.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacting { .. })),
        "compacting announced: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "no compacted event on failure"
    );
    assert!(text_of(&events).contains("still works"));
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);

    let req = &observer.requests()[0];
    let serialized = serde_json::to_string(&req.messages).unwrap();
    assert!(serialized.contains("old-user-0"), "history untouched");

    let stored = db::messages::list_messages(&env.pool, &conv_id)
        .await
        .unwrap();
    assert!(
        !stored.iter().any(|m| {
            m.details_json
                .as_deref()
                .is_some_and(|d| d.contains("compaction"))
        }),
        "no marker row on failure"
    );
}

#[tokio::test]
async fn load_llm_view_starts_at_last_marker_drops_orphans_and_anchors() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    // Pre-marker history: invisible to the LLM from now on.
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "old-0"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::Assistant, "old-1"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // The marker row.
    let marker = compact::build_marker_message("the summary");
    let details = compact::marker_details(oct_agent::agent::CompactionTrigger::Auto, 2, 100, 20);
    msg_db::insert_message(&env.pool, &conv_id, &marker, None, None, Some(&details))
        .await
        .unwrap();

    // An orphan tool result AFTER the marker (e.g. a repair placeholder for
    // a pre-marker call): must be filtered out of the view.
    let orphan = Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: "ghost".to_string(),
            content: json!("result of a call that was compacted away"),
            is_error: false,
            images: Vec::new(),
        })],
    };
    msg_db::insert_message(&env.pool, &conv_id, &orphan, None, None, None)
        .await
        .unwrap();

    // Post-marker live messages, the assistant carrying usage (the anchor).
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "q"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let assistant = msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::Assistant, "a"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::update_message_usage(&env.pool, &assistant.id, Some(100), Some(50), None)
        .await
        .unwrap();

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();

    // [marker, q, a] — old rows excluded, orphan dropped.
    assert_eq!(view.messages.len(), 3);
    assert_eq!(view.messages[0].role, Role::User);
    let ContentPart::Text(t) = &view.messages[0].parts[0] else {
        panic!()
    };
    assert!(t.contains("the summary"));
    assert_eq!(view.messages[1].role, Role::User);
    assert_eq!(view.messages[2].role, Role::Assistant);

    // Anchor: the assistant row's input+output, at its view index.
    let anchor = view.anchor.expect("anchor present");
    assert_eq!(anchor.tokens, 150);
    assert_eq!(anchor.message_index, 2);

    // Pre-marker dangling calls are not repairable anymore.
    assert!(
        msg_db::find_dangling_tool_calls(&env.pool, &conv_id)
            .await
            .unwrap()
            .is_empty()
    );
}

// --- Membership split: compaction persistence, COW anchor semantics ------

use oct_agent::db::messages::CowSwap;

const CLEARED_PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Seed usage provenance explicitly: these tests exercise measured v2
/// anchors, while unversioned legacy anchors are intentionally discarded.
async fn measured_projection_details(pool: &sqlx::SqlitePool, conv_id: &str) -> serde_json::Value {
    let context = oct_agent::context::ContextState::new(
        "test system prompt".into(),
        msg_db::load_llm_view(pool, conv_id).await.unwrap(),
    );
    let assembler = oct_agent::attachments::RequestAssembler::new(
        None,
        oct_agent::context::projection::MediaPolicy::default(),
    );
    let projection = assembler.plan(&context).await.unwrap().snapshot;
    json!({ "projection": projection, "input_modalities": oct_agent::modality::InputModalities::Unknown, "prompt_generation": 0 })
}

/// Alternating user/assistant rounds plus a trailing user message, seeded
/// INTO the DB (unlike `compact_history`, which only builds an in-memory
/// view). The last assistant carries usage so it becomes the provider
/// anchor.
async fn seed_db_history(pool: &sqlx::SqlitePool, conv_id: &str, rounds: usize) {
    for i in 0..rounds {
        msg_db::insert_message(
            pool,
            conv_id,
            &Message::text(Role::User, format!("old-user-{i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let details = measured_projection_details(pool, conv_id).await;
        let assistant = msg_db::insert_message(
            pool,
            conv_id,
            &Message::text(Role::Assistant, format!("old-assistant-{i}")),
            None,
            None,
            Some(&details),
        )
        .await
        .unwrap();
        if i == rounds - 1 {
            msg_db::update_message_usage(pool, &assistant.id, Some(400), Some(30), None)
                .await
                .unwrap();
        }
    }
    msg_db::insert_message(
        pool,
        conv_id,
        &Message::text(Role::User, "the current request"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
}

/// Regression (P1-3 + P1-2): after an auto compaction the verbatim tail must
/// survive in the DB AFTER the marker (so the next `load_llm_view` keeps it),
/// and the pre-compaction assistant usage in the tail must NOT be picked up
/// as the anchor — its input tokens contain the compacted-away prefix.
#[tokio::test]
async fn auto_compaction_persists_tail_and_reloads_without_old_anchor() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    seed_db_history(&env.pool, &conv_id, 8).await; // 17 messages

    // No usage in the stream: the run's assistant stays usage-less, so the
    // reloaded view must have NO anchor at all (local estimation takes over,
    // mirroring the in-run anchor=None reset right after compaction).
    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("done"),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Ok("SUMMARY: compacted".to_string()));

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    // 430-token anchor + the trailing user message pushes past the 425
    // threshold of a 500-token window.
    assert!(view.anchor.is_some(), "seeded anchor present before run");
    let (_handle, mut rx) = support::spawn_agent_with(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(500),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "compaction ran: {events:?}"
    );

    let reloaded = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    // The view starts at the marker and KEEPS the tail verbatim.
    let ContentPart::Text(marker_text) = &reloaded.messages[0].parts[0] else {
        panic!("view starts at the marker");
    };
    assert!(marker_text.contains("[Context compaction]"));
    let joined = reloaded
        .messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect::<String>();
    assert!(joined.contains("old-assistant-7"), "tail assistant kept");
    assert!(joined.contains("the current request"), "tail user kept");
    assert!(!joined.contains("old-user-0"), "region excluded");
    // The tail assistant carries usage but was appended BEFORE the marker.
    assert_eq!(reloaded.anchor, None, "no pre-compaction anchor");

    // The stored timeline is contiguous: marker sits between the region and
    // the shifted-up tail (two-phase move leaves no holes or negatives).
    let stored = msg_db::list_messages(&env.pool, &conv_id).await.unwrap();
    let orderings: Vec<i64> = stored.iter().map(|m| m.ordering).collect();
    assert_eq!(
        orderings,
        (1..=stored.len() as i64).collect::<Vec<_>>(),
        "contiguous orderings"
    );
    let marker_row = stored
        .iter()
        .find(|m| {
            m.details_json
                .as_deref()
                .is_some_and(|d| d.contains("\"kind\":\"compaction\""))
        })
        .expect("marker row");
    let tail_rows_before: Vec<&db::messages::StoredMessage> = stored
        .iter()
        .filter(|m| m.ordering > marker_row.ordering)
        .collect();
    assert!(
        tail_rows_before.len() >= 6,
        "tail rows persisted after the marker"
    );
}

/// Companion: once a NEW usage lands after the marker, the reloaded anchor is
/// that fresh assistant — the revision rule must not exclude it.
#[tokio::test]
async fn post_compaction_usage_becomes_the_reloaded_anchor() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    seed_db_history(&env.pool, &conv_id, 8).await;

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("done"),
        usage(300, 60),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Ok("SUMMARY: compacted".to_string()));

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let (_handle, mut rx) = support::spawn_agent_with(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(500),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. }))
    );

    let reloaded = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let anchor = reloaded.anchor.expect("fresh usage anchors the view");
    assert_eq!(anchor.tokens, 360);
    assert_eq!(
        anchor.message_index,
        reloaded.messages.len() - 1,
        "anchor is the post-marker assistant"
    );
}

/// A cleared tool-result message: placeholder content, same call_id, images
/// emptied.
fn cleared_tool_result(call_id: &str) -> Message {
    Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: call_id.to_string(),
            content: json!(CLEARED_PLACEHOLDER),
            is_error: false,
            images: Vec::new(),
        })],
    }
}

/// COW swap that happened AFTER the anchor's request: the anchor's provider
/// tokens must be adjusted by the recomputed savings; re-applying is a no-op.
#[tokio::test]
async fn cow_swap_after_anchor_deducts_tokens_across_reload() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "q"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            })],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                content: json!("x".repeat(800)),
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
    let anchor = msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::Assistant, "a2"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::update_message_usage(&env.pool, &anchor.id, Some(500), Some(50), None)
        .await
        .unwrap();

    let before = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    assert_eq!(before.anchor.as_ref().unwrap().tokens, 550);

    // Swap AFTER the anchor exists (its revision is later than the anchor's
    // append revision) — the anchor's 550 tokens counted the full content.
    let swapped = msg_db::apply_cow_swaps(
        &env.pool,
        &conv_id,
        &[CowSwap {
            call_id: "c1".to_string(),
            copy: cleared_tool_result("c1"),
            details: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!(swapped.len(), 1);

    let after = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    // The view shows the placeholder.
    let ContentPart::ToolResult(tr) = &after.messages[2].parts[0] else {
        panic!("tool row");
    };
    assert_eq!(tr.content, json!(CLEARED_PLACEHOLDER));

    let original_msg = Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            content: json!("x".repeat(800)),
            is_error: false,
            images: Vec::new(),
        })],
    };
    let savings = compact::estimate_message(&original_msg)
        - compact::estimate_message(&cleared_tool_result("c1"));
    assert_eq!(
        after.anchor.as_ref().unwrap().tokens,
        550 - savings,
        "anchor adjusted by recomputed savings"
    );

    // Idempotent: the original no longer holds the slot.
    let again = msg_db::apply_cow_swaps(
        &env.pool,
        &conv_id,
        &[CowSwap {
            call_id: "c1".to_string(),
            copy: cleared_tool_result("c1"),
            details: None,
        }],
    )
    .await
    .unwrap();
    assert!(again.is_empty(), "re-swap skipped");
    let stable = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    assert_eq!(
        stable.anchor.as_ref().unwrap().tokens,
        after.anchor.as_ref().unwrap().tokens
    );
}

#[tokio::test]
async fn cow_image_savings_use_the_anchor_gate_in_memory_and_after_reload() {
    use oct_agent::context::{ContextState, estimate_message_with_modalities};
    use oct_agent::modality::{InputModalities, Modality, gate_request_messages};

    for modalities in [
        InputModalities::Known(vec![Modality::Text]),
        InputModalities::Known(vec![Modality::Text, Modality::Image]),
        InputModalities::Unknown,
    ] {
        let env = TestEnv::new().await;
        let conv = new_conversation(&env.pool, env.dir.path()).await;
        let original = Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "shot".into(),
                content: json!("screenshot"),
                is_error: false,
                images: vec![
                    ToolResultImage {
                        mime_type: "image/png".into(),
                        data: "AAAA".into()
                    };
                    4
                ],
            })],
        };
        let history = [
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: "shot".into(),
                    name: "screenshot".into(),
                    arguments: "{}".into(),
                })],
            },
            original.clone(),
        ];
        for message in &history {
            msg_db::insert_message(&env.pool, &conv, message, None, None, None)
                .await
                .unwrap();
        }
        // Missing metadata covers legacy anchors that saw every image.
        let details = (modalities != InputModalities::Unknown)
            .then(|| json!({"input_modalities": modalities}));
        let anchor = msg_db::insert_message(
            &env.pool,
            &conv,
            &Message::text(Role::Assistant, "done"),
            None,
            None,
            details.as_ref(),
        )
        .await
        .unwrap();
        let tokens = if modalities == InputModalities::Known(vec![Modality::Text]) {
            3000u64
        } else {
            8000
        };
        msg_db::update_message_usage(&env.pool, &anchor.id, Some(tokens as u32), Some(0), None)
            .await
            .unwrap();
        let before = msg_db::load_llm_view(&env.pool, &conv).await.unwrap();
        assert_eq!(before.anchor.as_ref().unwrap().input_modalities, modalities);
        let mut context =
            ContextState::new(String::new(), before).with_input_modalities(modalities.clone());
        let copy = micro::cleared_copy(&original);
        let projected = gate_request_messages(vec![original.clone()], &modalities);
        let savings = compact::estimate_message(&projected[0])
            .saturating_sub(compact::estimate_message(&copy));
        assert_eq!(
            savings,
            estimate_message_with_modalities(&original, &modalities)
                .saturating_sub(estimate_message_with_modalities(&copy, &modalities))
        );
        // Deliberately pass raw savings: the anchor must recompute using its
        // own snapshot rather than trust a caller's different projection.
        let raw_savings =
            compact::estimate_message(&original).saturating_sub(compact::estimate_message(&copy));
        context.apply_clears(&[(2, raw_savings)], micro::PLACEHOLDER_CONTENT);
        msg_db::apply_cow_swaps(
            &env.pool,
            &conv,
            &[CowSwap {
                call_id: "shot".into(),
                copy,
                details: None,
            }],
        )
        .await
        .unwrap();
        let reloaded = msg_db::load_llm_view(&env.pool, &conv).await.unwrap();
        assert_eq!(reloaded.anchor.as_ref().unwrap().tokens, tokens - savings);
        assert_eq!(context.anchor(), reloaded.anchor.as_ref());
        assert!(
            reloaded.anchor.unwrap().tokens > 2900,
            "gated images must not zero the anchor"
        );
    }
}

/// COW swap that happened BEFORE the anchor's request: the anchor's usage
/// already reflects the placeholder — no deduction.
#[tokio::test]
async fn cow_swap_before_anchor_is_not_deducted() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "q"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            })],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                content: json!("x".repeat(800)),
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

    // Swap FIRST, then the anchor assistant is appended (its request saw the
    // placeholder already).
    msg_db::apply_cow_swaps(
        &env.pool,
        &conv_id,
        &[CowSwap {
            call_id: "c1".to_string(),
            copy: cleared_tool_result("c1"),
            details: None,
        }],
    )
    .await
    .unwrap();
    let anchor = msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::Assistant, "a2"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    msg_db::update_message_usage(&env.pool, &anchor.id, Some(500), Some(50), None)
        .await
        .unwrap();

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    assert_eq!(
        view.anchor.as_ref().unwrap().tokens,
        550,
        "no deduction for a swap older than the anchor"
    );
}

/// The two-phase tail shift must keep orderings contiguous and unique under
/// repeated mid-timeline inserts (and never leave parked negative values).
#[tokio::test]
async fn insert_at_ordering_shifts_tail_without_unique_collision() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    for i in 0..5 {
        msg_db::insert_message(
            &env.pool,
            &conv_id,
            &Message::text(Role::User, format!("m{i}")),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let marker = msg_db::insert_message_at_ordering(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "marker"),
        None,
        None,
        None,
        3,
    )
    .await
    .unwrap();
    assert_eq!(marker.ordering, 3);

    let stored = msg_db::list_messages(&env.pool, &conv_id).await.unwrap();
    assert_eq!(stored.len(), 6);
    assert_eq!(
        stored.iter().map(|m| m.ordering).collect::<Vec<_>>(),
        (1..=6).collect::<Vec<_>>(),
        "contiguous after first insert"
    );
    assert_eq!(stored[2].parts_json, marker.parts_json, "marker at slot 3");
    assert!(stored[3].parts_json.contains("m2"), "m2 shifted up");

    let again = msg_db::insert_message_at_ordering(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "marker2"),
        None,
        None,
        None,
        2,
    )
    .await
    .unwrap();
    assert_eq!(again.ordering, 2);

    let stored = msg_db::list_messages(&env.pool, &conv_id).await.unwrap();
    assert_eq!(stored.len(), 7);
    assert_eq!(
        stored.iter().map(|m| m.ordering).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>(),
        "contiguous after second insert"
    );
    assert!(
        stored.iter().all(|m| m.ordering > 0),
        "no parked negatives remain"
    );
    // Revisions are unique per mutation and ordered by MUTATION time, not by
    // timeline position — the markers were appended last (highest revisions)
    // yet sit at middle slots. That decoupling is the whole point of the
    // revision counter.
    let mut revisions: Vec<i64> = stored.iter().map(|m| m.appended_at_revision).collect();
    revisions.sort_unstable();
    assert_eq!(revisions, (1..=7).collect::<Vec<_>>());
    let mut marker_revisions: Vec<i64> = stored
        .iter()
        .filter(|m| m.parts_json.contains("marker"))
        .map(|m| m.appended_at_revision)
        .collect();
    marker_revisions.sort_unstable();
    assert_eq!(marker_revisions, vec![6, 7], "markers mutated last");
}

/// A pool with the PRE-split schema (every older migration replayed by raw
/// SQL, no sqlx bookkeeping) for testing the split migration's SQL directly.
/// The migrator wiring itself is exercised by every TestEnv (fresh chain).
async fn legacy_split_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("temp dir");
    let url = format!("sqlite://{}", dir.path().join("legacy.db").display());
    let options = <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(&url)
        .expect("url")
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("legacy pool");

    for up in [
        include_str!("../../../migrations/20260417182650_init_schema.up.sql"),
        include_str!("../../../migrations/20260916100000_add_details_json_to_messages.up.sql"),
        include_str!("../../../migrations/20260921120000_add_conversation_title_source.up.sql"),
        include_str!("../../../migrations/20260922090000_add_project_additional_dirs.up.sql"),
        include_str!("../../../migrations/20260922100000_add_mcp_servers.up.sql"),
        include_str!("../../../migrations/20260922110000_add_project_scope_to_mcp_servers.up.sql"),
        include_str!("../../../migrations/20260923110000_add_mcp_server_tokens.up.sql"),
    ] {
        sqlx::raw_sql(up).execute(&pool).await.expect("legacy up");
    }
    (dir, pool)
}

/// Migration fidelity: a legacy-shaped database (old single-table layout,
/// append-only) must migrate into the split with `appended_at_revision`
/// derived from the old ordering, keeping a post-marker assistant eligible
/// as the usage anchor.
#[tokio::test]
async fn membership_migration_backfill_preserves_anchor_eligibility() {
    let (_dir, pool) = legacy_split_pool().await;

    sqlx::query("INSERT INTO projects (id, name, working_dir, created_at, updated_at) VALUES ('p1', 'legacy', '/tmp', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)")
        .execute(&pool)
        .await
        .expect("legacy project");
    sqlx::query("INSERT INTO conversations (id, project_id, title, created_at, updated_at) VALUES ('c1', 'p1', 'legacy', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)")
        .execute(&pool)
        .await
        .expect("legacy conversation");
    let parts = |text: &str| serde_json::to_string(&Message::text(Role::User, text).parts).unwrap();
    // (ordering 1) user, (2) marker, (3) assistant with usage.
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, details_json, ordering, created_at) VALUES ('m1', 'c1', 'user', $1, NULL, 1, CURRENT_TIMESTAMP)")
        .bind(parts("legacy q"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, details_json, ordering, created_at) VALUES ('m2', 'c1', 'user', $1, $2, 2, CURRENT_TIMESTAMP)")
        .bind(parts("[Context compaction] legacy summary"))
        .bind(r#"{"kind":"compaction"}"#)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, details_json, ordering, provider_id, model_id, input_tokens, output_tokens, created_at) VALUES ('m3', 'c1', 'assistant', $1, NULL, 3, 'p', 'm', 100, 20, CURRENT_TIMESTAMP)")
        .bind(serde_json::to_string(&Message::text(Role::Assistant, "a").parts).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    sqlx::raw_sql(include_str!(
        "../../../migrations/20260923140000_split_message_membership.up.sql"
    ))
    .execute(&pool)
    .await
    .expect("split migration");

    // The post-marker assistant stays eligible: old ordering made its
    // appended_at_revision (3) later than the marker's (2).
    let view = msg_db::load_llm_view(&pool, "c1").await.expect("view");
    assert_eq!(view.messages.len(), 2, "view starts at the marker");
    let anchor = view.anchor.expect("legacy anchor preserved");
    assert_eq!(anchor.tokens, 120);
    assert_eq!(anchor.message_index, 1);

    let stored = msg_db::list_messages(&pool, "c1").await.unwrap();
    assert_eq!(
        stored.iter().map(|m| m.ordering).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let revision: i64 =
        sqlx::query_scalar("SELECT context_revision FROM conversations WHERE id = 'c1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(revision, 3, "counter continues past the old max ordering");
}

// --- Microcompact -----------------------------------------------------------

use oct_agent::agent::micro;

/// [user, (assistant+call, big result) × n, user] seeded into the DB. Result
/// contents are distinct (`p{i}` + pad) so cleared/kept boundaries are
/// assertable in wire requests.
async fn seed_tool_history(pool: &sqlx::SqlitePool, conv_id: &str, rounds: usize, pad: usize) {
    msg_db::insert_message(
        pool,
        conv_id,
        &Message::text(Role::User, "go"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    for i in 0..rounds {
        msg_db::insert_message(
            pool,
            conv_id,
            &Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
        msg_db::insert_message(
            pool,
            conv_id,
            &Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: format!("c{i}"),
                    content: json!(format!("p{i} ") + &"x".repeat(pad)),
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
        pool,
        conv_id,
        &Message::text(Role::User, "next"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn microcompact_waterline_clears_old_results_and_keeps_recent() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    seed_tool_history(&env.pool, &conv_id, 8, 4_000).await;

    // est ≈ 8200 tokens: over the 70% waterline of a 10k window (7000) but
    // under the summary threshold (8500) even BEFORE clearing — isolating
    // micro from summary compaction entirely.
    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("ok"),
        finish(FinishReason::Stop),
    ])]);
    let observer = model.observer();

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let (_handle, mut rx) = support::spawn_agent_full(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(10_000),
        None,
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let micro_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Microcompacted { .. }))
        .collect();
    assert_eq!(micro_events.len(), 1, "one microcompact at the turn top");
    let AgentEvent::Microcompacted {
        triggered_by,
        replacements,
        est_tokens_saved,
        ..
    } = micro_events[0]
    else {
        unreachable!()
    };
    assert_eq!(
        *triggered_by,
        oct_agent::agent::MicrocompactTrigger::Waterline
    );
    // 8 results − trailing round (none, ends on user) − 5 protected = 3.
    assert_eq!(replacements.len(), 3);
    let cleared: Vec<&str> = replacements.iter().map(|r| r.call_id.as_str()).collect();
    assert_eq!(cleared, vec!["c0", "c1", "c2"]);
    assert!(*est_tokens_saved >= micro::MIN_SAVINGS_TOKENS);

    // No summary compaction happened alongside.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "waterline micro alone; no summary call"
    );

    // The wire request carries placeholders for the cleared three and the
    // verbatim payloads for the protected five.
    let req = &observer.requests()[0];
    let serialized = serde_json::to_string(&req.messages).unwrap();
    assert_eq!(
        serialized.matches(micro::PLACEHOLDER_CONTENT).count(),
        3,
        "cleared results are placeholders on the wire"
    );
    for i in 0..3 {
        assert!(!serialized.contains(&format!("p{i} ")), "p{i} cleared");
    }
    for i in 3..8 {
        assert!(serialized.contains(&format!("p{i} ")), "p{i} protected");
    }

    // Persisted as COW: the reloaded view shows placeholders, the originals
    // stay reachable via supersedes.
    let reloaded = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let reloaded_serialized = serde_json::to_string(&reloaded.messages).unwrap();
    assert_eq!(
        reloaded_serialized
            .matches(micro::PLACEHOLDER_CONTENT)
            .count(),
        3,
        "reloaded view matches the in-run rewrite"
    );
    let stored = msg_db::list_messages(&env.pool, &conv_id).await.unwrap();
    let swapped_rows: Vec<&db::messages::StoredMessage> = stored
        .iter()
        .filter(|m| m.swapped_at_revision.is_some())
        .collect();
    assert_eq!(swapped_rows.len(), 3);
    assert!(
        swapped_rows
            .iter()
            .all(|m| m.supersedes_message_id.is_some()
                && m.parts_json.contains(micro::PLACEHOLDER_CONTENT))
    );
}

/// A failed summary call disables AUTO compaction only — the waterline
/// microcompact must stay armed on later tool-round turn tops (regression:
/// the waterline check used to sit inside the `!auto_compact_disabled`
/// guard, so one summary failure silently took micro down with it).
#[tokio::test]
async fn waterline_micro_still_fires_after_summary_failure() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    // 8 seeded rounds × ~2k-token results: over the 85% summary threshold
    // of a 10k window even after the turn-1 micro clear.
    seed_tool_history(&env.pool, &conv_id, 8, 8_000).await;
    // Six files the first turn reads in parallel: their results give the
    // turn-3 top enough clearable candidates past the protected window.
    for i in 0..6 {
        tokio::fs::write(
            env.dir.path().join(format!("big{i}.txt")),
            std::iter::repeat_n('y', 10_000).collect::<String>(),
        )
        .await
        .unwrap();
    }

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("call-a", "read_file", json!({"path": "big0.txt"})),
            tool_call("call-b", "read_file", json!({"path": "big1.txt"})),
            tool_call("call-c", "read_file", json!({"path": "big2.txt"})),
            tool_call("call-d", "read_file", json!({"path": "big3.txt"})),
            tool_call("call-e", "read_file", json!({"path": "big4.txt"})),
            tool_call("call-f", "read_file", json!({"path": "big5.txt"})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![
            tool_call("call-x", "read_file", json!({"path": "big0.txt"})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    // The turn-1-top summary compaction fails.
    model.push_generation(Err(ModelError::provider("summary exploded")));
    let observer = model.observer();

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let (_handle, mut rx) = support::spawn_agent_full(
        model,
        tools::default_tools(tools::workspace::WorkspaceRoots::new(
            env.dir.path().to_path_buf(),
            vec![],
        )),
        env.pool.clone(),
        &conv_id,
        view,
        Some(10_000),
        None,
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // The run survived the failed compaction and finished normally.
    assert!(text_of(&events).contains("done"));
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);

    // The summary was attempted exactly once (turn 1 top) and never landed;
    // auto compaction is disabled for the rest of the run.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacting { .. })),
        "compacting announced"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "no compacted event on failure"
    );
    assert_eq!(observer.generate_requests().len(), 1);

    // TWO waterline microcompacts: turn 1 top (before the failed summary)
    // AND turn 3 top (after it) — micro is independent of the auto switch.
    let micro_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Microcompacted { .. }))
        .collect();
    assert_eq!(micro_events.len(), 2, "events: {events:?}");
    for event in &micro_events {
        let AgentEvent::Microcompacted { triggered_by, .. } = event else {
            unreachable!()
        };
        assert_eq!(
            *triggered_by,
            oct_agent::agent::MicrocompactTrigger::Waterline
        );
    }
    // The turn-3 clear includes results that only exist because the run kept
    // going after the summary failure.
    let AgentEvent::Microcompacted { replacements, .. } = micro_events[1] else {
        unreachable!()
    };
    assert!(
        replacements.iter().any(|r| r.call_id == "call-a"),
        "turn-3 micro cleared a run-produced result: {replacements:?}"
    );
}

#[tokio::test]
async fn microcompact_idle_entry_fires_without_a_window() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    seed_tool_history(&env.pool, &conv_id, 8, 4_000).await;

    // No context window at all (waterline entry disabled) — the idle entry
    // alone must still clear once.
    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("ok"),
        finish(FinishReason::Stop),
    ])]);
    let observer = model.observer();

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let (_handle, mut rx) = support::spawn_agent_full(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        None,
        Some(Duration::from_secs(2 * 3_600)),
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    let AgentEvent::Microcompacted {
        triggered_by,
        replacements,
        ..
    } = events
        .iter()
        .find(|e| matches!(e, AgentEvent::Microcompacted { .. }))
        .expect("idle microcompact fired")
    else {
        unreachable!()
    };
    assert_eq!(*triggered_by, oct_agent::agent::MicrocompactTrigger::Idle);
    assert_eq!(replacements.len(), 3);

    let serialized = serde_json::to_string(&observer.requests()[0].messages).unwrap();
    assert_eq!(serialized.matches(micro::PLACEHOLDER_CONTENT).count(), 3);
}

/// Same-turn composition: the waterline microcompact fires AND the estimate
/// still sits over the 85% summary threshold afterwards — the marker insert
/// then shifts the (already swapped) tail slots. The riskiest combination.
#[tokio::test]
async fn microcompact_waterline_then_summary_compacts_same_turn() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    msg_db::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "go"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    for i in 0..8 {
        msg_db::insert_message(
            &env.pool,
            &conv_id,
            &Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // The three OLDEST results are small-but-worth-it (≥ MIN_SAVINGS in
        // total); the five protected ones are huge — post-micro est stays
        // over the summary threshold, so compaction fires in the same turn.
        let pad = if i < 3 { 1_600 } else { 8_000 };
        msg_db::insert_message(
            &env.pool,
            &conv_id,
            &Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: format!("c{i}"),
                    content: json!(format!("p{i} ") + &"x".repeat(pad)),
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
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "next"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("done"),
        finish(FinishReason::Stop),
    ])]);
    model.push_generation(Ok("SUMMARY: same turn".to_string()));

    let view = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    let (_handle, mut rx) = support::spawn_agent_full(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        view,
        Some(11_000),
        None,
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;

    // Event order: micro first, then the summary compaction, then the turn.
    let pos = |name: &str| {
        events
            .iter()
            .position(|e| event_name(e) == name)
            .unwrap_or_else(|| panic!("missing event {name} in {events:?}"))
    };
    assert!(pos("microcompacted") < pos("compacting"));
    assert!(pos("compacting") < pos("compacted"));
    assert!(pos("compacted") < pos("text_delta"));

    // The wire request is [system, marker, tail user] — the region (with its
    // cleared placeholders) is summarized away.
    let AgentEvent::Microcompacted { replacements, .. } = &events[pos("microcompacted")] else {
        unreachable!()
    };
    assert_eq!(replacements.len(), 3);

    // DB: timeline contiguous; the swapped slots kept their orderings (they
    // sit BEFORE the cut, untouched by the marker's tail shift) and their
    // revision stamps.
    let stored = msg_db::list_messages(&env.pool, &conv_id).await.unwrap();
    assert_eq!(
        stored.iter().map(|m| m.ordering).collect::<Vec<_>>(),
        (1..=stored.len() as i64).collect::<Vec<_>>(),
        "contiguous after micro+compact in one turn"
    );
    let swapped: Vec<&db::messages::StoredMessage> = stored
        .iter()
        .filter(|m| m.swapped_at_revision.is_some())
        .collect();
    assert_eq!(swapped.len(), 3);
    assert!(
        swapped
            .iter()
            .all(|m| m.parts_json.contains(micro::PLACEHOLDER_CONTENT))
    );
    assert!(
        swapped.iter().all(|m| m.ordering <= 9),
        "region slots unshifted"
    );
}

/// The in-run anchor deduction must equal what `load_llm_view` recomputes on
/// reload — one batch with targets on BOTH sides of the anchor.
#[tokio::test]
async fn microcompact_anchor_mixed_deduction_matches_reload() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;

    let tool_msg = |call_id: &str| Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: call_id.to_string(),
            content: json!(format!("payload-{call_id} ") + &"x".repeat(4_000)),
            is_error: false,
            images: Vec::new(),
        })],
    };
    let call_msg = |call_id: &str| Message {
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCall {
            id: call_id.to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        })],
    };

    let mut seed: Vec<Message> = vec![Message::text(Role::User, "go")];
    seed.push(call_msg("t0"));
    seed.push(tool_msg("t0"));
    seed.push(call_msg("t1"));
    seed.push(tool_msg("t1"));
    seed.push(Message::text(Role::Assistant, "anchor assistant"));
    seed.push(call_msg("t2"));
    seed.push(tool_msg("t2"));
    for i in 3..8 {
        seed.push(call_msg(&format!("t{i}")));
        seed.push(tool_msg(&format!("t{i}")));
    }
    seed.push(Message::text(Role::User, "next"));

    for msg in &seed {
        let details = measured_projection_details(&env.pool, &conv_id).await;
        let stored = msg_db::insert_message(&env.pool, &conv_id, msg, None, None, Some(&details))
            .await
            .unwrap();
        // The plain-text anchor assistant (index 5 in the view) gets usage.
        if let ContentPart::Text(t) = msg.parts.first().unwrap()
            && t.contains("anchor assistant")
        {
            msg_db::update_message_usage(&env.pool, &stored.id, Some(500), Some(50), None)
                .await
                .unwrap();
        }
    }

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        text("ok"),
        finish(FinishReason::Stop),
    ])]);
    let (_handle, mut rx) = support::spawn_agent_full(
        model,
        Vec::new(),
        env.pool.clone(),
        &conv_id,
        msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap(),
        // est ≈ 550 + ~6.1k of post-anchor bulk: over the 70% waterline of an
        // 8k window (5600) but under the summary threshold (6800).
        Some(8_000),
        None,
    )
    .await;
    let events = collect_events(&mut rx, is_terminal).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "summary compaction stays out of the picture"
    );
    let AgentEvent::Microcompacted {
        est_context_tokens, ..
    } = events
        .iter()
        .find(|e| matches!(e, AgentEvent::Microcompacted { .. }))
        .expect("microcompact fired")
    else {
        unreachable!()
    };

    // Expected: full_messages with t0/t1/t2 cleared and the anchor adjusted
    // by exactly the BEFORE-anchor savings (t0, t1) — t2 sits after the
    // anchor, its savings only shrink the local increment.
    let mut expected = vec![Message::text(Role::System, "test system prompt")];
    for (i, msg) in seed.iter().enumerate() {
        let mut m = msg.clone();
        if matches!(m.role, Role::Tool) {
            for part in &mut m.parts {
                if let ContentPart::ToolResult(tr) = part
                    && ["t0", "t1", "t2"].contains(&tr.call_id.as_str())
                {
                    tr.content = json!(micro::PLACEHOLDER_CONTENT);
                    tr.images.clear();
                }
            }
        }
        let _ = i;
        expected.push(m);
    }
    let s = |id: &str| {
        let copy = micro::cleared_copy(&tool_msg(id));
        compact::estimate_message(&tool_msg(id)).saturating_sub(compact::estimate_message(&copy))
    };
    let expected_anchor = (
        550u64.saturating_sub(s("t0")).saturating_sub(s("t1")),
        6usize,
    );
    let expected_est = compact::estimate_context(&expected, &[], Some(expected_anchor));
    assert_eq!(*est_context_tokens, expected_est, "in-run 口径 == expected");

    // Reload: the DB-side recomputation agrees on the anchor tokens.
    let reloaded = msg_db::load_llm_view(&env.pool, &conv_id).await.unwrap();
    assert_eq!(
        reloaded.anchor.as_ref().unwrap().tokens,
        expected_anchor.0,
        "reload deduction == in-run deduction"
    );
}

/// Conversation deletion sweeps the orphaned content rows (membership is
/// gone; originals kept alive only by the deleted conversation's copies go
/// too).
#[tokio::test]
async fn deleting_conversation_sweeps_orphaned_content_rows() {
    let env = TestEnv::new().await;
    let conv_a = new_conversation(&env.pool, env.dir.path()).await;
    let conv_b = new_conversation(&env.pool, env.dir.path()).await;

    for conv in [&conv_a, &conv_b] {
        msg_db::insert_message(
            &env.pool,
            conv,
            &Message::text(Role::User, "q"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        msg_db::insert_message(
            &env.pool,
            conv,
            &Message {
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
        msg_db::insert_message(
            &env.pool,
            conv,
            &Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: "c1".to_string(),
                    content: json!("x".repeat(800)),
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

    // Swap conv_a's result: conv_a now owns an extra copy row + its original
    // loses the (single) membership reference.
    msg_db::apply_cow_swaps(
        &env.pool,
        &conv_a,
        &[CowSwap {
            call_id: "c1".to_string(),
            copy: cleared_tool_result("c1"),
            details: None,
        }],
    )
    .await
    .unwrap();

    let count = |pool: &sqlx::SqlitePool, conv: &str| {
        let pool = pool.clone();
        let conv = conv.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?",
            )
            .bind(&conv)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let content_count = |pool: &sqlx::SqlitePool| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(&env.pool, &conv_a).await, 3);
    // 2×3 originals + 1 copy.
    assert_eq!(content_count(&env.pool).await, 7);

    oct_agent::db::conversations::delete_conversation(&env.pool, &conv_a)
        .await
        .unwrap();

    // Only conv_b's three rows survive: the copy, its original AND the other
    // originals of conv_a are all swept.
    assert_eq!(content_count(&env.pool).await, 3);
    assert_eq!(count(&env.pool, &conv_b).await, 3, "conv_b untouched");
}

/// The down migration is lossless EVEN AFTER microcompact swaps: slots
/// pointing at cleared copies are repointed at their originals before the
/// fold, so the legacy table carries the original content forward.
#[tokio::test]
async fn split_migration_down_restores_cow_and_folds_losslessly() {
    let (_dir, pool) = legacy_split_pool().await;

    sqlx::query("INSERT INTO projects (id, name, working_dir, created_at, updated_at) VALUES ('p1', 'legacy', '/tmp', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, project_id, title, created_at, updated_at) VALUES ('c1', 'p1', 'legacy', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)")
        .execute(&pool)
        .await
        .unwrap();
    let parts = |text: &str, role: &str| {
        serde_json::to_string(
            &Message::text(
                if role == "tool" {
                    Role::Tool
                } else if role == "assistant" {
                    Role::Assistant
                } else {
                    Role::User
                },
                text,
            )
            .parts,
        )
        .unwrap()
    };
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, ordering, created_at) VALUES ('m1', 'c1', 'user', $1, 1, CURRENT_TIMESTAMP)")
        .bind(parts("q", "user"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, ordering, created_at) VALUES ('m2', 'c1', 'assistant', $1, 2, CURRENT_TIMESTAMP)")
        .bind(serde_json::to_string(&Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "c9".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            })],
        }
        .parts)
        .unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, parts_json, ordering, created_at) VALUES ('m3', 'c1', 'tool', $1, 3, CURRENT_TIMESTAMP)")
        .bind(serde_json::to_string(&Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "c9".to_string(),
                content: json!("original big output"),
                is_error: false,
                images: Vec::new(),
            })],
        }
        .parts)
        .unwrap())
        .execute(&pool)
        .await
        .unwrap();

    sqlx::raw_sql(include_str!(
        "../../../migrations/20260923140000_split_message_membership.up.sql"
    ))
    .execute(&pool)
    .await
    .expect("split up");

    // Swap the tool result, then reverse the whole migration.
    msg_db::apply_cow_swaps(
        &pool,
        "c1",
        &[CowSwap {
            call_id: "c9".to_string(),
            copy: cleared_tool_result("c9"),
            details: None,
        }],
    )
    .await
    .unwrap();
    let after_swap = msg_db::list_messages(&pool, "c1").await.unwrap();
    assert!(
        after_swap[2]
            .parts_json
            .contains(micro::PLACEHOLDER_CONTENT)
    );

    sqlx::raw_sql(include_str!(
        "../../../migrations/20260923140000_split_message_membership.down.sql"
    ))
    .execute(&pool)
    .await
    .expect("split down");

    // The legacy shape is back and carries the ORIGINAL content in the tool
    // slot — the swap was restored, not folded into the placeholder.
    let rows = sqlx::query_as::<_, (String, String, String, i64, String)>(
        "SELECT id, role, parts_json, ordering, conversation_id FROM messages ORDER BY ordering",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3, "no rows lost");
    let tool_row = rows.iter().find(|r| r.1 == "tool").unwrap();
    assert_eq!(tool_row.0, "m3", "original row restored into the slot");
    assert!(tool_row.2.contains("original big output"));
    assert!(!tool_row.2.contains(micro::PLACEHOLDER_CONTENT));
    assert_eq!(tool_row.3, 3);
    assert_eq!(tool_row.4, "c1");
}

// --- Permission gate ------------------------------------------------

use futures_util::StreamExt as _;
use oct_agent::agent::RunHandle;
use oct_agent::agent::events::SubscribeStream;
use oct_agent::permissions::evaluator::PermissionMode;
use oct_agent::permissions::gate::{PermissionGate, publish_cancelled_approvals};
use oct_agent::permissions::registry::{ApprovalDecision, ApprovalRegistry};
use oct_agent::permissions::rules::{Effect, Rule, RuleSet};
use oct_agent::tools::workspace::WorkspaceRoots;
use support::spawn_agent_with_gate;

/// One raw event with the shared bounded-timeout contract (same budget as
/// `collect_events`).
async fn next_rx_event(rx: &mut SubscribeStream) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(30), rx.next())
        .await
        .expect("timed out waiting for agent event (loop hung?)")
        .expect("agent event stream ended prematurely")
}

/// Read events until one `approval_requested` lands; returns it (the other
/// events are dropped — later assertions read the hub snapshot instead).
async fn wait_for_approval(rx: &mut SubscribeStream) -> (String, AgentEvent) {
    loop {
        let event = next_rx_event(rx).await;
        if let AgentEvent::ApprovalRequested { approval_id, .. } = &event {
            return (approval_id.clone(), event);
        }
        assert!(
            !is_terminal(&event),
            "run ended before asking for approval: {event:?}"
        );
    }
}

/// Spawn the real loop with a caller-built permission gate: the permission
/// suites park scripted turns on the gate's approval flow and answer
/// through the very registry the gate holds. The workspace is the test
/// env's temp dir; "outside" targets are absolute paths in a second temp
/// dir.
async fn spawn_gated(
    model: ScriptedModel,
    pool: sqlx::SqlitePool,
    env: &TestEnv,
    conv_id: &str,
    user_message: &str,
    gate: Arc<PermissionGate>,
) -> (RunHandle, SubscribeStream) {
    let view = db::messages::LlmView {
        messages: vec![oct_llm_provider::core::Message::text(
            Role::User,
            user_message,
        )],
        orderings: vec![1],
        anchor: None,
    };
    spawn_agent_with_gate(
        model,
        tools::default_tools(WorkspaceRoots::new(env.dir.path().to_path_buf(), vec![])),
        pool,
        conv_id,
        view,
        None,
        None,
        gate,
        oct_agent::modality::InputModalities::Unknown,
    )
    .await
}

/// Build a gate over the env's workspace with the given mode/rule/subagent
/// shape and hand back the shared registry so the test can play the decide
/// endpoint. The exec root points somewhere nothing ever writes, keeping
/// the built-in deny out of these suites' way (it has its own tests).
fn build_gate(
    env: &TestEnv,
    conv_id: &str,
    mode: PermissionMode,
    _subagent: bool,
    rules: impl FnOnce(&mut RuleSet),
) -> (Arc<PermissionGate>, Arc<ApprovalRegistry>) {
    let registry = Arc::new(ApprovalRegistry::new());
    let roots = WorkspaceRoots::new(env.dir.path().to_path_buf(), vec![]);
    let mut set = RuleSet::new(env.dir.path().join("no-exec-root"));
    rules(&mut set);
    let gate = PermissionGate::new(
        conv_id.to_string(),
        "run-perm".to_string(),
        roots,
        set,
        mode,
        None,
        registry.clone(),
    );
    (gate, registry)
}

/// Outside-workspace write arguments for `path` (the temp dir exists, so
/// the write resolver anchors the target under it).
fn outside_write_args(dir: &std::path::Path, file: &str) -> serde_json::Value {
    json!({"path": dir.join(file).display().to_string(), "content": "perm-content"})
}

#[tokio::test]
async fn permission_ask_allow_executes_outside_write() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");
    let canonical = outside.path().canonicalize().unwrap();
    let target = canonical.join("out.txt");

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "write_file",
                outside_write_args(outside.path(), "out.txt"),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, false, |_| {});
    let (handle, mut rx) = spawn_gated(
        model,
        env.pool.clone(),
        &env,
        &conv_id,
        "write outside",
        gate,
    )
    .await;

    let (approval_id, event) = wait_for_approval(&mut rx).await;
    match event {
        AgentEvent::ApprovalRequested {
            call_id,
            tool,
            kind,
            path,
            outside: outside_flag,
            rule_suggestion,
            ..
        } => {
            assert_eq!(call_id, "call-1");
            assert_eq!(tool, "write_file");
            assert_eq!(kind, "write");
            assert_eq!(path.as_deref(), Some(target.display().to_string().as_str()));
            assert_eq!(outside_flag, Some(true), "the resolved target is outside");
            assert_eq!(
                rule_suggestion.as_deref(),
                Some(format!("{}/**", canonical.display()).as_str())
            );
        }
        other => panic!("expected ApprovalRequested, got {other:?}"),
    }
    let _ = approval_id;

    // Answer allow (the decide endpoint's plain-allow path): the tool
    // really executes and the run settles normally.
    assert!(
        registry
            .resolve(&approval_id, ApprovalDecision::Allow)
            .is_ok()
    );
    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    let result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                call_id, is_error, ..
            } if call_id == "call-1" => Some(*is_error),
            _ => None,
        })
        .expect("write result");
    assert!(!result, "an allowed write must succeed");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "perm-content",
        "the file really landed outside the workspace"
    );

    // The refusal-free result flows back to the model like any tool result.
    let second = &observer.requests()[1];
    assert_eq!(second.messages.len(), 4);
    assert_eq!(second.messages[3].role, Role::Tool);
    drop(handle);
}

#[tokio::test]
async fn permission_ask_deny_returns_error_result_visible_to_model() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "write_file",
                outside_write_args(outside.path(), "denied.txt"),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("understood"), finish(FinishReason::Stop)]),
    ]);
    let observer = model.observer();

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, false, |_| {});
    let (_handle, mut rx) = spawn_gated(
        model,
        env.pool.clone(),
        &env,
        &conv_id,
        "write outside",
        gate,
    )
    .await;

    let (approval_id, _) = wait_for_approval(&mut rx).await;
    assert!(
        registry
            .resolve(&approval_id, ApprovalDecision::Deny)
            .is_ok()
    );

    // The denial rides the NORMAL result path: an is_error ToolResult
    // event, a persisted tool row and a tool message in the next request.
    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult {
            call_id,
            content,
            is_error,
            details,
            ..
        }) => {
            assert_eq!(call_id, "call-1");
            assert!(*is_error);
            assert!(content.contains("denied by the user"), "got: {content}");
            assert_eq!(
                details
                    .as_ref()
                    .and_then(|d| d.get("kind"))
                    .and_then(|k| k.as_str()),
                Some("permission_denied")
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    let second = &observer.requests()[1];
    match &second.messages[3].parts[0] {
        ContentPart::ToolResult(ToolResult {
            content, is_error, ..
        }) => {
            assert!(*is_error);
            assert!(
                content
                    .as_str()
                    .is_some_and(|c| c.contains("denied by the user")),
                "the model sees the refusal: {content}"
            );
        }
        other => panic!("expected a ToolResult part, got {other:?}"),
    }
    // Nothing was written.
    assert!(!outside.path().join("denied.txt").exists());
}

#[tokio::test]
async fn permission_pending_cancel_resolves_cancelled() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");

    let model = ScriptedModel::new(vec![Turn::Stream(vec![
        tool_call(
            "call-1",
            "write_file",
            outside_write_args(outside.path(), "cancelled.txt"),
        ),
        finish(FinishReason::ToolCalls),
    ])]);

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, false, |_| {});
    let (handle, mut rx) = spawn_gated(
        model,
        env.pool.clone(),
        &env,
        &conv_id,
        "write outside",
        gate,
    )
    .await;

    let (approval_id, _) = wait_for_approval(&mut rx).await;

    // Cancel while parked: the run ends in Cancelled...
    handle.cancel();
    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Cancelled);

    // ...and the supervisor's settlement sweep (the deterministic cleanup
    // path — the parked future may simply be dropped) resolves the open
    // approval as cancelled on the run's own hub, exactly once no matter
    // which side won the select race.
    publish_cancelled_approvals(registry.as_ref(), "run-perm", &handle.hub);
    let snapshot = handle.hub.snapshot();
    let cancelled: Vec<&AgentEvent> = snapshot
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ApprovalResolved {
                    outcome: oct_agent::agent::ApprovalOutcome::Cancelled,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(cancelled.len(), 1, "exactly one cancelled resolution");
    match cancelled[0] {
        AgentEvent::ApprovalResolved {
            approval_id: id, ..
        } => assert_eq!(*id, approval_id),
        other => panic!("expected ApprovalResolved, got {other:?}"),
    }
    assert!(registry.pending_for_conversation(&conv_id).is_empty());
    // The write never happened.
    assert!(!outside.path().join("cancelled.txt").exists());
}

#[tokio::test]
async fn permission_allow_always_injects_rule_and_skips_next_ask() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");

    // Two identical-class outside writes in one run: the first asks, the
    // second (after the "endpoint" injected the allow_always rule) must
    // pass without another approval.
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "write_file",
                outside_write_args(outside.path(), "first.txt"),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![
            tool_call(
                "call-2",
                "write_file",
                outside_write_args(outside.path(), "second.txt"),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, false, |_| {});
    let (handle, mut rx) = spawn_gated(
        model,
        env.pool.clone(),
        &env,
        &conv_id,
        "write twice",
        gate.clone(),
    )
    .await;

    let (approval_id, event) = wait_for_approval(&mut rx).await;
    let AgentEvent::ApprovalRequested {
        rule_suggestion, ..
    } = &event
    else {
        panic!("expected ApprovalRequested");
    };
    let pattern = rule_suggestion
        .clone()
        .expect("write actions suggest a dir glob");

    // Emulate the decide endpoint's allow_always path: persist is the DB
    // layer's business (covered by the e2e suite); the LIVE part is the
    // rule injection into the conversation's active gate + resolve.
    gate.add_rule(Rule {
        effect: Effect::Allow,
        tool: "write_file".into(),
        pattern: Some(pattern),
    });
    assert!(
        registry
            .resolve(&approval_id, ApprovalDecision::AllowAlways)
            .is_ok()
    );

    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    assert_eq!(
        handle
            .hub
            .snapshot()
            .iter()
            .filter(|e| matches!(e, AgentEvent::ApprovalRequested { .. }))
            .count(),
        1,
        "the second same-rule write must not ask again"
    );
    for file in ["first.txt", "second.txt"] {
        assert_eq!(
            std::fs::read_to_string(outside.path().join(file)).unwrap(),
            "perm-content",
            "{file} must have been written"
        );
    }
}

#[tokio::test]
async fn permission_bypass_mode_runs_without_asking() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "write_file",
                outside_write_args(outside.path(), "bypassed.txt"),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Bypass, false, |_| {});
    let (handle, mut rx) =
        spawn_gated(model, env.pool.clone(), &env, &conv_id, "just write", gate).await;

    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    assert!(
        handle
            .hub
            .snapshot()
            .iter()
            .all(|e| !matches!(e, AgentEvent::ApprovalRequested { .. })),
        "bypass mode must never ask"
    );
    assert!(registry.pending_for_conversation(&conv_id).is_empty());
    assert!(outside.path().join("bypassed.txt").exists());
}

#[tokio::test]
async fn permission_outside_read_asks_in_default_mode() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let outside = tempfile::tempdir().expect("outside temp dir");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "outside-bytes").unwrap();
    let canonical = outside.path().canonicalize().unwrap();

    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "read_file",
                json!({"path": secret.display().to_string()}),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);

    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, false, |_| {});
    let (handle, mut rx) = spawn_gated(
        model,
        env.pool.clone(),
        &env,
        &conv_id,
        "read outside",
        gate,
    )
    .await;

    let (approval_id, event) = wait_for_approval(&mut rx).await;
    match event {
        AgentEvent::ApprovalRequested {
            kind,
            path,
            outside: outside_flag,
            ..
        } => {
            assert_eq!(kind, "read");
            assert_eq!(
                path.as_deref(),
                Some(canonical.join("secret.txt").display().to_string().as_str())
            );
            assert_eq!(outside_flag, Some(true));
        }
        other => panic!("expected ApprovalRequested, got {other:?}"),
    }

    assert!(
        registry
            .resolve(&approval_id, ApprovalDecision::Allow)
            .is_ok()
    );
    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    match events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolResult { .. }))
    {
        Some(AgentEvent::ToolResult {
            content, is_error, ..
        }) => {
            assert!(!is_error);
            assert!(content.contains("outside-bytes"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    drop(handle);
}

#[tokio::test]
async fn permission_worker_ask_waits_for_user_decision() {
    let env = TestEnv::new().await;
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call(
                "call-1",
                "write_file",
                json!({"path": "inner.txt", "content": "x"}),
            ),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]),
    ]);
    let (gate, registry) = build_gate(&env, &conv_id, PermissionMode::Default, true, |_| {});
    let (handle, mut rx) =
        spawn_gated(model, env.pool.clone(), &env, &conv_id, "worker task", gate).await;
    let (approval, _) = wait_for_approval(&mut rx).await;
    assert!(!env.dir.path().join("inner.txt").exists());
    registry
        .resolve(&approval, ApprovalDecision::Allow)
        .unwrap();
    let events = collect_events(&mut rx, is_terminal).await;
    assert_eq!(*events.last().unwrap(), AgentEvent::Finish);
    assert_eq!(
        std::fs::read_to_string(env.dir.path().join("inner.txt")).unwrap(),
        "x"
    );
    drop(handle);
}

// --- Repository instructions & the frozen prompt environment -----------------
//
// These tests drive the RunService's own assembly (`run_root_with_guard`),
// the production path the HTTP layer uses, so the system prompt asserted on
// is exactly what a real run would send: base prompt + code-mode section +
// the frozen `<repo-instructions>` block.

// `compact`, `msg_db` and `Message` are already imported by the compaction
// section above; everything else this section needs is pulled here.
use oct_agent::agent::MicrocompactTrigger;
use oct_agent::agent::repo_instructions::PromptEnvRefresh;
use oct_agent::context::ContextEntry;
use oct_agent::runtime::{RootRunRequest, RunService};

use support::ScriptedObserver;

/// A RunService wired like the test HTTP harness: same two-phase manager
/// wiring, direct tool mode pinned (the app-level code-mode default would
/// otherwise depend on a helper binary next to the test binary).
async fn spawn_run_service(env: &TestEnv) -> Arc<RunService> {
    oct_agent::db::settings::set(
        &env.pool,
        oct_agent::codemode::SETTING,
        oct_agent::codemode::McpToolMode::Direct.as_str(),
    )
    .await
    .expect("pin direct tool mode");
    let subagents = Arc::new(oct_agent::subagent::SubagentManager::default());
    let background = Arc::new(oct_agent::runtime::background::BackgroundTaskManager::new(
        env.pool.clone(),
        env.dir.path().join("exec"),
    ));
    let runs = Arc::new(RunService {
        lifecycle: Default::default(),
        pool: env.pool.clone(),
        registry: Arc::new(oct_llm_provider::providers::default_registry()),
        mcp: Arc::new(oct_agent::mcp::McpManager::default()),
        sessions: Arc::new(dashmap::DashMap::new()),
        file_locks: dashmap::DashMap::new(),
        subagents: subagents.clone(),
        background: background.clone(),
        attachments: Arc::new(oct_agent::attachments::AttachmentService::new(
            env.dir.path().join("attachments"),
            env.pool.clone(),
        )),
        approvals: Arc::new(oct_agent::permissions::registry::ApprovalRegistry::new()),
        gates: Arc::new(dashmap::DashMap::new()),
        prompt_envs: Arc::new(dashmap::DashMap::new()),
        prompt_env_locks: dashmap::DashMap::new(),
    });
    subagents.set_service(runs.clone());
    background.set_service(runs.clone());
    runs
}

/// Run ONE root turn over the production assembly path: insert the user
/// message, load the view, occupy, subscribe, and hand everything to
/// `run_root_with_guard`. Draining the subscription to EOF waits for the
/// supervisor's settlement (the hub drops with the occupancy guard), so the
/// conversation is free for the next turn when this returns. `window` is the
/// run's context window — callers keep it large enough that the turn-top
/// waterline triggers stay silent unless a test wants them.
async fn run_root_turn(
    env: &TestEnv,
    runs: &Arc<RunService>,
    conv_id: &str,
    model: ScriptedModel,
    window: u32,
) -> ScriptedObserver {
    let observer = model.observer();
    let conversation = oct_agent::db::conversations::get_conversation(&env.pool, conv_id)
        .await
        .expect("read conversation")
        .expect("conversation exists");
    let project = oct_agent::db::projects::get_project(&env.pool, &conversation.project_id)
        .await
        .expect("read project")
        .expect("project exists");
    oct_agent::db::messages::insert_message(
        &env.pool,
        conv_id,
        &Message::text(Role::User, "go"),
        None,
        None,
        None,
    )
    .await
    .expect("insert user message");
    let view = oct_agent::db::messages::load_llm_view(&env.pool, conv_id)
        .await
        .expect("load view");

    let guard = runs.occupy(conv_id).expect("conversation is free");
    // Subscribe through a short-lived hub borrow: holding the hub Arc (or a
    // whole RunHandle clone) for the drain below would keep the hub alive
    // past the supervisor's settlement and the stream would never EOF.
    let mut stream = {
        let hub = runs
            .sessions
            .get(conv_id)
            .expect("occupancy guarantees the entry")
            .value()
            .hub
            .clone();
        hub.subscribe()
    };
    runs.run_root_with_guard(RootRunRequest {
        guard,
        conversation_id: conv_id.to_string(),
        project,
        model: Box::new(model),
        provider_id: "test".to_string(),
        model_id: "scripted".to_string(),
        view,
        context_window: Some(window),
        idle: None,
        reasoning: None,
        attach_task_tool: false,
    })
    .await
    .expect("root run starts");

    // SSE EOF = the supervisor settled and occupancy released.
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("run settled within the timeout (loop hung?)")
    {
        let _ = event;
    }
    observer
}

/// A model whose single turn answers with plain text and stops.
fn plain_turn_model() -> ScriptedModel {
    ScriptedModel::new(vec![Turn::Stream(vec![
        text("ok"),
        finish(FinishReason::Stop),
    ])])
}

/// The system prompt text of the observer's n-th recorded request.
fn system_prompt_at(observer: &ScriptedObserver, index: usize) -> String {
    let requests = observer.requests();
    assert!(
        requests.len() > index,
        "model saw at least {} requests",
        index + 1
    );
    let message = &requests[index].messages[0];
    assert_eq!(message.role, Role::System);
    message
        .parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// The conversation's entries (message + ordering), zipped the way the chat
/// layer builds them for the compaction transaction.
async fn conversation_entries(env: &TestEnv, conv_id: &str) -> Vec<ContextEntry> {
    let view = oct_agent::db::messages::load_llm_view(&env.pool, conv_id)
        .await
        .expect("load view");
    view.messages
        .into_iter()
        .zip(view.orderings)
        .map(|(message, ordering)| ContextEntry { message, ordering })
        .collect()
}

#[tokio::test]
async fn repo_instructions_block_enters_the_system_prompt() {
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("AGENTS.md"), "Use tabs in this repo.").unwrap();
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let runs = spawn_run_service(&env).await;

    let observer = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 8192).await;
    let system = system_prompt_at(&observer, 0);
    assert!(
        system.contains("<repo-instructions>"),
        "block wrapper present"
    );
    assert!(
        system.contains("Use tabs in this repo."),
        "file content present"
    );
    assert!(system.contains("</repo-instructions>"));
    // The file tag carries the absolute path.
    let tagged = format!("path=\"{}\"", env.dir.path().join("AGENTS.md").display());
    assert!(system.contains(&tagged), "absolute path in the file tag");
    // The block closes the prompt: nothing after the wrapper's end.
    assert!(system.rfind("</repo-instructions>").is_some_and(|start| {
        system[start + "</repo-instructions>".len()..]
            .trim()
            .is_empty()
    }));
}

#[tokio::test]
async fn claude_md_loads_only_as_the_same_directory_fallback() {
    let runs_setup = async |env: &TestEnv| {
        let conv_id = new_conversation(&env.pool, env.dir.path()).await;
        let runs = spawn_run_service(env).await;
        let observer = run_root_turn(env, &runs, &conv_id, plain_turn_model(), 8192).await;
        (conv_id, runs, observer)
    };

    // CLAUDE.md alone: it is the fallback and loads.
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("CLAUDE.md"), "claude-only rules").unwrap();
    let (_, _, observer) = runs_setup(&env).await;
    let system = system_prompt_at(&observer, 0);
    assert!(system.contains("claude-only rules"));

    // Both present: AGENTS.md wins and CLAUDE.md does not load.
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("AGENTS.md"), "agents rules").unwrap();
    std::fs::write(env.dir.path().join("CLAUDE.md"), "claude rules").unwrap();
    let (_, _, observer) = runs_setup(&env).await;
    let system = system_prompt_at(&observer, 0);
    assert!(system.contains("agents rules"));
    assert!(
        !system.contains("claude rules"),
        "no double load per directory"
    );
}

#[tokio::test]
async fn frozen_repo_block_is_reused_byte_for_byte_across_runs() {
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v1").unwrap();
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let runs = spawn_run_service(&env).await;

    let first = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 8192).await;
    let system1 = system_prompt_at(&first, 0);
    assert!(system1.contains("RULE-v1"));

    // Edit the file between runs WITHOUT a refresh point: the frozen block
    // must keep the prompt byte-identical (this is the prefix-cache
    // guarantee).
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v2").unwrap();
    let second = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 8192).await;
    let system2 = system_prompt_at(&second, 0);
    assert_eq!(
        system1, system2,
        "clean entry reuses the block byte-for-byte"
    );
}

#[tokio::test]
async fn manual_compaction_refreshes_the_frozen_block_on_the_next_run() {
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v1").unwrap();
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let runs = spawn_run_service(&env).await;

    let first = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 8192).await;
    assert!(system_prompt_at(&first, 0).contains("RULE-v1"));

    // Grow the history past the compaction floor with text the tail budget
    // (window 1024 → 102 tokens) cannot keep verbatim.
    for _ in 0..5 {
        for role_text in [("user", "x".repeat(160)), ("assistant", "y".repeat(160))] {
            let role = if role_text.0 == "user" {
                Role::User
            } else {
                Role::Assistant
            };
            oct_agent::db::messages::insert_message(
                &env.pool,
                &conv_id,
                &Message::text(role, &role_text.1),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
    }

    // The shared compaction transaction with the conversation's refresh
    // handle — the same wiring the manual endpoint uses.
    let entries = conversation_entries(&env, &conv_id).await;
    let summary_model = ScriptedModel::new(vec![]);
    summary_model.push_generation(Ok("summary of the region".into()));
    let outcome = match compact::run_compaction(
        &summary_model,
        &env.pool,
        &conv_id,
        compact::CompactRequest {
            entries: &entries,
            est_before: 4_000,
            system_prompt: None,
            tool_specs: &[],
            window: Some(1024),
            trigger: oct_agent::agent::CompactionTrigger::Manual,
            hub: None,
            cancel: None,
            prompt_env_refresh: Some(runs.prompt_env_refresh(&conv_id)),
        },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => panic!("manual compaction over the seeded history must succeed"),
    };

    // Edit AFTER the compaction flagged the entry dirty: the next run must
    // re-read the files (fresh text) instead of reusing the frozen block.
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v2").unwrap();
    let second = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 8192).await;
    let system = system_prompt_at(&second, 0);
    assert!(system.contains("RULE-v2"), "dirty entry re-reads the files");
    assert!(!system.contains("RULE-v1"));
    // The compaction really happened (a marker row landed in the timeline).
    assert!(!outcome.summary_message_id.is_empty());
}

#[tokio::test]
async fn microcompaction_refreshes_the_frozen_block_on_the_next_run() {
    let env = TestEnv::new().await;
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v1").unwrap();
    let conv_id = new_conversation(&env.pool, env.dir.path()).await;
    let runs = spawn_run_service(&env).await;

    // Seed a history with clearable old tool results: user, nine
    // (assistant call + large result) rounds, user. The newest five results
    // and the trailing round are protected; the four oldest clear. The
    // rounds are sized to keep the total estimate BELOW the waterline
    // (8192 × 70% ≈ 5734) so the first run below does NOT microcompact on
    // its own — the manual invocation is what must flag the environment.
    oct_agent::db::messages::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "go"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    for i in 0..9 {
        let assistant = Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: format!("c{i}"),
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            })],
        };
        oct_agent::db::messages::insert_message(&env.pool, &conv_id, &assistant, None, None, None)
            .await
            .unwrap();
        let result = Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: format!("c{i}"),
                content: serde_json::Value::String("x".repeat(1_600)),
                is_error: false,
                images: Vec::new(),
            })],
        };
        oct_agent::db::messages::insert_message(&env.pool, &conv_id, &result, None, None, None)
            .await
            .unwrap();
    }
    oct_agent::db::messages::insert_message(
        &env.pool,
        &conv_id,
        &Message::text(Role::User, "next"),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // First run freezes the environment with the v1 block. The oversized
    // window keeps BOTH turn-top triggers silent (history + prompt stay far
    // below 70%/85% of it) — only the manual invocation below may touch the
    // environment.
    let first = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 16384).await;
    assert!(system_prompt_at(&first, 0).contains("RULE-v1"));

    // One real microcompaction over the persisted history, with the
    // conversation's refresh handle wired exactly like the loop does.
    let view = oct_agent::db::messages::load_llm_view(&env.pool, &conv_id)
        .await
        .expect("load view");
    let mut context = oct_agent::context::ContextState::new("sys".into(), view);
    let hub = oct_agent::agent::EventHub::new();
    let refresh: PromptEnvRefresh = runs.prompt_env_refresh(&conv_id);
    let attempt = oct_agent::agent::micro::run_microcompaction(
        &env.pool,
        &conv_id,
        &hub,
        MicrocompactTrigger::Waterline,
        &mut context,
        &[],
        Some(&refresh),
    )
    .await;
    assert!(
        matches!(attempt, oct_agent::agent::micro::MicroAttempt::Done(_)),
        "seeded history has clearable results"
    );

    // Dirty → the next run re-reads the file.
    std::fs::write(env.dir.path().join("AGENTS.md"), "RULE-v2").unwrap();
    let second = run_root_turn(&env, &runs, &conv_id, plain_turn_model(), 16384).await;
    let system = system_prompt_at(&second, 0);
    assert!(system.contains("RULE-v2"));
    assert!(!system.contains("RULE-v1"));
}
