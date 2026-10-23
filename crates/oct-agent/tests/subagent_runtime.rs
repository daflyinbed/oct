//! Subagent runtime integration tests (PR2 acceptance).
//!
//! Everything rides the full production chain — HTTP API → ApplicationServices →
//! agent loop → real tools → SQLite → SSE — with the LLM behind the shared
//! OpenAI-compatible mock. Per-agent scripting is routed by MARKER: each
//! task's delegation prompt embeds a unique marker (`TASKA-7f3a`), so the
//! parent's turns and every child's turns each get their own scripted queue
//! on the one mock (the "可按 agent/run 分配的模型工厂").
//!
//! No sleeps: every wait is parked on an SSE event, a stream EOF (the
//! settlement signal — the child's occupancy guard drops exactly when its
//! supervisor finishes) or a gate the test holds.

mod support;

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};

use support::TestApp;
use support::llm::{LlmTurn, finish_chunk, gate, spawn_llm_mock, text_chunk, tool_call_chunk};
use support::sse_payloads;

// --- Shared helpers ---------------------------------------------------------

type EventStream = futures_util::stream::BoxStream<'static, Value>;

/// Next SSE payload matching `pred`; fails the test on timeout (30s) or
/// stream end, mirroring `support::collect_events`'s assertion model.
async fn next_event(stream: &mut EventStream, pred: impl Fn(&Value) -> bool) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("timed out waiting for an SSE event (run hung?)")
            .expect("SSE stream ended before the expected event");
        if pred(&event) {
            return event;
        }
    }
}

/// Drain the remaining payloads until the stream ends (SSE EOF = the run's
/// hub dropped = the supervisor finished settlement).
async fn drain_to_end(stream: &mut EventStream) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("timed out draining the SSE stream (EOF never arrived?)");
        match event {
            Some(event) => out.push(event),
            None => return out,
        }
    }
}

fn updated(desc: &str, status: &str) -> impl Fn(&Value) -> bool {
    let desc = desc.to_string();
    let status = status.to_string();
    move |v: &Value| {
        v["type"] == "subagent_updated"
            && v["data"]["description"] == desc
            && v["data"]["status"] == status
    }
}
/// GET /api/conversations/{conv}/subagents
async fn subagent_list(app: &TestApp, conv: &str) -> Vec<Value> {
    let resp = app
        .get_json(&format!("/api/conversations/{conv}/subagents"))
        .await;
    assert_eq!(resp.status(), 200, "subagent list should load");
    resp.json::<Vec<Value>>().await.expect("task list json")
}

/// Parallel dispatch does not promise cross-child event ordering: B may
/// complete before A's startup event. Observe both without discarding either.
async fn running_a_and_completed_b(events: &mut EventStream) -> String {
    let mut agent_a = None;
    let mut b_completed = false;
    while agent_a.is_none() || !b_completed {
        let event = next_event(events, |v| {
            updated("A", "running")(v) || updated("B", "completed")(v)
        })
        .await;
        if updated("A", "running")(&event) {
            agent_a = Some(event["data"]["agent_id"].as_str().unwrap().to_string());
        } else {
            b_completed = true;
        }
    }
    agent_a.unwrap()
}

/// GET /api/conversations/{conv}/messages
async fn history(app: &TestApp, conv: &str) -> Vec<Value> {
    let resp = app
        .get_json(&format!("/api/conversations/{conv}/messages"))
        .await;
    assert_eq!(resp.status(), 200);
    resp.json::<Vec<Value>>().await.expect("history json")
}

/// The (content, is_error) of the tool result answering `call_id`.
fn tool_result_of<'a>(messages: &'a [Value], call_id: &str) -> Option<(&'a Value, bool)> {
    messages.iter().find_map(|m| {
        m["parts"].as_array()?.iter().find_map(|p| {
            if p["type"] == "tool_result" && p["call_id"] == call_id {
                Some((&p["content"], p["is_error"].as_bool().unwrap_or(false)))
            } else {
                None
            }
        })
    })
}

/// The subagent rows of a root conversation, straight from the DB.
async fn subagent_rows(
    pool: &sqlx::SqlitePool,
    root_conv: &str,
) -> Vec<(String, String, i64, String)> {
    let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT r.id, r.status, r.execution_epoch, c.id FROM agent_runs r \
         JOIN conversations c ON c.id = r.conversation_id \
         WHERE c.root_conversation_id = $1 AND r.kind = 'subagent'",
    )
    .bind(root_conv)
    .fetch_all(pool)
    .await
    .expect("subagent rows");
    rows
}

/// The `task` tool call the parent model makes for one delegation.
fn task_call(call_id: &str, description: &str, prompt: &str) -> Value {
    task_call_at(0, call_id, description, prompt)
}

/// A `task` tool call at a given tool-call index: parallel calls in one
/// round need distinct indexes (the adapter groups deltas by index).
fn task_call_at(index: u64, call_id: &str, description: &str, prompt: &str) -> Value {
    let args = json!({ "description": description, "prompt": prompt }).to_string();
    support::llm::tool_call_chunk_at(index, call_id, "task", &args)
}

/// A parent turn issuing ONE task call (`call-a`) delegating the marker's
/// task — the shape the recovery/backfill tests use.
fn parent_single_task_turn(marker: &str) -> LlmTurn {
    LdmTurns::single(marker)
}

/// A child that answers immediately with `text` and stops.
fn quick_child_turn(text: &str) -> LlmTurn {
    LdmTurns::quick_child(text)
}

/// The parent turn that issues the two task calls, then ends the round with
/// the tool_calls finish reason.
fn dual_task_parent_turn() -> LlmTurn {
    LlmTurn::Complete(vec![
        task_call_at(0, "call-a", "A", "Research TASKA-7f3a and report."),
        task_call_at(1, "call-b", "B", "Research TASKB-9c2e and report."),
        finish_chunk("tool_calls"),
    ])
}

