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
use oct_llm_provider::core::{ContentPart, FinishReason, ModelError, Role, ToolCall, ToolResult};
use oct_mcp::McpCallResult;

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
                },
            )
            .with_result(
                "stats",
                McpCallResult {
                    text: String::new(),
                    structured_json: Some(json!({"rows": [], "count": 0})),
                    is_error: false,
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
                ..
            })
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
