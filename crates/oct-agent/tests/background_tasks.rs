//! Background bash task e2e: the `execute_command`
//! `run_in_background` path over the full HTTP stack — envelope-immediate
//! tool answer, fd-direct-write output files, one-shot settlement, the
//! wake-run notification chain, the send-time catch-up after a simulated
//! crash, and the stop/cleanup surfaces. The LLM is the scripted
//! `LlmMock`; the background commands themselves are real `sh -c`
//! processes whose timing the tests control through the scripted turns.
//!
//! Waiting model: the wake run has no SSE subscriber the test holds, so
//! synchronization is "poll the LlmMock's captured requests until the
//! notification turn arrived" (bounded by [`wait_until`]'s deadline) — the
//! mock's request log IS the wake-run signal. Everything else waits on
//! SSE EOF (run settlement) as usual.

mod support;

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};

use sqlx::Row;
use support::llm::{LlmMock, LlmTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};
use support::{TestApp, sse_payloads};

// --- helpers ----------------------------------------------------------------

async fn wired_app(llm: &LlmMock) -> (TestApp, String, String) {
    let app = TestApp::new().await;
    app.register_llm_provider(llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    // This suite predates the permission layer and drives
    // `execute_command` end to end: pin the conversation to `bypass` so no
    // approval interaction enters the flows under test.
    app.set_permission_mode(&project, &conv, "bypass").await;
    (app, project, conv)
}

/// A scripted turn calling execute_command with run_in_background: true.
fn bg_tool_turn(command: &str) -> LlmTurn {
    LlmTurn::Complete(vec![
        tool_call_chunk(
            "call-bg",
            "execute_command",
            &json!({
                "command": command,
                "run_in_background": true,
            })
            .to_string(),
        ),
        finish_chunk("tool_calls"),
    ])
}

fn text_turn(text: &str) -> LlmTurn {
    LlmTurn::Complete(vec![text_chunk(text), finish_chunk("stop")])
}

/// Bounded retry-poll (the e2e analogue of the frontend's `expect.poll`):
/// evaluate the async probe until it produces a value or the deadline
/// passes. The wake run has no stream the test can await, so this is the
/// synchronization primitive for "the drainer did its job".
async fn wait_until<F, Fut, T>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next SSE payload matching `pred` (events of a stream already in
/// flight); bounded like the subagent tests' helper.
async fn next_event(
    stream: &mut futures_util::stream::BoxStream<'static, Value>,
    pred: impl Fn(&Value) -> bool,
) -> Value {
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
async fn drain_to_end(stream: &mut futures_util::stream::BoxStream<'static, Value>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("timed out draining SSE (run hung?)")
    {
        out.push(event);
    }
    out
}

/// The task id from an SSE tool_result envelope ("task_id: bg_...").
fn task_id_from_envelope(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "tool_result")
        .find_map(|e| {
            let content = e["data"]["content"].as_str()?;
            let line = content.lines().find(|l| l.starts_with("task_id: "))?;
            Some(line.trim_start_matches("task_id: ").to_string())
        })
        .expect("a tool_result envelope carrying the task id")
}

/// Whether one captured LLM request carries a user message containing
/// `marker` (notifications ride in user messages).
fn request_contains(requests: &[Value], marker: &str) -> bool {
    requests.iter().any(|body| {
        body["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"].to_string().contains(marker))
            })
            .unwrap_or(false)
    })
}

async fn background_task_row(pool: &sqlx::SqlitePool, task_id: &str) -> Option<Value> {
    sqlx::query(
        "SELECT id, status, exit_code, error, notified, output_bytes FROM background_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|row| {
        json!({
            "id": row.get::<String, _>("id"),
            "status": row.get::<String, _>("status"),
            "exit_code": row.get::<Option<i64>, _>("exit_code"),
            "error": row.get::<Option<String>, _>("error"),
            "notified": row.get::<i64, _>("notified"),
            "output_bytes": row.get::<Option<i64>, _>("output_bytes"),
        })
    })
}

// --- 1. The happy path: envelope, fd-write output, wake-run notification ----