/// Wire a full app: mock provider registered, project + conversation created.
async fn wired_app(llm: &support::llm::LlmMock) -> (TestApp, String, String) {
    let app = TestApp::new().await;
    app.register_llm_provider(llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    // This suite predates the permission layer and its workers
    // write/edit inside the workspace — under `default` mode a subagent's
    // Interactive approvals are exercised separately; these tests
    // exercise. `bypass` keeps the subagent flows as they were.
    app.set_permission_mode(&project, &conv, "bypass").await;
    (app, project, conv)
}

// --- 1. Parallel execution --------------------------------------------------

#[tokio::test]
async fn parallel_tasks_run_concurrently_and_settle_once() {
    let llm = spawn_llm_mock(vec![
        dual_task_parent_turn(),
        LlmTurn::Complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    // Child A parks mid-stream (the test holds the gate); child B completes
    // immediately — observing B settled while A is still parked proves the
    // two task calls of one round execute concurrently.
    let (gate_a, gate_a_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("a-working")],
            gate: gate_a_rx,
            post: vec![text_chunk(" done-a"), finish_chunk("stop")],
        }],
    );
    llm.push_marker_turns(
        "TASKB-9c2e",
        vec![LlmTurn::Complete(vec![
            text_chunk("done-b"),
            finish_chunk("stop"),
        ])],
    );
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    running_a_and_completed_b(&mut events).await;

    // Overlap proof: A is still parked behind the closed gate, yet B settled.
    let tasks = subagent_list(&app, &conv).await;
    let a = tasks
        .iter()
        .find(|t| t["description"] == "A")
        .expect("task A");
    let b = tasks
        .iter()
        .find(|t| t["description"] == "B")
        .expect("task B");
    assert_eq!(a["status"], "running");
    assert!(a["running"].as_bool().unwrap(), "A live in memory");
    assert_eq!(b["status"], "completed");
    assert!(!b["running"].as_bool().unwrap(), "B settled");

    gate_a.send(true).unwrap();
    next_event(&mut events, updated("A", "completed")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    // Results land in the parent history exactly once per call, both
    // non-error envelopes carrying status completed.
    let messages = history(&app, &conv).await;
    for call in ["call-a", "call-b"] {
        let (content, is_error) = tool_result_of(&messages, call).expect("task result row");
        assert!(!is_error, "{call} must not be an error result");
        let envelope: Value = serde_json::from_str(content.as_str().expect("envelope is a string"))
            .expect("envelope json");
        assert_eq!(envelope["status"], "completed");
    }

    let rows = subagent_rows(&app.pool, &conv).await;
    assert_eq!(rows.len(), 2, "one child run per task call");
    assert!(rows.iter().all(|r| r.1 == "completed"));

    // The child conversation keeps its full history (delegation + reply).
    // The envelope's agent_id is the child CONVERSATION id (column 4).
    let agent_b = rows
        .iter()
        .find(|r| r.3 == tool_result_agent(&messages, "call-b"))
        .map(|r| r.3.clone())
        .expect("B row");
    let child_messages = history(&app, &agent_b).await;
    assert_eq!(
        child_messages.first().expect("delegation")["role"],
        "user",
        "the child conversation starts with the delegation prompt"
    );
}

/// The concatenated USER-role message content of a captured mock request —
/// the same key the mock's marker routing uses.
fn user_role_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|m| m["role"] == "user")
                .map(|m| m["content"].to_string())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// The agent id a persisted task envelope names (details live on the tool
/// result's metadata only in the UI layer; the envelope JSON carries it too).
fn tool_result_agent(messages: &[Value], call_id: &str) -> String {
    let (content, _) = tool_result_of(messages, call_id).expect("task result");
    let envelope: Value =
        serde_json::from_str(content.as_str().expect("envelope")).expect("envelope");
    envelope["agent_id"].as_str().expect("agent id").to_string()
}

// --- 2. Parent cancel cascades ----------------------------------------------

#[tokio::test]
async fn parent_cancel_cascades_to_children() {
    let llm = spawn_llm_mock(vec![dual_task_parent_turn()]).await;
    let (_gate_a, gate_a_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("a-working")],
            gate: gate_a_rx,
            post: vec![finish_chunk("stop")],
        }],
    );
    llm.push_marker_turns(
        "TASKB-9c2e",
        vec![LlmTurn::Complete(vec![
            text_chunk("done-b"),
            finish_chunk("stop"),
        ])],
    );
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    let a_started = next_event(&mut events, updated("A", "running")).await;
    let agent_a = a_started["data"]["agent_id"].as_str().unwrap().to_string();

    // Subscribe to the child's own stream BEFORE cancelling: its EOF is the
    // deterministic settlement signal (the occupancy guard drops when the
    // child's supervisor finishes).
    let child_resp = app
        .get_json(&format!("/api/subagents/{agent_a}/events"))
        .await;
    assert_eq!(child_resp.status(), 200, "child stream should be live");
    let mut child_events = sse_payloads(child_resp);

    app.cancel(&conv).await;
    next_event(&mut events, |v| v["type"] == "cancelled").await;
    drain_to_end(&mut events).await;
    drain_to_end(&mut child_events).await;

    let rows = subagent_rows(&app.pool, &conv).await;
    let a = rows.iter().find(|r| r.3 == agent_a).expect("A row");
    assert_eq!(a.1, "cancelled", "the cascaded child is cancelled");
    let tasks = subagent_list(&app, &conv).await;
    assert!(
        tasks.iter().all(|t| !t["running"].as_bool().unwrap()),
        "no active children remain"
    );

    // The parent history stays legal: the cancelled task call is answered by
    // the loop's synthetic cancel result.
    let messages = history(&app, &conv).await;
    let (_, is_error) = tool_result_of(&messages, "call-a").expect("call-a answered");
    assert!(
        is_error,
        "the cancelled delegation reads as an error result"
    );
}

// --- 3. One child failing does not block the sibling -------------------------

