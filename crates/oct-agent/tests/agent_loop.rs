//! Agent loop integration tests. The LLM is replaced by a scripted model
//! (see `support`); everything else — tools, SQLite persistence, event
//! broadcast, cancellation — runs the production code paths.

mod support;

use async_trait::async_trait;
use serde_json::json;

use oct_agent::agent::AgentEvent;
use oct_agent::db;
use oct_agent::tools::{self, AgentTool, ToolContext, ToolOutput};
use oct_llm_provider::core::{ContentPart, FinishReason, ModelError, Role, ToolCall, ToolResult};

use support::{
    ScriptedModel, TestEnv, Turn, collect_events, finish, is_terminal, new_conversation,
    reasoning, spawn_agent, text, tool_call, usage,
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
    let details: serde_json::Value =
        serde_json::from_str(stored[0].details_json.as_deref().expect("assistant details"))
            .expect("details_json should deserialize");
    let ms = details["reasoning_duration_ms"]
        .as_u64()
        .expect("reasoning_duration_ms should be a number");
    assert!(ms < 30_000, "scripted stream finishes instantly, got {ms}ms");

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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
        tools::default_tools(env.dir.path().to_path_buf()),
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
