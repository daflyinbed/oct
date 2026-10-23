//! Combined crash-recovery regressions over the production HTTP/DB/LLM path.
//! Model requests are served only by the local scripted mock.
mod support;

use std::collections::HashSet;

use oct_agent::db::{background as bg, conversations, messages as msg, runs};
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall};
use serde_json::{Value, json};
use support::TestApp;
use support::llm::{LlmMock, LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};

async fn app(replies: &[&str]) -> (TestApp, LlmMock, String, String) {
    let llm = spawn_llm_mock(
        replies
            .iter()
            .map(|text| LlmTurn::Complete(vec![text_chunk(text), finish_chunk("stop")]))
            .collect(),
    )
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    (app, llm, project, conv)
}

async fn assert_root_assembly_failure_settles_run(table: &str) {
    let (app, llm, _project, conv) = app(&["resumed after assembly failure"]).await;
    let (hide, restore) = match table {
        "attachment_assets" => (
            "ALTER TABLE attachment_assets RENAME TO unavailable_table",
            "ALTER TABLE unavailable_table RENAME TO attachment_assets",
        ),
        "permission_rules" => (
            "ALTER TABLE permission_rules RENAME TO unavailable_table",
            "ALTER TABLE unavailable_table RENAME TO permission_rules",
        ),
        _ => panic!("unsupported fault injection: {table}"),
    };
    // These reads happen after the user message and running row are durable,
    // but before a supervisor owns the run. Restore the table without a
    // bootstrap sweep so the assertions observe startup's own settlement.
    sqlx::query(hide).execute(&app.pool).await.unwrap();
    let response = app.send_message(&conv, "durable request").await;
    sqlx::query(restore).execute(&app.pool).await.unwrap();
    assert_eq!(response.status(), 500);

    let failed = runs::latest_run_for_conversation(&app.pool, &conv)
        .await
        .unwrap()
        .expect("assembly failure must retain the registered run");
    assert_eq!(failed.status, "failed");
    assert!(failed.finished_at.is_some());
    assert!(failed.error.as_deref().unwrap().contains(table));
    assert!(!app.state.sessions.contains_key(&conv));
    assert!(llm.requests().is_empty(), "the agent loop never started");
    assert_eq!(msg::count_user_messages(&app.pool, &conv).await.unwrap(), 1);

    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    assert!(events.iter().any(|event| event["type"] == "finish"));
    assert_eq!(llm.requests().len(), 1);
    assert_eq!(msg::count_user_messages(&app.pool, &conv).await.unwrap(), 1);
    let resumed = runs::latest_run_for_conversation(&app.pool, &conv)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(resumed.id, failed.id);
    assert_eq!(resumed.status, "completed");
    assert_eq!(
        runs::get_run(&app.pool, &failed.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
}

#[tokio::test]
async fn root_attachment_assembly_failure_settles_run_and_allows_resume() {
    assert_root_assembly_failure_settles_run("attachment_assets").await;
}

#[tokio::test]
async fn root_permission_assembly_failure_settles_run_and_allows_resume() {
    assert_root_assembly_failure_settles_run("permission_rules").await;
}

async fn seed_call(app: &TestApp, conv: &str, name: &str, call_id: &str) {
    msg::insert_message(
        &app.pool,
        conv,
        &Message::text(Role::User, "original request"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    let message = Message {
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCall {
            id: call_id.into(),
            name: name.into(),
            arguments: "{}".into(),
        })],
    };
    msg::insert_message(&app.pool, conv, &message, None, None, None)
        .await
        .unwrap();
}

async fn seed_notification(app: &TestApp, conv: &str) {
    bg::create_task(
        &app.pool,
        bg::NewBackgroundTask {
            id: "bg_recovery_probe".into(),
            conversation_id: conv.into(),
            parent_run_id: None,
            parent_call_id: "older-background-call".into(),
            command: "true".into(),
            working_dir: None,
            output_path: app.dir.path().join("output.log").display().to_string(),
        },
    )
    .await
    .unwrap();
    bg::finish_task(
        &app.pool,
        "bg_recovery_probe",
        bg::TaskFinalStatus::Completed,
        Some(0),
        None,
        0,
    )
    .await
    .unwrap();
}

async fn seed_lost_child(
    app: &TestApp,
    project: &str,
    conv: &str,
    child_provider: &str,
) -> runs::AgentRun {
    seed_call(app, conv, "task", "pending-task").await;
    let parent = runs::create_run(
        &app.pool,
        runs::NewRun {
            conversation_id: conv.into(),
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
    let mut tx = app.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let child = conversations::create_subagent_conversation_in_tx(
        &mut tx,
        conversations::NewSubagentConversation {
            project_id: project,
            parent_conversation_id: conv,
            root_conversation_id: conv,
            agent_type: "worker",
            title: "recovery probe",
        },
    )
    .await
    .unwrap();
    msg::insert_message_in_tx(
        &mut tx,
        &child.id,
        &Message::text(Role::User, "child delegation"),
        Some(child_provider),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    let run = runs::create_run_in_tx(
        &mut tx,
        runs::NewRun {
            conversation_id: child.id,
            kind: "subagent",
            parent_run_id: Some(parent.id.clone()),
            root_run_id: Some(parent.id),
            parent_tool_call_id: Some("pending-task".into()),
            provider_id: Some(child_provider.into()),
            model_id: Some("test-model".into()),
            context_window: None,
            description: Some("recovery probe".into()),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    oct_agent::runtime::bootstrap(&app.pool).await.unwrap();
    run
}

/// Validate the wire contract that permissive model mocks otherwise miss:
/// every assistant's complete tool batch must precede the next user message.
fn assert_closed_batches_before_notification(request: &Value) {
    let messages = request["messages"].as_array().unwrap();
    let mut pending = HashSet::new();
    let mut saw_notification = false;
    for message in messages {
        if let Some(calls) = message["tool_calls"].as_array() {
            assert!(pending.is_empty(), "a new batch interrupted {pending:?}");
            pending.extend(calls.iter().map(|call| call["id"].as_str().unwrap()));
        } else if message["role"] == "tool" {
            let call = message["tool_call_id"].as_str().unwrap();
            assert!(pending.remove(call), "orphan/duplicate result {call}");
        } else {
            assert!(pending.is_empty(), "non-tool message split {pending:?}");
            saw_notification |= message["content"]
                .as_str()
                .is_some_and(|text| text.contains("<task-notification>"));
        }
    }
    assert!(pending.is_empty());
    assert!(
        saw_notification,
        "the pending notification must reach the model"
    );
}

#[tokio::test]
async fn send_persists_its_client_receipt_without_exposing_it_to_the_model() {
    let (app, llm, _project, conv) = app(&["accepted"]).await;
    let receipt = "client-send-receipt";
    let response = app
        .post_json(
            &format!("/api/conversations/{conv}/messages"),
            json!({
                "content": "request with receipt",
                "provider_spec": "mock-llm:test-model",
                "client_request_id": receipt,
            }),
        )
        .await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let history: Value = app
        .get_json(&format!("/api/conversations/{conv}/messages"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(history[0]["details"]["extra"]["client_request_id"], receipt);
    assert!(!llm.requests()[0].to_string().contains(receipt));
}

#[tokio::test]
async fn send_repairs_tool_batch_before_background_notification() {
    let (app, llm, _project, conv) = app(&["continued"]).await;
    seed_call(&app, &conv, "execute_command", "pending-command").await;
    seed_notification(&app, &conv).await;

    let response = app.send_message(&conv, "next request").await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert_closed_batches_before_notification(&requests[0]);
    assert!(
        msg::find_dangling_tool_calls(&app.pool, &conv)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn resume_repairs_ordinary_batch_before_background_notification() {
    let (app, llm, _project, conv) = app(&["resumed"]).await;
    seed_call(&app, &conv, "execute_command", "pending-command").await;
    seed_notification(&app, &conv).await;

    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert_closed_batches_before_notification(&requests[0]);
}

#[tokio::test]
async fn resume_backfills_managed_batch_before_background_notification() {
    let (app, llm, project, conv) = app(&["child recovered", "parent resumed"]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    seed_notification(&app, &conv).await;

    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;

    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_closed_batches_before_notification(&requests[1]);
    let settled = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "completed");
    assert_eq!(settled.execution_epoch, 2);
}

#[tokio::test]
async fn missing_child_provider_keeps_lost_run_cancellable_and_notification_pending() {
    let (app, llm, project, conv) = app(&["continued after child cancellation"]).await;
    let child = seed_lost_child(&app, &project, &conv, "removed-provider").await;
    seed_notification(&app, &conv).await;

    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    assert!(events.iter().any(|event| event["type"] == "error"));
    let after = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(after.status, "lost");
    assert_eq!(
        after.execution_epoch, 1,
        "preparation must not claim execution"
    );
    assert!(llm.requests().is_empty());
    let pending = bg::list_pending_notifications(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "an unfinished task batch cannot receive user notifications"
    );

    let cancelled = app
        .post_json(
            &format!("/api/subagents/{}/cancel", child.conversation_id),
            Value::Null,
        )
        .await;
    assert_eq!(cancelled.status(), 200);
    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert_closed_batches_before_notification(&requests[0]);
    assert_eq!(
        runs::get_run(&app.pool, &child.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
}

#[tokio::test]
async fn unowned_running_child_can_be_cancelled_without_restarting_the_server() {
    let (app, llm, project, conv) = app(&[]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    let epoch = runs::bump_execution_epoch(&app.pool, &child.id)
        .await
        .unwrap()
        .unwrap();
    // This is the original ghost state: the parent already answered its call,
    // leaving no dangling delegation for resume to find.
    msg::answer_tool_call_if_absent(&app.pool, &conv, "pending-task", "cancelled", true, None)
        .await
        .unwrap();
    assert!(!app.state.sessions.contains_key(&child.conversation_id));
    assert!(!app.state.subagents.is_active(&child.conversation_id));
    let response = app
        .post_json(
            &format!("/api/subagents/{}/cancel", child.conversation_id),
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), 200);
    let settled = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "cancelled");
    assert_eq!(settled.execution_epoch, epoch);
    assert!(settled.finished_at.is_some());
    assert!(!app.state.sessions.contains_key(&child.conversation_id));
    assert!(
        msg::find_dangling_tool_calls(&app.pool, &conv)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(llm.requests().is_empty());
    let response = app
        .post_json(
            &format!("/api/subagents/{}/cancel", child.conversation_id),
            Value::Null,
        )
        .await;
    assert_eq!(
        response.status(),
        404,
        "terminal runs are not cancellable again"
    );
}

#[tokio::test]
async fn resume_recovers_an_unowned_running_child_on_the_same_row() {
    let (app, llm, project, conv) = app(&["child recovered", "parent continued"]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    let epoch = runs::bump_execution_epoch(&app.pool, &child.id)
        .await
        .unwrap()
        .unwrap();
    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let settled = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "completed");
    assert_eq!(settled.execution_epoch, epoch + 1);
    assert_eq!(
        runs::list_subagent_runs(&app.pool, &conv)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(llm.requests().len(), 2);
    assert!(
        msg::find_dangling_tool_calls(&app.pool, &conv)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn child_cancel_reaches_recovery_assembly_before_supervisor_takeover() {
    use futures_util::StreamExt;
    let (app, llm, project, conv) = app(&[]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    // Recovery resolves the tree's prompt under this real application lock.
    // Keep assembly waiting while HTTP cancellation remains able to use DB.
    let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    app.resources
        .prompt_env_locks
        .insert(conv.clone(), lock.clone());
    let guard = lock.lock().await;
    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    let mut events = support::sse_payloads(response);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.next().await {
            if event["type"] == "subagent_updated" && event["data"]["status"] == "running" {
                return;
            }
        }
        panic!("child never entered assembly");
    })
    .await
    .unwrap();
    let active = app
        .state
        .subagents
        .child_by_agent(&child.conversation_id)
        .unwrap();
    assert!(
        active.hub.upgrade().unwrap().snapshot().is_empty(),
        "no replay metadata before preparation finishes"
    );
    let response = app
        .post_json(
            &format!("/api/subagents/{}/cancel", child.conversation_id),
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), 200);
    assert!(
        !app.state.sessions.get(&conv).unwrap().cancel.is_cancelled(),
        "child cancellation must not cancel its parent"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while events.next().await.is_some() {}
    })
    .await
    .expect("cancellation must settle without releasing the prompt lock");
    drop(guard);
    let settled = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "cancelled");
    assert_eq!(settled.execution_epoch, 2);
    assert!(!app.state.subagents.is_active(&child.conversation_id));
    assert!(!app.state.sessions.contains_key(&child.conversation_id));
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn assembly_read_failure_settles_child_and_releases_its_startup_handle() {
    let (app, llm, project, conv) = app(&[]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    sqlx::query("ALTER TABLE attachment_assets RENAME TO unavailable_attachment_assets")
        .execute(&app.pool)
        .await
        .unwrap();
    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    sqlx::query("ALTER TABLE unavailable_attachment_assets RENAME TO attachment_assets")
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(events.iter().any(|event| event["type"] == "error"));
    let settled = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "failed");
    assert_eq!(settled.execution_epoch, 2);
    assert!(settled.error.unwrap().contains("attachment_assets"));
    assert!(!app.state.subagents.is_active(&child.conversation_id));
    assert!(!app.state.sessions.contains_key(&child.conversation_id));
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn failure_after_execution_claim_settles_child_and_parent_can_retry() {
    let (app, llm, project, conv) = app(&["continued after startup failure"]).await;
    let child = seed_lost_child(&app, &project, &conv, "mock-llm").await;
    seed_call(
        &app,
        &child.conversation_id,
        "execute_command",
        "child-command",
    )
    .await;
    // The read-only preparation succeeds. Only repair's post-claim durable
    // append fails, exercising cleanup before a supervisor owns the child.
    sqlx::query(
        "CREATE TRIGGER reject_tool_repair BEFORE INSERT ON messages WHEN NEW.role = 'tool' BEGIN SELECT RAISE(FAIL, 'injected tool repair failure'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    assert!(events.iter().any(|event| event["type"] == "error"));
    let after = runs::get_run(&app.pool, &child.id).await.unwrap().unwrap();
    assert_eq!(after.status, "failed");
    assert_eq!(after.execution_epoch, 2);
    assert!(
        after
            .error
            .unwrap()
            .contains("injected tool repair failure")
    );
    assert!(llm.requests().is_empty());
    assert_eq!(
        app.get_json(&format!("/api/subagents/{}/events", child.conversation_id))
            .await
            .status(),
        404
    );

    sqlx::query("DROP TRIGGER reject_tool_repair")
        .execute(&app.pool)
        .await
        .unwrap();
    let response = app.resume(&conv).await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let requests = llm.requests();
    assert_eq!(
        requests.len(),
        1,
        "a failed startup is backfilled, never rerun"
    );
    let result = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "pending-task")
        .unwrap();
    let envelope: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(envelope["status"], json!("failed"));
}

async fn assert_cancel_during_mcp_reconciliation(resume: bool) {
    use std::sync::Arc;
    use std::time::Duration;

    let (app, llm, _project, conv) = app(&[]).await;
    if resume {
        seed_call(&app, &conv, "execute_command", "pending-command").await;
    }
    let requested = Arc::new(tokio::sync::Notify::new());
    let notify = requested.clone();
    let router = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move || {
            let notify = notify.clone();
            async move {
                notify.notify_one();
                std::future::pending::<axum::http::StatusCode>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let created = app
        .post_json(
            "/api/mcp/servers",
            json!({"name":"hanging-recovery-probe","transport":"http","url":mcp_url,"enabled":true}),
        )
        .await;
    assert_eq!(created.status(), 201);

    let suffix = if resume { "resume" } else { "messages" };
    let url = format!("{}/api/conversations/{conv}/{suffix}", app.base_url);
    let sending = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .json(&json!({"content":"new request","provider_spec":"mock-llm:test-model"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), requested.notified())
        .await
        .expect("the run reached the hanging MCP handshake");

    app.cancel(&conv).await;
    let response = tokio::time::timeout(Duration::from_secs(5), sending)
        .await
        .expect("cancelling MCP preparation must release the request")
        .unwrap();
    assert_eq!(response.status(), 200);
    let events = app.read_sse(response).await;
    assert!(events.iter().any(|event| event["type"] == "cancelled"));
    let status: Value = app
        .get_json(&format!("/api/conversations/{conv}/run"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["running"], false);
    assert_eq!(status["resumable"], true);
    assert!(
        llm.requests().is_empty(),
        "cancelled preparation must not call the model"
    );
    server.abort();
}

#[tokio::test]
async fn send_cancellation_releases_hanging_mcp_preparation() {
    assert_cancel_during_mcp_reconciliation(false).await;
}

#[tokio::test]
async fn resume_cancellation_releases_hanging_mcp_preparation() {
    assert_cancel_during_mcp_reconciliation(true).await;
}