#[tokio::test]
async fn child_failure_does_not_block_sibling() {
    let llm = spawn_llm_mock(vec![
        dual_task_parent_turn(),
        LlmTurn::Complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns("TASKA-7f3a", vec![LlmTurn::Fail(500)]);
    llm.push_marker_turns(
        "TASKB-9c2e",
        vec![LlmTurn::Complete(vec![
            text_chunk("done-b"),
            finish_chunk("stop"),
        ])],
    );
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    // Both children settle in one round trip each — collect the whole run
    // and assert on the set (no cross-child ordering assumption).
    let collected = drain_to_end(&mut events).await;
    assert!(
        collected.iter().any(|v| updated("A", "failed")(v)),
        "the failed child surfaces on the parent stream"
    );
    assert!(
        collected.iter().any(|v| updated("B", "completed")(v)),
        "the sibling completes normally"
    );
    assert!(collected.iter().any(|v| v["type"] == "finish"));

    let messages = history(&app, &conv).await;
    let (content, is_error) =
        tool_result_of(&messages, "call-a").expect("failed task still answers");
    assert!(is_error, "a failed child surfaces as a task error");
    let envelope: Value = serde_json::from_str(content.as_str().unwrap()).expect("envelope");
    assert_eq!(envelope["status"], "failed");

    let (_, b_error) = tool_result_of(&messages, "call-b").expect("B answered");
    assert!(!b_error, "the sibling completed normally");
}

// --- 4. Cancelling one child does not touch the sibling ----------------------

#[tokio::test]
async fn cancel_single_child_only() {
    let llm = spawn_llm_mock(vec![
        dual_task_parent_turn(),
        LlmTurn::Complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    let (_gate_a, gate_a_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("a-working")],
            gate: gate_a_rx,
            post: vec![finish_chunk("stop")],
        }],
    );
    llm.push_marker_turns(
        "TASKB-9c2e",
        vec![LlmTurn::Complete(vec![
            text_chunk("done-b"),
            finish_chunk("stop"),
        ])],
    );
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    let agent_a = running_a_and_completed_b(&mut events).await;

    let cancel_resp = app
        .post_json(&format!("/api/subagents/{agent_a}/cancel"), json!(null))
        .await;
    assert_eq!(cancel_resp.status(), 200);

    // The parent receives the cancelled envelope and finishes normally; the
    // never-opened gate proves the child stopped through cancellation, not
    // by running to completion.
    next_event(&mut events, updated("A", "cancelled")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    let rows = subagent_rows(&app.pool, &conv).await;
    let by_agent = |agent: &str| rows.iter().find(|r| r.3 == agent).expect("row");
    assert_eq!(by_agent(&agent_a).1, "cancelled");
    let messages = history(&app, &conv).await;
    let (content, is_error) = tool_result_of(&messages, "call-a").expect("cancelled task answers");
    assert!(is_error);
    let envelope: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert_eq!(envelope["status"], "cancelled");
}

// --- 5. Cross-agent read-state isolation ------------------------------------

/// A worker's edit_file without its own read_file is rejected — the parent's
/// (or a sibling's) reads never authorize it.
#[tokio::test]
async fn child_edit_without_own_read_errors() {
    let llm = spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            task_call("call-a", "A", "Edit f.txt now. TASKA-7f3a"),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![
            LlmTurn::Complete(vec![
                tool_call_chunk(
                    "ca1",
                    "edit_file",
                    &json!({
                        "file_path": "f.txt",
                        "old_string": "v0",
                        "new_string": "a-edited",
                    })
                    .to_string(),
                ),
                finish_chunk("tool_calls"),
            ]),
            LlmTurn::Complete(vec![text_chunk("gave up"), finish_chunk("stop")]),
        ],
    );
    let (app, _project, conv) = wired_app(&llm).await;
    std::fs::write(app.dir.path().join("f.txt"), "v0").unwrap();

    let mut events = sse_payloads(app.send_message(&conv, "edit the file").await);
    next_event(&mut events, updated("A", "completed")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    let rows = subagent_rows(&app.pool, &conv).await;
    let child_messages = history(&app, &rows[0].3).await;
    let (content, is_error) = tool_result_of(&child_messages, "ca1").expect("edit result");
    assert!(is_error, "edit without the agent's own read must fail");
    let text = content.as_str().unwrap();
    assert!(
        text.contains("has not been read"),
        "the read-before-edit rule fires for the fresh worker: {text}"
    );
}

/// After agent A writes a file, agent B's EARLIER read is stale — A's write
/// does not refresh B's records (per-agent read state over shared locks).
#[tokio::test]
async fn sibling_write_makes_earlier_read_stale() {
    let llm = spawn_llm_mock(vec![dual_task_parent_turn()]).await;
    // A's write is gated so the test can guarantee B's read lands FIRST
    // (otherwise scheduling could run B's read after A's write, which would
    // leave B's record fresh and defeat the point).
    let (gate_a_write, gate_a_write_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![
            LlmTurn::Complete(vec![
                tool_call_chunk("ra1", "read_file", &json!({"path": "f.txt"}).to_string()),
                finish_chunk("tool_calls"),
            ]),
            LlmTurn::Gated {
                pre: vec![],
                gate: gate_a_write_rx,
                post: vec![
                    tool_call_chunk(
                        "wa1",
                        "write_file",
                        &json!({"path": "f.txt", "content": "v1-from-a"}).to_string(),
                    ),
                    finish_chunk("tool_calls"),
                ],
            },
            LdmTurns::quick_child("done-a"),
        ],
    );
    // B's edit is gated until after A's completed write: B's EARLIER read
    // must read as stale by then.
    let (gate_b, gate_b_rx) = gate();
    llm.push_marker_turns(
        "TASKB-9c2e",
        vec![
            LdmTurns::complete(vec![
                tool_call_chunk("rb1", "read_file", &json!({"path": "f.txt"}).to_string()),
                finish_chunk("tool_calls"),
            ]),
            LlmTurn::Gated {
                pre: vec![],
                gate: gate_b_rx,
                post: vec![
                    tool_call_chunk(
                        "eb1",
                        "edit_file",
                        &json!({
                            "file_path": "f.txt",
                            "old_string": "v0",
                            "new_string": "b-edited",
                        })
                        .to_string(),
                    ),
                    finish_chunk("tool_calls"),
                ],
            },
            LdmTurns::quick_child("done-b"),
        ],
    );
    let (app, _project, conv) = wired_app(&llm).await;
    std::fs::write(app.dir.path().join("f.txt"), "v0").unwrap();

    let mut events = sse_payloads(app.send_message(&conv, "please edit").await);
    let b_started = next_event(&mut events, updated("B", "running")).await;
    let agent_b = b_started["data"]["agent_id"].as_str().unwrap().to_string();

    // Park on B's own stream until its read result lands — the deterministic
    // "B has read" signal — then let A's write through.
    let b_resp = app
        .get_json(&format!("/api/subagents/{agent_b}/events"))
        .await;
    assert_eq!(b_resp.status(), 200);
    let mut b_stream = sse_payloads(b_resp);
    next_event(&mut b_stream, |v| {
        v["type"] == "tool_result" && v["data"]["call_id"] == "rb1"
    })
    .await;
    gate_a_write.send(true).unwrap();
    next_event(&mut events, updated("A", "completed")).await;
    // Force a deterministic mtime gap: filesystem timestamp granularity can
    // land A's write in the same tick as B's earlier read (T0 == T1), which
    // would hide the staleness from B's check. Nudging the file's mtime
    // forward pins the gap regardless of the filesystem's clock resolution.
    {
        let file = std::fs::File::options()
            .write(true)
            .open(app.dir.path().join("f.txt"))
            .unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        file.set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
    }
    gate_b.send(true).unwrap();
    next_event(&mut events, updated("B", "completed")).await;
    drain_to_end(&mut events).await;
    drain_to_end(&mut b_stream).await;

    let rows = subagent_rows(&app.pool, &conv).await;
    let parent_messages = history(&app, &conv).await;
    let agent_b = rows
        .iter()
        .find(|r| r.3 == tool_result_agent(&parent_messages, "call-b"))
        .map(|r| r.3.clone())
        .expect("B row");
    let child_messages = history(&app, &agent_b).await;
    let (content, is_error) = tool_result_of(&child_messages, "eb1").expect("B edit result");
    assert!(is_error, "B's pre-A read must be stale after A's write");
    let text = content.as_str().unwrap();
    assert!(
        text.contains("modified since read"),
        "stale detection names the actual cause: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(app.dir.path().join("f.txt")).unwrap(),
        "v1-from-a",
        "A's write stands; B's rejected edit changed nothing"
    );
}

// --- 6. Toolset inheritance: parent minus task, MCP included -----------------

#[tokio::test]
async fn child_inherits_parent_tools_excluding_task() {
    let llm = spawn_llm_mock(vec![
        LdmTurns::single("TASKA-7f3a"),
        LdmTurns::complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    // The child calls the inherited MCP tool first, then reports.
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![
            LdmTurns::complete(vec![
                tool_call_chunk(
                    "ce1",
                    "mcp__fake__echo",
                    &json!({"message": "hi"}).to_string(),
                ),
                finish_chunk("tool_calls"),
            ]),
            LdmTurns::complete(vec![
                tool_call_chunk(
                    "ce2",
                    "mcp__fake__echo",
                    &json!({"message":"after saved rule"}).to_string(),
                ),
                finish_chunk("tool_calls"),
            ]),
            LdmTurns::quick_child("done-a"),
        ],
    );
    let (app, project, conv) = wired_app(&llm).await;
    app.set_permission_mode(&project, &conv, "default").await;

    // A global (scope = None) fake MCP server: its tools join every run of
    // every project, parent and child alike, on the one shared connection.
    let fake = std::sync::Arc::new(support::mcp::FakeMcpConnection::new(vec![
        support::mcp::fake_tool("echo", Some("Echo the arguments back.")),
    ]));
    let listing = vec![support::mcp::fake_tool(
        "echo",
        Some("Echo the arguments back."),
    )];
    app.mcp.insert_connection_for_test(
        &uuid::Uuid::new_v4().to_string(),
        "fake",
        None,
        fake.clone(),
        listing,
    );

    let mut events = sse_payloads(app.send_message(&conv, "use the tools").await);
    let requested = next_event(&mut events, |event| event["type"] == "approval_requested").await;
    let approval = requested["data"]["approval_id"].as_str().unwrap();
    let agent = requested["data"]["agent_id"].as_str().unwrap();
    assert_eq!(
        app.post_json(
            &format!("/api/conversations/{agent}/approvals/{approval}"),
            json!({"decision":"allow_always"})
        )
        .await
        .status(),
        200
    );
    next_event(&mut events, updated("A", "completed")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    // Schema equality: the child request advertises the parent's tools minus
    // `task` — the MCP tool rides along on the SHARED connection.
    let requests = llm.requests();
    let child_request = requests
        .iter()
        .find(|r| r["messages"].to_string().contains("TASKA-7f3a") && r["tools"].is_array())
        .expect("child request captured");
    let child_tools: Vec<&str> = child_request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        child_tools.contains(&"mcp__fake__echo"),
        "the child inherits the MCP tool: {child_tools:?}"
    );
    assert!(
        !child_tools.contains(&"task"),
        "the child never sees the delegation tool: {child_tools:?}"
    );
    assert!(child_tools.contains(&"read_file"));

    // Dispatch equality: the call went over the shared connection under the
    // ORIGINAL server-side name, and cancelling-free usage never shut it down.
    assert_eq!(
        fake.calls(),
        vec![
            ("echo".to_string(), json!({"message": "hi"})),
            ("echo".to_string(), json!({"message": "after saved rule"})),
        ],
        "the child dispatched to the shared MCP connection"
    );
    assert_eq!(fake.shutdown_count(), 0, "the shared connection survives");
    let rules = oct_agent::db::permissions::list_rules_for_project(&app.pool, &project)
        .await
        .unwrap();
    assert!(rules.iter().any(|rule| rule.tool == "mcp__fake__echo"
        && rule.pattern.is_none()
        && rule.source == "approval"));
}

// --- 6b. Repo instructions shared across the task tree -----------------------

/// The `<repo-instructions>` … `</repo-instructions>` substring of a captured
/// request's system message.
fn repo_block_of(request: &Value) -> String {
    let system = request["messages"][0]["content"]
        .as_str()
        .expect("system message");
    let start = system
        .find("<repo-instructions>")
        .expect("repo block present");
    let end = start
        + system[start..]
            .find("</repo-instructions>")
            .expect("repo block end")
        + "</repo-instructions>".len();
    system[start..end].to_string()
}

#[tokio::test]
async fn worker_prompt_carries_the_parents_repo_instructions_block() {
    let llm = spawn_llm_mock(vec![
        LdmTurns::single("TASKA-7f3a"),
        LdmTurns::complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns("TASKA-7f3a", vec![LdmTurns::quick_child("done-a")]);
    let (app, _project, conv) = wired_app(&llm).await;

    // The workspace instruction file exists BEFORE the parent's first run —
    // its assembly freezes the tree's prompt environment.
    std::fs::write(
        app.dir.path().join("AGENTS.md"),
        "SHARED-REPO-RULES everywhere",
    )
    .unwrap();

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    next_event(&mut events, updated("A", "completed")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    // Parent = the request carrying the research ask; child = the request
    // whose user turn is the delegation prompt (the marker key).
    let requests = llm.requests();
    let parent_request = requests
        .iter()
        .find(|r| r["messages"].to_string().contains("please research"))
        .expect("parent request captured");
    let child_request = requests
        .iter()
        .find(|r| user_role_text(r).contains("TASKA-7f3a"))
        .expect("child request captured");

    // Both prompts carry the block, and it is byte-identical: the worker
    // shares the ROOT conversation's frozen entry, so parent and children
    // never drift mid-conversation.
    let parent_block = repo_block_of(parent_request);
    let child_block = repo_block_of(child_request);
    assert!(parent_block.contains("SHARED-REPO-RULES everywhere"));
    assert!(child_block.contains("SHARED-REPO-RULES everywhere"));
    assert_eq!(parent_block, child_block);
}

// --- 7. Crash recovery: both lost, one resume restarts the tree --------------

#[tokio::test]
async fn restart_recovery_resumes_child_then_parent() {
    let llm = spawn_llm_mock(vec![
        parent_single_task_turn("TASKA-7f3a"),
        // The parent's second turn is pushed later, after the "restart".
    ])
    .await;
    let (_gate_a, gate_a_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("a-partial")],
            gate: gate_a_rx,
            post: vec![finish_chunk("stop")],
        }],
    );
    let (app, project, conv) = wired_app(&llm).await;
    assert_eq!(app.post_json("/api/providers/mock-llm/models", json!({"model_id":"snapshot-model","name":"snapshot-model","tool_call":false,"limit_context":54321})).await.status(), 201);
    let config_path = format!("/api/projects/{project}/settings/agents/worker");
    assert_eq!(
        app.patch_json(
            &config_path,
            json!({
                "model":{"mode":"fixed","provider_id":"mock-llm","model_id":"snapshot-model"},
                "instructions":"ORIGINAL-SNAPSHOT-INSTRUCTIONS",
                "tools":{"mode":"allowlist","items":[]}
            })
        )
        .await
        .status(),
        200
    );

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    let initial_child = next_event(&mut events, updated("A", "running")).await;
    let agent_id = initial_child["data"]["agent_id"].as_str().unwrap();

    // Simulate the crash: checkpoint the WAL, copy the db file, boot a NEW
    // app over the copy. Its startup sweep marks the interrupted parent and
    // child runs `lost` — the recovery input.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&app.pool)
        .await
        .unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::copy(app.dir.path().join("test.db"), dir2.path().join("test.db")).unwrap();
    let app2 = TestApp::new_with_dir(dir2).await;
    assert_eq!(
        app2.patch_json(
            &config_path,
            json!({
                "model":{"mode":"parent"}, "instructions":"CHANGED-AFTER-CRASH",
                "tools":{"mode":"parent"}
            })
        )
        .await
        .status(),
        200
    );

    // A plain send must refuse to append past the unsettled delegation.
    let resp = app2.send_message(&conv, "hello again").await;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "pending_recovery"
    );

    // A closed tool batch from the preceding execution must survive replay.
    // A plain final assistant would instead exercise the completion shortcut.
    use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
    for message in [
        Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "before-restart-call".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            })],
        },
        Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "before-restart-call".into(),
                content: json!("before-restart"),
                is_error: false,
                images: vec![],
            })],
        },
    ] {
        oct_agent::db::messages::insert_message(
            &app2.pool,
            agent_id,
            &message,
            Some("mock-llm"),
            Some("snapshot-model"),
            None,
        )
        .await
        .unwrap();
    }
    let (recovered_tx, recovered_rx) = gate();

    // One resume: the child restarts on its ORIGINAL run row (epoch bumps),
    // its result lands exactly once, then the parent continues.
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("done-a-recovered")],
            gate: recovered_rx,
            post: vec![finish_chunk("stop")],
        }],
    );
    llm.push_turns(vec![LlmTurn::Complete(vec![
        text_chunk("parent final"),
        finish_chunk("stop"),
    ])]);
    let mut events2 = sse_payloads(app2.resume(&conv).await);
    next_event(&mut events2, updated("A", "running")).await;
    let mut child_events = sse_payloads(
        app2.get_json(&format!("/api/subagents/{agent_id}/events"))
            .await,
    );
    let ready = next_event(&mut child_events, |v| v["type"] == "run_meta").await;
    assert_eq!(ready["data"]["run_id"], initial_child["data"]["run_id"]);
    assert_eq!(ready["data"]["execution_epoch"], 2);
    next_event(&mut child_events, |v| v["type"] == "text_delta").await;
    let snapshot = app2
        .get_json(&format!("/api/subagents/{agent_id}/messages"))
        .await;
    let revision: i64 = snapshot.headers()["X-Context-Revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(snapshot.headers()["X-History-Epoch"], "0");
    let child_history: Vec<Value> = snapshot.json().await.unwrap();
    assert!(
        child_history
            .iter()
            .any(|m| m["parts"].to_string().contains("before-restart"))
    );
    assert_eq!(ready["data"]["replay_after_revision"], revision);
    assert!(
        child_history
            .iter()
            .all(|m| m["appended_at_revision"].as_i64().unwrap() <= revision)
    );
    recovered_tx.send(true).unwrap();
    drain_to_end(&mut child_events).await;

    next_event(&mut events2, updated("A", "completed")).await;
    next_event(&mut events2, |v| v["type"] == "finish").await;
    drain_to_end(&mut events2).await;

    let rows = subagent_rows(&app2.pool, &conv).await;
    assert_eq!(
        rows.len(),
        1,
        "the SAME run row is continued, not duplicated"
    );
    assert_eq!(rows[0].1, "completed");
    assert_eq!(rows[0].2, 2, "execution_epoch bumped by the recovery");

    // The parent received the result exactly once and its final turn saw it.
    let messages = history(&app2, &conv).await;
    let count = messages
        .iter()
        .filter(|m| {
            m["parts"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|p| p["type"] == "tool_result" && p["call_id"] == "call-a")
            })
        })
        .count();
    assert_eq!(count, 1, "the task result lands in the parent history once");
    let (content, is_error) = tool_result_of(&messages, "call-a").expect("result");
    assert!(!is_error);
    let envelope: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert_eq!(envelope["output"], "done-a-recovered");

    let resumed = oct_agent::db::runs::get_run(&app2.pool, &rows[0].0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.model_id.as_deref(), Some("snapshot-model"));
    assert_eq!(resumed.context_window, Some(54321));
    let requests = llm.requests();
    let recovered_child = requests
        .iter()
        .rev()
        .find(|r| user_role_text(r).contains("TASKA-7f3a"))
        .unwrap();
    assert_eq!(recovered_child["model"], "snapshot-model");
    assert!(recovered_child["tools"].is_null() || recovered_child["tools"] == json!([]));
    let system = recovered_child["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("ORIGINAL-SNAPSHOT-INSTRUCTIONS"));
    assert!(!system.contains("CHANGED-AFTER-CRASH"));
    let parent_final = requests
        .iter()
        .filter(|r| r["tools"].is_array())
        .find(|r| r["messages"].to_string().contains("done-a-recovered"))
        .expect("the parent's continued turn carried the task result");
    // The continued parent turn must also still offer the task tool: the
    // resumed run is a full root run, not a degraded one.
    assert!(
        parent_final["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "task"),
        "the resumed root run keeps the delegation tool"
    );
}

// --- 8. Recovery backfills a completed child without re-running it ----------

#[tokio::test]
async fn recovery_backfills_completed_child_without_rerunning() {
    let llm = spawn_llm_mock(vec![parent_single_task_turn("TASKA-7f3a")]).await;
    let (_gate_a, gate_a_rx) = gate();
    llm.push_marker_turns(
        "TASKA-7f3a",
        vec![LlmTurn::Gated {
            pre: vec![text_chunk("a-partial")],
            gate: gate_a_rx,
            post: vec![finish_chunk("stop")],
        }],
    );
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    next_event(&mut events, updated("A", "running")).await;

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&app.pool)
        .await
        .unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::copy(app.dir.path().join("test.db"), dir2.path().join("test.db")).unwrap();
    let app2 = TestApp::new_with_dir(dir2).await;

    // The child "completed" in the copy's past (e.g. it settled in a window
    // the parent never saw): settle its row by hand with a final message.
    let (child_run_id, child_conv): (String, String) = sqlx::query_as(
        "SELECT r.id, r.conversation_id FROM agent_runs r \
         JOIN conversations c ON c.id = r.conversation_id \
         WHERE c.root_conversation_id = $1 AND r.kind = 'subagent'",
    )
    .bind(&conv)
    .fetch_one(&app2.pool)
    .await
    .unwrap();
    let final_msg = oct_agent::db::messages::insert_message(
        &app2.pool,
        &child_conv,
        &oct_llm_provider::core::Message::text(
            oct_llm_provider::core::Role::Assistant,
            "child-final",
        ),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    oct_agent::db::runs::finish_run(
        &app2.pool,
        &child_run_id,
        oct_agent::db::runs::RunFinalStatus::Completed,
        Some(&final_msg.id),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Count CHILD model requests the way the mock routes them: by USER
    // message content. (The parent's continued request embeds the delegation
    // prompt inside the assistant tool_call's arguments — whole-body matching
    // would count it too.)
    let child_requests = |llm: &support::llm::LlmMock| {
        llm.requests()
            .iter()
            .filter(|r| user_role_text(r).contains("TASKA-7f3a"))
            .count()
    };
    let marker_requests_before = child_requests(&llm);
    llm.push_turns(vec![LlmTurn::Complete(vec![
        text_chunk("parent final"),
        finish_chunk("stop"),
    ])]);

    let mut events2 = sse_payloads(app2.resume(&conv).await);
    next_event(&mut events2, |v| v["type"] == "finish").await;
    drain_to_end(&mut events2).await;

    // No new child model request: the settled row IS the answer.
    let marker_requests_after = child_requests(&llm);
    assert_eq!(marker_requests_before, marker_requests_after);

    let messages = history(&app2, &conv).await;
    let (content, _) = tool_result_of(&messages, "call-a").expect("backfilled");
    let envelope: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
    assert_eq!(envelope["output"], "child-final");
}

// --- 9. Resume eligibility: lost run with a tool tail ------------------------

#[tokio::test]
async fn lost_run_with_tool_tail_is_resumable_but_completed_is_not() {
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("resumed-final"),
        finish_chunk("stop"),
    ])])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;

    // Seed an interrupted turn directly: user → assistant(tool_call) → tool
    // result. Every call is answered, the tail is a tool row — only the run
    // state distinguishes "crashed before the next request" from "finished".
    let pool = &app.pool;
    oct_agent::db::messages::insert_message(
        pool,
        &conv,
        &oct_llm_provider::core::Message::text(oct_llm_provider::core::Role::User, "run the check"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    oct_agent::db::messages::insert_message(
        pool,
        &conv,
        &oct_llm_provider::core::Message {
            role: oct_llm_provider::core::Role::Assistant,
            parts: vec![oct_llm_provider::core::ContentPart::ToolCall(
                oct_llm_provider::core::ToolCall {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            )],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    oct_agent::db::messages::insert_message(
        pool,
        &conv,
        &oct_llm_provider::core::Message {
            role: oct_llm_provider::core::Role::Tool,
            parts: vec![oct_llm_provider::core::ContentPart::ToolResult(
                oct_llm_provider::core::ToolResult {
                    call_id: "t1".into(),
                    content: serde_json::Value::String("ok".into()),
                    is_error: false,
                    images: vec![],
                },
            )],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let seed_run = |status: &'static str| {
        let pool = pool.clone();
        let conv = conv.clone();
        async move {
            let run = oct_agent::db::runs::create_run(
                &pool,
                oct_agent::db::runs::NewRun {
                    conversation_id: conv,
                    kind: "root",
                    parent_run_id: None,
                    root_run_id: None,
                    parent_tool_call_id: None,
                    provider_id: Some("mock-llm".into()),
                    model_id: Some("test-model".into()),
                    context_window: None,
                    description: None,
                },
            )
            .await
            .unwrap();
            if status != "running" {
                sqlx::query("UPDATE agent_runs SET status = $1 WHERE id = $2")
                    .bind(status)
                    .bind(&run.id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
    };

    // `lost`: resumable — the model is asked again and the turn completes.
    seed_run("lost").await;
    let mut events = sse_payloads(app.resume(&conv).await);
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;
    let messages = history(&app, &conv).await;
    let last_text = messages
        .iter()
        .rev()
        .find_map(|m| {
            m["parts"].as_array()?.iter().find_map(|p| {
                if p["type"] == "text" {
                    p["text"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
        })
        .expect("final assistant text");
    assert_eq!(last_text, "resumed-final");

    // `completed` with the SAME history shape: nothing to resume.
    seed_run("completed").await;
    let resp = app.resume(&conv).await;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["code"],
        "nothing_to_resume"
    );
}

// --- 10. Managed-conversation guards ----------------------------------------

#[tokio::test]
async fn managed_conversation_rejects_send_resume_compact() {
    let llm = spawn_llm_mock(vec![
        parent_single_task_turn("TASKA-7f3a"),
        LdmTurns::complete(vec![text_chunk("all done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns("TASKA-7f3a", vec![quick_child_turn("done-a")]);
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "please research").await);
    next_event(&mut events, updated("A", "completed")).await;
    next_event(&mut events, |v| v["type"] == "finish").await;
    drain_to_end(&mut events).await;

    let rows = subagent_rows(&app.pool, &conv).await;
    let child = &rows[0].3;

    let send = app.send_message(child, "direct send").await;
    assert_eq!(send.status(), 409);
    assert_eq!(
        send.json::<Value>().await.unwrap()["code"],
        "managed_conversation"
    );
    let resume = app.resume(child).await;
    assert_eq!(resume.status(), 409);
    assert_eq!(
        resume.json::<Value>().await.unwrap()["code"],
        "managed_conversation"
    );
    let compact = app.compact(child).await;
    assert_eq!(compact.status(), 409);
    assert_eq!(
        compact.json::<Value>().await.unwrap()["code"],
        "managed_conversation"
    );
}

// --- Shared turn builders (keep the long literals out of the tests) ---------

/// Turn builders namespaced behind a zero-sized marker type so they read
/// clearly at the call sites.
struct LdmTurns;

impl LdmTurns {
    /// A parent turn issuing ONE task call (call-a) for the marker's task.
    fn single(marker: &str) -> LlmTurn {
        LdmTurns::complete(vec![
            task_call("call-a", "A", &format!("Research {marker} and report.")),
            finish_chunk("tool_calls"),
        ])
    }

    fn complete(items: Vec<Value>) -> LlmTurn {
        LlmTurn::Complete(items)
    }

    /// A child that answers immediately with `text` and stops.
    fn quick_child(text: &str) -> LlmTurn {
        LdmTurns::complete(vec![text_chunk(text), finish_chunk("stop")])
    }
}

/// Root and child viewers share one approval identity. Decisions remain
/// scoped to that task tree, and persisted rules reach the live worker.
#[tokio::test]
async fn worker_approval_is_forwarded_scoped_and_can_be_decided_from_either_surface() {
    for decision in ["allow", "allow_always", "deny"] {
        let marker = "WORKER-APPROVAL";
        let llm = spawn_llm_mock(vec![
            parent_single_task_turn(marker),
            quick_child_turn("parent done"),
        ])
        .await;
        let mut child_turns = vec![LlmTurn::Complete(vec![
            tool_call_chunk(
                "worker-write",
                "write_file",
                &json!({"path":"approved-worker.txt","content":"child bytes"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ])];
        if decision == "allow_always" {
            child_turns.push(LlmTurn::Complete(vec![
                tool_call_chunk(
                    "worker-write-again",
                    "write_file",
                    &json!({"path":"approved-worker.txt","content":"after saved rule"}).to_string(),
                ),
                finish_chunk("tool_calls"),
            ]));
        }
        child_turns.push(quick_child_turn("child done"));
        llm.push_marker_turns(marker, child_turns);
        let (app, project, root) = wired_app(&llm).await;
        app.set_permission_mode(&project, &root, "default").await;
        let other = app.create_conversation(&project).await;
        let mut parent = sse_payloads(app.send_message(&root, "delegate").await);
        let requested =
            next_event(&mut parent, |event| event["type"] == "approval_requested").await;
        let agent = requested["data"]["agent_id"].as_str().unwrap();
        let approval = requested["data"]["approval_id"].as_str().unwrap();
        assert!(!app.dir.path().join("approved-worker.txt").exists());
        let pending: Vec<Value> = app
            .get_json(&format!("/api/conversations/{root}/approvals"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["agent_id"], agent);
        assert_eq!(pending[0]["approval_id"], approval);
        let mut child = sse_payloads(
            app.get_json(&format!("/api/subagents/{agent}/events"))
                .await,
        );
        let replay = next_event(&mut child, |event| event["type"] == "approval_requested").await;
        assert_eq!(replay, requested);
        assert_eq!(
            app.post_json(
                &format!("/api/conversations/{other}/approvals/{approval}"),
                json!({"decision":"allow"})
            )
            .await
            .status(),
            404
        );
        if decision == "allow_always" {
            // Both panels may click together: exactly one decision and one
            // persisted rule, while the next worker call uses that live rule.
            let parent_path = format!("/api/conversations/{root}/approvals/{approval}");
            let child_path = format!("/api/conversations/{agent}/approvals/{approval}");
            let (from_parent, from_child) = tokio::join!(
                app.post_json(&parent_path, json!({"decision":decision})),
                app.post_json(&child_path, json!({"decision":decision})),
            );
            let mut statuses = [from_parent.status().as_u16(), from_child.status().as_u16()];
            statuses.sort();
            assert_eq!(statuses, [200, 404]);
        } else {
            assert_eq!(
                app.post_json(
                    &format!("/api/conversations/{root}/approvals/{approval}"),
                    json!({"decision":decision})
                )
                .await
                .status(),
                200
            );
        }
        let resolution =
            next_event(&mut parent, |event| event["type"] == "approval_resolved").await;
        assert_eq!(resolution["data"]["agent_id"], agent);
        assert_eq!(
            resolution["data"]["outcome"],
            if decision == "deny" {
                "denied"
            } else {
                "approved"
            }
        );
        let child_resolution =
            next_event(&mut child, |event| event["type"] == "approval_resolved").await;
        assert_eq!(resolution, child_resolution);
        drain_to_end(&mut parent).await;
        drain_to_end(&mut child).await;
        assert_eq!(
            app.dir.path().join("approved-worker.txt").exists(),
            decision != "deny"
        );
        let pending: Vec<Value> = app
            .get_json(&format!("/api/conversations/{root}/approvals"))
            .await
            .json()
            .await
            .unwrap();
        assert!(pending.is_empty());
        assert_eq!(
            app.post_json(
                &format!("/api/conversations/{root}/approvals/{approval}"),
                json!({"decision":"allow"})
            )
            .await
            .status(),
            404
        );
        if decision == "allow_always" {
            let rules = oct_agent::db::permissions::list_rules_for_project(&app.pool, &project)
                .await
                .unwrap();
            assert_eq!(
                rules
                    .iter()
                    .filter(|rule| rule.tool == "write_file" && rule.source == "approval")
                    .count(),
                1
            );
            assert_eq!(
                std::fs::read_to_string(app.dir.path().join("approved-worker.txt")).unwrap(),
                "after saved rule"
            );
        }
    }
}

#[tokio::test]
async fn cancelling_a_waiting_worker_resolves_its_parent_approval_and_preserves_siblings() {
    let llm = spawn_llm_mock(vec![
        dual_task_parent_turn(),
        quick_child_turn("parent done"),
    ])
    .await;
    for marker in ["TASKA-7f3a", "TASKB-9c2e"] {
        llm.push_marker_turns(
            marker,
            vec![
                LlmTurn::Complete(vec![
                    tool_call_chunk(
                        marker,
                        "write_file",
                        &json!({"path":format!("{marker}.txt"),"content":"child"}).to_string(),
                    ),
                    finish_chunk("tool_calls"),
                ]),
                quick_child_turn("child done"),
            ],
        );
    }
    let (app, project, root) = wired_app(&llm).await;
    app.set_permission_mode(&project, &root, "default").await;
    let mut parent = sse_payloads(app.send_message(&root, "delegate both").await);
    let first = next_event(&mut parent, |event| event["type"] == "approval_requested").await;
    let second = next_event(&mut parent, |event| event["type"] == "approval_requested").await;
    let agent = first["data"]["agent_id"].as_str().unwrap();
    assert_eq!(
        app.post_json(&format!("/api/subagents/{agent}/cancel"), json!({}))
            .await
            .status(),
        200
    );
    let cancelled = next_event(&mut parent, |event| {
        event["type"] == "approval_resolved"
            && event["data"]["approval_id"] == first["data"]["approval_id"]
    })
    .await;
    assert_eq!(cancelled["data"]["outcome"], "cancelled");
    let pending: Vec<Value> = app
        .get_json(&format!("/api/conversations/{root}/approvals"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["approval_id"], second["data"]["approval_id"]);
    let approval = second["data"]["approval_id"].as_str().unwrap();
    assert_eq!(
        app.post_json(
            &format!("/api/conversations/{root}/approvals/{approval}"),
            json!({"decision":"allow"})
        )
        .await
        .status(),
        200
    );
    drain_to_end(&mut parent).await;
    let pending: Vec<Value> = app
        .get_json(&format!("/api/conversations/{root}/approvals"))
        .await
        .json()
        .await
        .unwrap();
    assert!(pending.is_empty());
}