#[tokio::test]
async fn background_task_completes_and_wakes_the_model() {
    let llm = spawn_llm_mock(vec![
        bg_tool_turn("echo hello-bg; sleep 1; echo world-bg"),
        text_turn("started it"),
        // The wake turn: served when the notification run starts.
        text_turn("bg-handled"),
    ])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;

    let events = app
        .read_sse(app.send_message(&conv, "start it").await)
        .await;
    assert!(
        events.iter().any(|e| e["type"] == "finish"),
        "run 1 must settle: {events:?}"
    );

    // The envelope answered the call immediately.
    let task_id = task_id_from_envelope(&events);
    assert!(task_id.starts_with("bg_"), "unexpected id {task_id}");
    assert!(events.iter().any(|e| {
        e["data"]["content"]
            .as_str()
            .is_some_and(|c| c.contains("Command started in the background"))
    }));

    // The output file fills while the command runs (fd-direct-write: the
    // interim output is on disk before the task settles).
    wait_until("interim output to land in the file", || async {
        let resp = app
            .get_json(&format!("/api/background-tasks/{task_id}/output"))
            .await;
        (resp.status() == 200
            && resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["content"].as_str().map(str::to_string))
                .is_some_and(|c| c.contains("hello-bg")))
        .then_some(())
    })
    .await;

    // Settlement + wake: the drainer inserts the notification (one user
    // message, tail inlined) and the wake turn consumes it.
    wait_until("the wake request carrying the notification", || async {
        let requests = llm.requests();
        request_contains(&requests, "<task-notification>").then(|| requests)
    })
    .await;
    let requests = llm.requests();
    let notification_user = requests
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect::<String>()
        .contains("<task-notification>");
    assert!(notification_user);
    let blob = requests
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .collect::<String>();
    assert!(blob.contains("exit_code: 0"), "{blob}");
    assert!(blob.contains(&task_id), "{blob}");
    assert!(blob.contains("world-bg"), "tail inlined: {blob}");

    // The task settled exactly once and is marked notified.
    let row = wait_until("settled row", || async {
        background_task_row(&app.pool, &task_id)
            .await
            .filter(|r| r["status"] != "running")
    })
    .await;
    assert_eq!(row["status"], "completed");
    assert_eq!(row["exit_code"], 0);
    assert_eq!(row["notified"], 1);

    // The REST list shows the settled task.
    let listed = app
        .get_json(&format!("/api/conversations/{conv}/background-tasks"))
        .await;
    assert_eq!(listed.status(), 200);
    let tasks: Vec<Value> = listed.json().await.unwrap();
    let entry = tasks
        .iter()
        .find(|t| t["id"] == task_id.as_str())
        .expect("task in the conversation list");
    assert_eq!(entry["status"], "completed");

    // The wake turn's answer persisted into the history.
    wait_until("the wake answer persisted", || async {
        let resp = app
            .get_json(&format!("/api/conversations/{conv}/messages"))
            .await;
        let body = resp.text().await.unwrap_or_default();
        body.contains("bg-handled").then_some(())
    })
    .await;
}

// --- 2. Run cancellation spares the background task --------------------------

#[tokio::test]
async fn cancelling_the_run_does_not_kill_the_background_task() {
    let marker = app_dir_marker("spared");
    let llm = spawn_llm_mock(vec![
        bg_tool_turn(&format!("sleep 1; echo survived > {}", marker.display())),
        // The run parks on a hanging turn; the test cancels it.
        LlmTurn::Hang(vec![text_chunk("thinking")]),
        // The wake turn once the cancelled run releases the conversation.
        text_turn("noted-after-cancel"),
    ])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;

    let mut events = sse_payloads(app.send_message(&conv, "start and stall").await);
    next_event(&mut events, |e| e["type"] == "tool_result").await;

    app.cancel(&conv).await;
    let tail = drain_to_end(&mut events).await;
    assert!(
        tail.iter().any(|e| e["type"] == "cancelled"),
        "run 1 cancelled: {tail:?}"
    );

    // The task outlives the cancelled run: its marker lands, the row
    // settles completed and the notification still wakes the model.
    wait_until("the spared task to finish", || async {
        std::fs::metadata(&marker).ok().map(|_| ())
    })
    .await;
    let requests = wait_until("the notification wake", || async {
        let requests = llm.requests();
        request_contains(&requests, "<task-notification>").then(|| requests)
    })
    .await;
    assert!(request_contains(&requests, "exit_code: 0"));
}

// --- 3. HTTP cancel: graceful kill + settled row + cancelled notification ----

#[tokio::test]
async fn http_cancel_settles_the_task_as_cancelled() {
    let llm = spawn_llm_mock(vec![
        bg_tool_turn("sleep 30"),
        text_turn("parked"),
        text_turn("cancel-noted"),
    ])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;

    let events = app
        .read_sse(app.send_message(&conv, "start long").await)
        .await;
    let task_id = task_id_from_envelope(&events);

    let resp = app
        .post_json(
            &format!("/api/background-tasks/{task_id}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let settled: Value = resp.json().await.unwrap();
    assert_eq!(settled["status"], "cancelled");

    // The cancelled completion still notifies (the envelope promised it).
    let requests = wait_until("the cancelled notification wake", || async {
        let requests = llm.requests();
        request_contains(&requests, "status: cancelled").then(|| requests)
    })
    .await;
    assert!(request_contains(&requests, &task_id));

    // Unknown ids are plain 404s.
    let resp = app
        .post_json("/api/background-tasks/bg_nope/cancel", json!({}))
        .await;
    assert_eq!(resp.status(), 404);
    let _ = conv;
}

// --- 4. Output watchdog: oversized output kills and fails the task -----------

#[tokio::test]
async fn output_watchdog_kills_runaway_output() {
    let llm = spawn_llm_mock(vec![
        bg_tool_turn("head -c 300 /dev/zero | tr '\\0' 'x'; sleep 30"),
        text_turn("parked"),
        text_turn("limit-noted"),
    ])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;
    // Shrink the cap so the 5s watchdog tick sees the (already written)
    // 300 bytes over a 256-byte limit.
    app.background.set_output_limit(256);

    let events = app
        .read_sse(app.send_message(&conv, "write a lot").await)
        .await;
    let task_id = task_id_from_envelope(&events);

    let row = wait_until("the watchdog to settle the task", || async {
        background_task_row(&app.pool, &task_id)
            .await
            .filter(|r| r["status"] == "failed")
    })
    .await;
    let error = row["error"].as_str().unwrap_or_default();
    assert!(error.contains("1 GiB"), "error names the limit: {error}");
}

// --- 5. Crash: restart marks lost; the send catch-up delivers ---------------

#[tokio::test]
async fn restart_marks_lost_and_send_catch_up_notifies() {
    let llm = spawn_llm_mock(vec![
        bg_tool_turn("sleep 30"),
        text_turn("started"),
        // The turn served to the post-restart send (after the catch-up
        // message was prepended before the user's "hello").
        text_turn("after-restart"),
    ])
    .await;
    let (app, _project, conv) = wired_app(&llm).await;

    let events = app
        .read_sse(app.send_message(&conv, "start long").await)
        .await;
    let task_id = task_id_from_envelope(&events);

    // Simulate the crash: checkpoint the WAL, copy the db, boot a NEW app
    // over the copy. Its startup sweep marks the interrupted task `lost`.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&app.pool)
        .await
        .unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::copy(app.dir.path().join("test.db"), dir2.path().join("test.db")).unwrap();
    // The provider row (pointing at the still-running LlmMock) came over
    // with the copied database — no re-registration.
    let app2 = TestApp::new_with_dir(dir2).await;

    let row = background_task_row(&app2.pool, &task_id)
        .await
        .expect("row survived the copy");
    assert_eq!(row["status"], "lost", "bootstrap swept the running row");

    // The plain send flushes the lost notification BEFORE the user's
    // message: one request carries both.
    let resp = app2.send_message(&conv, "hello").await;
    assert_eq!(resp.status(), 200, "the catch-up send must start a run");
    let requests = wait_until("the catch-up request", || async {
        let requests = llm.requests();
        request_contains(&requests, "interrupted by a backend restart").then(|| requests)
    })
    .await;
    assert!(request_contains(&requests, &task_id));
    assert!(request_contains(&requests, "hello"));

    // Delivered exactly once: notified flipped in the same transaction.
    let row = background_task_row(&app2.pool, &task_id).await.unwrap();
    assert_eq!(row["notified"], 1);
}

// --- 6. Conversation deletion stops tasks and cleans the output dir ---------

#[tokio::test]
async fn deleting_the_conversation_cancels_tasks_and_cleans_output() {
    let llm = spawn_llm_mock(vec![bg_tool_turn("sleep 30"), text_turn("parked")]).await;
    let (app, project, conv) = wired_app(&llm).await;

    let events = app
        .read_sse(app.send_message(&conv, "start long").await)
        .await;
    let task_id = task_id_from_envelope(&events);
    let exec_dir = app.dir.path().join("exec").join(&conv);
    assert!(exec_dir.is_dir(), "per-conversation output dir exists");

    let resp = app
        .http_delete(&format!("/api/projects/{project}/conversations/{conv}"))
        .await;
    assert_eq!(resp.status(), 204);

    // Rows cascade with the conversation; the output dir is removed with
    // the (bounded) cancellation of the live task.
    wait_until("rows cascaded and output cleaned", || async {
        (background_task_row(&app.pool, &task_id).await.is_none() && !exec_dir.exists())
            .then_some(())
    })
    .await;
}

// --- 7. Generic output reading, task_stop and the worker background gate --

/// Direct tool calls (no LLM): generic output reading and the subagent
/// gate of execute_command.
#[tokio::test]
async fn output_files_are_readable_with_generic_tools_and_stop_still_works() {
    use oct_agent::agent::EventHub;
    use oct_agent::tools::execute::{ExecuteBackground, ExecuteCommandTool};
    use oct_agent::tools::task_stop::TaskStopTool;
    use oct_agent::tools::{AgentTool, ToolContext};
    use tokio_util::sync::CancellationToken;

    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let ctx = ToolContext::new(
        "call-direct",
        Arc::new(EventHub::new()),
        CancellationToken::new(),
    );
    // parent_run_id carries a REAL FK: create the run row the way a root
    // run would.
    let run = oct_agent::db::runs::create_run(
        &app.pool,
        oct_agent::db::runs::NewRun {
            conversation_id: conv.clone(),
            kind: "root",
            parent_run_id: None,
            root_run_id: None,
            parent_tool_call_id: None,
            provider_id: None,
            model_id: None,
            context_window: None,
            description: None,
        },
    )
    .await
    .unwrap();

    // Start a task directly through the manager (what the execute branch
    // does after validation).
    let task = app
        .background
        .start_bash(oct_agent::runtime::background::BashStart {
            conversation_id: conv.clone(),
            parent_run_id: Some(run.id.clone()),
            parent_call_id: "call-direct".into(),
            command: "echo alpha; sleep 1; echo omega".into(),
            cwd: app.dir.path().to_path_buf(),
        })
        .await
        .unwrap();

    if let Some((mut done, _)) = app.background.active_task(&task.id) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !*done.borrow_and_update() {
                done.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    let roots =
        oct_agent::tools::workspace::WorkspaceRoots::new(app.dir.path().to_path_buf(), vec![])
            .with_read_only_root(app.background.exec_dir().to_path_buf());
    let read = oct_agent::tools::read::ReadFileTool::new(
        roots.clone(),
        Arc::new(oct_agent::tools::shared::AgentReadState::new()),
    );
    let out = read
        .execute(
            json!({"path": task.output_path, "start_line": 2, "end_line": 2}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("omega"));
    assert!(!out.content.contains("alpha"));
    let grep = oct_agent::tools::grep::GrepTool::new(roots.clone());
    let out = grep
        .execute(json!({"path": task.output_path, "pattern": "alpha"}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("alpha"));

    // task_stop on a settled task reports the already-exited status.
    let stop_tool = TaskStopTool::new(app.background.clone(), conv.clone());
    let out = stop_tool
        .execute(json!({"task_id": task.id}), &ctx)
        .await
        .unwrap();
    assert!(out.content.contains("already exited"), "{}", out.content);

    // task_stop on a LIVE task: graceful kill, settled as cancelled.
    let live = app
        .background
        .start_bash(oct_agent::runtime::background::BashStart {
            conversation_id: conv.clone(),
            parent_run_id: None,
            parent_call_id: "call-direct-2".into(),
            command: "sleep 30".into(),
            cwd: app.dir.path().to_path_buf(),
        })
        .await
        .unwrap();
    let out = stop_tool
        .execute(json!({"task_id": live.id}), &ctx)
        .await
        .unwrap();
    assert!(
        out.content.contains("stopped (status: cancelled)"),
        "{}",
        out.content
    );

    // The subagent gate: a plain (background-less) execute tool refuses
    // run_in_background — the schema stays identical, dispatch is closed.
    let plain = ExecuteCommandTool::new(oct_agent::tools::workspace::WorkspaceRoots::new(
        app.dir.path().to_path_buf(),
        Vec::new(),
    ));
    let out = plain
        .execute(
            json!({"command": "echo x", "run_in_background": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(
        out.content.contains("not available in this run"),
        "{}",
        out.content
    );

    // The background-enabled instance on the same conversation works.
    let enabled = ExecuteCommandTool::with_background(
        oct_agent::tools::workspace::WorkspaceRoots::new(app.dir.path().to_path_buf(), Vec::new()),
        ExecuteBackground {
            manager: app.background.clone(),
            conversation_id: conv.clone(),
            parent_run_id: run.id.clone(),
        },
    );
    let out = enabled
        .execute(
            json!({"command": "echo gated", "run_in_background": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("task_id: bg_"), "{}", out.content);
    assert!(out.content.contains("output_file: "), "{}", out.content);
    assert!(!out.content.contains("task_output"));
}

// --- misc ---------------------------------------------------------------------

/// A unique-enough marker file path under the system temp dir.
fn app_dir_marker(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "oct-bg-{tag}-{}-{}",
        std::process::id(),
        uuid_marker()
    ))
}

fn uuid_marker() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::SeqCst).to_string()
}

#[tokio::test]
async fn runtime_output_is_a_read_only_root_outside_the_project_workspace() {
    let llm = spawn_llm_mock(vec![
        bg_tool_turn("printf 'alpha\\nomega\\n'"),
        text_turn("started"),
        text_turn("notified"),
    ])
    .await;
    let (app, project, conv) = wired_app(&llm).await;
    let workspace = tempfile::tempdir().unwrap();
    assert_eq!(
        app.patch_json(
            &format!("/api/projects/{project}"),
            json!({"working_dir":workspace.path()})
        )
        .await
        .status(),
        200
    );
    let events = app.read_sse(app.send_message(&conv, "start").await).await;
    let task = task_id_from_envelope(&events);
    wait_until("wake run finished", || async {
        let messages: Vec<Value> = app
            .get_json(&format!("/api/conversations/{conv}/messages"))
            .await
            .json()
            .await
            .unwrap();
        let run: Value = app
            .get_json(&format!("/api/conversations/{conv}/run"))
            .await
            .json()
            .await
            .unwrap();
        (messages.iter().any(|message| {
            message["role"] == "assistant"
                && message["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|part| part["type"] == "text" && part["text"] == "notified")
        }) && run["running"] == false)
            .then_some(())
    })
    .await;
    let tasks: Vec<Value> = app
        .get_json(&format!("/api/conversations/{conv}/background-tasks"))
        .await
        .json()
        .await
        .unwrap();
    let path = tasks.iter().find(|row| row["id"] == task).unwrap()["output_path"]
        .as_str()
        .unwrap();
    assert!(!std::path::Path::new(path).starts_with(workspace.path()));
    let envelope = events
        .iter()
        .find(|event| event["type"] == "tool_result")
        .unwrap()["data"]["content"]
        .as_str()
        .unwrap();
    assert!(envelope.contains(&format!("output_file: {path}")));
    let turn = |id: &str, tool: &str, args: Value| {
        LlmTurn::Complete(vec![
            tool_call_chunk(id, tool, &args.to_string()),
            finish_chunk("tool_calls"),
        ])
    };
    llm.push_turns([
        turn(
            "read",
            "read_file",
            json!({"path":path,"start_line":2,"end_line":2}),
        ),
        turn("grep", "grep", json!({"path":path,"pattern":"alpha"})),
        turn(
            "glob",
            "glob",
            json!({"path":std::path::Path::new(path).parent(),"pattern":"*.log"}),
        ),
        text_turn("read done"),
    ]);
    app.set_permission_mode(&project, &conv, "default").await;
    let reads = app
        .read_sse(app.send_message(&conv, "read output").await)
        .await;
    assert!(
        !reads
            .iter()
            .any(|event| event["type"] == "approval_requested")
    );
    for (id, text) in [
        ("read", "omega"),
        ("grep", "alpha"),
        ("glob", task.as_str()),
    ] {
        let result = reads
            .iter()
            .find(|event| event["type"] == "tool_result" && event["data"]["call_id"] == id)
            .unwrap_or_else(|| panic!("missing {id}: {reads:?}"));
        assert_eq!(result["data"]["is_error"], false, "{result}");
        assert!(
            result["data"]["content"].as_str().unwrap().contains(text),
            "{result}"
        );
    }
    // Even bypass cannot modify the runtime's files after reading them.
    app.set_permission_mode(&project, &conv, "bypass").await;
    llm.push_turns([
        turn(
            "write",
            "write_file",
            json!({"path":path,"content":"tampered"}),
        ),
        turn(
            "edit",
            "edit_file",
            json!({"file_path":path,"old_string":"alpha","new_string":"tampered"}),
        ),
        text_turn("write refused"),
    ]);
    let writes = app
        .read_sse(app.send_message(&conv, "attempt writes").await)
        .await;
    for id in ["write", "edit"] {
        let result = writes
            .iter()
            .find(|event| event["type"] == "tool_result" && event["data"]["call_id"] == id)
            .unwrap();
        assert_eq!(result["data"]["is_error"], true);
        assert!(
            result["data"]["content"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), "alpha\nomega\n");
    for request in llm.requests() {
        assert!(
            !request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == "task_output")
        );
    }
}
