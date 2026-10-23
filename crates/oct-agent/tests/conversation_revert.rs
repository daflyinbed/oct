mod support;
use futures_util::StreamExt;

use oct_agent::db::conversations;
use oct_agent::db::history::RevertResponse;
use oct_agent::db::messages;
use oct_agent::db::runs;
use oct_agent::history::HistoryCut as ForkCut;
use oct_agent::history::HistorySide as ForkSide;
use oct_agent::history::RevertRequest;
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
use serde_json::{Value, json};
use support::{
    TestApp,
    llm::{self, LlmTurn, finish_chunk, text_chunk},
    sse_payloads,
};

async fn append(app: &TestApp, id: &str, role: Role, text: &str) -> messages::StoredMessage {
    messages::insert_message(
        &app.pool,
        id,
        &Message::text(role, text),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap()
}
async fn snapshot(app: &TestApp, id: &str) -> (i64, Vec<Value>) {
    let res = app
        .get_json(&format!("/api/conversations/{id}/messages"))
        .await;
    assert_eq!(res.status(), 200);
    let revision = res.headers()["X-Context-Revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    (revision, res.json().await.unwrap())
}
fn request(revision: i64, id: &str, side: ForkSide) -> RevertRequest {
    RevertRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        expected_revision: revision,
        cut: ForkCut::Message {
            message_id: id.into(),
            side,
        },
    }
}
async fn successful_revert(app: &TestApp, id: &str, body: &RevertRequest) -> RevertResponse {
    let res = app
        .post_json(&format!("/api/conversations/{id}/revert"), body)
        .await;
    if res.status() != 200 {
        panic!("{}", res.text().await.unwrap());
    }
    res.json().await.unwrap()
}
async fn assert_error(res: reqwest::Response, status: u16, code: &str) {
    assert_eq!(res.status(), status);
    assert_eq!(res.json::<Value>().await.unwrap()["code"], code);
}
fn new_run(id: &str) -> runs::NewRun {
    runs::NewRun {
        conversation_id: id.into(),
        kind: "root",
        parent_run_id: None,
        root_run_id: None,
        parent_tool_call_id: None,
        provider_id: Some("mock-llm".into()),
        model_id: Some("test-model".into()),
        context_window: None,
        description: None,
    }
}

#[tokio::test]
async fn before_user_preserves_identity_backup_retry_and_read_only_contract() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("new answer"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    app.set_permission_mode(&project, &id, "accept_edits").await;
    let first = append(&app, &id, Role::User, "first").await;
    append(&app, &id, Role::Assistant, "first answer").await;
    let target = append(&app, &id, Role::User, "edit me").await;
    append(&app, &id, Role::Assistant, "discard me").await;
    let (revision, original) = snapshot(&app, &id).await;
    let body = request(revision, &target.id, ForkSide::Before);
    let result = successful_revert(&app, &id, &body).await;
    assert_eq!(result.conversation.id, id);
    assert_eq!(result.conversation.title, "e2e");
    assert_eq!(result.conversation.permission_mode, "accept_edits");
    assert_eq!(result.conversation.history_epoch, 1);
    assert_eq!(result.context_revision, revision + 1);
    assert_eq!(result.draft.as_deref(), Some("edit me"));
    assert_eq!(result.backup.origin_kind, "revert_backup");
    assert_eq!(
        result.backup.backup_of_conversation_id.as_deref(),
        Some(id.as_str())
    );
    let (current_rev, current) = snapshot(&app, &id).await;
    assert_eq!(current_rev, revision + 1);
    assert_eq!(current.len(), 2);
    let (backup_rev, backup) = snapshot(&app, &result.backup.id).await;
    assert_eq!(backup_rev, revision);
    assert_eq!(
        backup.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        original.iter().map(|r| r["id"].clone()).collect::<Vec<_>>()
    );
    assert!(backup.iter().all(|r| r["inherited"] == true));
    assert_eq!(
        successful_revert(&app, &id, &body).await.backup.id,
        result.backup.id
    );
    assert_eq!(
        conversations::list_conversations_by_project(&app.pool, &project)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(model.requests().is_empty());
    let mut mismatch = body.clone();
    mismatch.cut = ForkCut::Head;
    assert_error(
        app.post_json(&format!("/api/conversations/{id}/revert"), &mismatch)
            .await,
        409,
        "operation_conflict",
    )
    .await;
    // An old window must fail before catch-up, repair, user insertion or LLM I/O.
    assert_error(app.post_json(&format!("/api/conversations/{id}/messages"), json!({"content":"stale", "provider_spec":"mock-llm:test-model", "expected_revision": revision})).await, 409, "stale_revision").await;
    assert_eq!(snapshot(&app, &id).await.1, current);
    for endpoint in ["messages", "resume", "compact", "revert"] {
        let data = if endpoint == "messages" {
            json!({"content":"forbidden", "provider_spec":"mock-llm:test-model"})
        } else {
            serde_json::to_value(request(revision, &first.id, ForkSide::Before)).unwrap()
        };
        assert_error(
            app.post_json(
                &format!("/api/conversations/{}/{endpoint}", result.backup.id),
                data,
            )
            .await,
            409,
            "read_only_conversation",
        )
        .await;
    }
    assert_error(
        app.patch_json(
            &format!("/api/projects/{project}/conversations/{}", result.backup.id),
            json!({"title":"not applied", "permission_mode":"bypass"}),
        )
        .await,
        409,
        "read_only_conversation",
    )
    .await;
    assert_eq!(
        app.patch_json(
            &format!("/api/projects/{project}/conversations/{}", result.backup.id),
            json!({"title":"saved history"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.get_json(&format!("/api/conversations/{}/run", result.backup.id))
            .await
            .json::<Value>()
            .await
            .unwrap()["resumable"],
        false
    );
    let branch = app.post_json(&format!("/api/conversations/{}/fork", result.backup.id), json!({"operation_id": uuid::Uuid::new_v4(), "expected_revision": revision, "cut":{"kind":"head"}})).await;
    assert_eq!(branch.status(), 201);
    assert_eq!(
        branch.json::<Value>().await.unwrap()["conversation"]["origin_kind"],
        "fork"
    );
    assert!(model.requests().is_empty());
    let send = app.post_json(&format!("/api/conversations/{id}/messages"), json!({"content":"edited question", "provider_spec":"mock-llm:test-model", "expected_revision":current_rev})).await;
    assert_eq!(send.status(), 200);
    sse_payloads(send).collect::<Vec<_>>().await;
    assert_eq!(model.requests().len(), 1);
    assert!(!model.requests()[0].to_string().contains("discard me"));
    assert_eq!(
        runs::latest_run_for_conversation(&app.pool, &id)
            .await
            .unwrap()
            .unwrap()
            .history_epoch,
        1
    );
    assert_eq!(snapshot(&app, &result.backup.id).await.1, backup);
    let second = successful_revert(
        &app,
        &id,
        &request(snapshot(&app, &id).await.0, &first.id, ForkSide::Before),
    )
    .await;
    assert_eq!(second.conversation.history_epoch, 2);
    assert!(second.context_revision > result.context_revision);
    assert!(snapshot(&app, &id).await.1.is_empty());
    assert_eq!(
        app.http_delete(&format!("/api/projects/{project}/conversations/{id}"))
            .await
            .status(),
        204
    );
    assert_eq!(snapshot(&app, &result.backup.id).await.1, backup);
    assert_eq!(
        conversations::get_conversation(&app.pool, &result.backup.id)
            .await
            .unwrap()
            .unwrap()
            .backup_of_conversation_id,
        None
    );
}

#[tokio::test]
async fn revert_restores_cow_original_and_removes_late_marker_with_backup_retaining_both() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    append(&app, &id, Role::User, "read").await;
    messages::insert_message(
        &app.pool,
        &id,
        &Message::new(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCall {
                id: "read".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            })],
        ),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let result = |content: &str| {
        Message::new(
            Role::Tool,
            vec![ContentPart::ToolResult(ToolResult {
                call_id: "read".into(),
                content: json!(content),
                is_error: false,
                images: vec![],
            })],
        )
    };
    let original = messages::insert_message(
        &app.pool,
        &id,
        &result("original content"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let reply = append(&app, &id, Role::Assistant, "done").await;
    messages::update_message_usage(&app.pool, &reply.id, Some(5000), Some(20), None)
        .await
        .unwrap();
    append(&app, &id, Role::User, "later").await;
    let copies = messages::apply_cow_swaps(
        &app.pool,
        &id,
        &[messages::CowSwap {
            call_id: "read".into(),
            copy: result("[Old tool result content cleared]"),
            details: Some(json!({"kind":"microcompact_cleared"})),
        }],
    )
    .await
    .unwrap();
    let marker = messages::insert_message_at_ordering(
        &app.pool,
        &id,
        &Message::text(Role::User, "future summary"),
        None,
        None,
        Some(&json!({"kind":"compaction"})),
        reply.ordering,
    )
    .await
    .unwrap();
    let response = successful_revert(
        &app,
        &id,
        &request(snapshot(&app, &id).await.0, &reply.id, ForkSide::After),
    )
    .await;
    let restored = messages::list_messages(&app.pool, &id).await.unwrap();
    assert_eq!(restored.len(), 4);
    assert_eq!(restored[2].id, original.id);
    assert_eq!(restored[2].swapped_at_revision, None);
    assert!(!restored.iter().any(|r| r.id == marker.id));
    let view = messages::load_llm_view(&app.pool, &id).await.unwrap();
    assert_eq!(view.messages.len(), 4);
    assert_eq!(view.anchor.unwrap().tokens, 5020);
    assert_eq!(
        app.get_json(&format!(
            "/api/conversations/{id}/messages/{}/original",
            copies[0].copy.id
        ))
        .await
        .status(),
        404
    );
    assert_eq!(
        app.get_json(&format!(
            "/api/conversations/{}/messages/{}/original",
            response.backup.id, copies[0].copy.id
        ))
        .await
        .status(),
        200
    );
    let backup = messages::list_messages(&app.pool, &response.backup.id)
        .await
        .unwrap();
    assert!(backup.iter().any(|r| r.id == marker.id));
    assert!(backup.iter().any(|r| r.id == copies[0].copy.id));
}

#[tokio::test]
async fn closed_tool_cut_resumes_in_new_epoch_without_replaying_tools_or_old_run() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("continued"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    append(&app, &id, Role::User, "tools").await;
    messages::insert_message(
        &app.pool,
        &id,
        &Message::new(
            Role::Assistant,
            ["a", "b"]
                .into_iter()
                .map(|id| {
                    ContentPart::ToolCall(ToolCall {
                        id: id.into(),
                        name: "execute_command".into(),
                        arguments: "{}".into(),
                    })
                })
                .collect(),
        ),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    let mut results = Vec::new();
    for call in ["a", "b"] {
        results.push(
            messages::insert_message(
                &app.pool,
                &id,
                &Message::new(
                    Role::Tool,
                    vec![ContentPart::ToolResult(ToolResult {
                        call_id: call.into(),
                        content: json!("already executed"),
                        is_error: false,
                        images: vec![],
                    })],
                ),
                None,
                None,
                None,
            )
            .await
            .unwrap(),
        );
    }
    append(&app, &id, Role::Assistant, "old answer").await;
    let old = runs::create_run(&app.pool, new_run(&id)).await.unwrap();
    runs::finish_run(
        &app.pool,
        &old.id,
        runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let revision = snapshot(&app, &id).await.0;
    assert_error(
        app.post_json(
            &format!("/api/conversations/{id}/revert"),
            request(revision, &results[0].id, ForkSide::After),
        )
        .await,
        400,
        "invalid_cut",
    )
    .await;
    let response = successful_revert(
        &app,
        &id,
        &request(revision, &results[1].id, ForkSide::After),
    )
    .await;
    assert_eq!(response.conversation.history_epoch, 1);
    assert!(
        runs::latest_run_for_conversation(&app.pool, &id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        app.get_json(&format!("/api/conversations/{id}/run"))
            .await
            .json::<Value>()
            .await
            .unwrap()["resumable"],
        true
    );
    assert!(model.requests().is_empty());
    let resume = app
        .post_json(&format!("/api/conversations/{id}/resume"), json!({}))
        .await;
    assert_eq!(resume.status(), 200);
    sse_payloads(resume).collect::<Vec<_>>().await;
    assert_eq!(model.requests().len(), 1);
    assert!(model.requests()[0].to_string().contains("already executed"));
    assert_eq!(
        runs::get_run(&app.pool, &old.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert_eq!(
        app.get_json(&format!("/api/conversations/{id}/run"))
            .await
            .json::<Value>()
            .await
            .unwrap()["resumable"],
        false
    );
}

#[tokio::test]
async fn pending_background_work_and_wake_block_without_mutating_history() {
    use oct_agent::db::background;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    let user = append(&app, &id, Role::User, "first").await;
    append(&app, &id, Role::Assistant, "reply").await;
    let before = snapshot(&app, &id).await;
    let body = request(before.0, &user.id, ForkSide::Before);
    let task = background::create_task(
        &app.pool,
        background::NewBackgroundTask {
            id: "bg-pending".into(),
            conversation_id: id.clone(),
            parent_run_id: None,
            parent_call_id: "removed".into(),
            command: "done".into(),
            working_dir: None,
            output_path: "/tmp/unused.log".into(),
        },
    )
    .await
    .unwrap();
    assert_error(
        app.post_json(&format!("/api/conversations/{id}/revert"), &body)
            .await,
        409,
        "background_pending",
    )
    .await;
    background::finish_task(
        &app.pool,
        &task.id,
        background::TaskFinalStatus::Completed,
        Some(0),
        None,
        0,
    )
    .await
    .unwrap();
    assert_error(
        app.post_json(&format!("/api/conversations/{id}/revert"), &body)
            .await,
        409,
        "background_pending",
    )
    .await;
    assert_eq!(snapshot(&app, &id).await, before);
    let wake = background::insert_notification_marking_tasks(
        &app.pool,
        &id,
        &Message::text(Role::User, "notification"),
        Some(&json!({"kind":"task_notification"})),
        std::slice::from_ref(&task.id),
    )
    .await
    .unwrap();
    let wake_body = request(snapshot(&app, &id).await.0, &user.id, ForkSide::Before);
    assert_error(
        app.post_json(&format!("/api/conversations/{id}/revert"), &wake_body)
            .await,
        409,
        "background_pending",
    )
    .await;
    let run = runs::create_run(&app.pool, new_run(&id)).await.unwrap();
    runs::finish_run(
        &app.pool,
        &run.id,
        runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let result = successful_revert(&app, &id, &wake_body).await;
    assert_eq!(result.context_revision, wake_body.expected_revision + 1);
    assert!(
        background::pending_wake(&app.pool, &id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        background::get_task(&app.pool, &task.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        background::get_visible_task(&app.pool, &task.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        app.get_json(&format!("/api/conversations/{id}/background-tasks"))
            .await
            .json::<Vec<Value>>()
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        app.get_json(&format!("/api/background-tasks/{}/output", task.id))
            .await
            .status(),
        404
    );
    assert_eq!(
        app.post_json(
            &format!("/api/background-tasks/{}/cancel", task.id),
            json!({})
        )
        .await
        .status(),
        404
    );
    assert!(
        snapshot(&app, &result.backup.id)
            .await
            .1
            .iter()
            .any(|r| r["id"] == wake)
    );
}

#[tokio::test]
async fn invalid_noop_stale_and_deleted_backup_replays_are_rejected() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    let other = app.create_conversation(&project).await;
    let foreign = append(&app, &other, Role::User, "foreign").await;
    let user = append(&app, &id, Role::User, "prompt").await;
    let reply = append(&app, &id, Role::Assistant, "answer").await;
    for (body, status, code) in [
        (
            request(1, &user.id, ForkSide::Before),
            409,
            "stale_revision",
        ),
        (request(2, &foreign.id, ForkSide::Before), 404, "not_found"),
        (request(2, &reply.id, ForkSide::After), 400, "invalid_cut"),
        (
            RevertRequest {
                cut: ForkCut::Head,
                ..request(2, &user.id, ForkSide::Before)
            },
            400,
            "invalid_cut",
        ),
    ] {
        assert_error(
            app.post_json(&format!("/api/conversations/{id}/revert"), body)
                .await,
            status,
            code,
        )
        .await;
    }
    assert_eq!(
        conversations::list_conversations_by_project(&app.pool, &project)
            .await
            .unwrap()
            .len(),
        2
    );
    let body = request(2, &user.id, ForkSide::Before);
    let result = successful_revert(&app, &id, &body).await;
    assert_eq!(
        app.http_delete(&format!(
            "/api/projects/{project}/conversations/{}",
            result.backup.id
        ))
        .await
        .status(),
        204
    );
    assert_error(
        app.post_json(&format!("/api/conversations/{id}/revert"), body)
            .await,
        409,
        "operation_conflict",
    )
    .await;
}

#[tokio::test]
async fn database_failure_rolls_back_backup_cut_and_revision_together() {
    use sqlx::Executor;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    let user = append(&app, &id, Role::User, "prompt").await;
    append(&app, &id, Role::Assistant, "answer").await;
    let before = snapshot(&app, &id).await;
    app.pool.execute("CREATE TRIGGER reject_cut BEFORE DELETE ON conversation_messages BEGIN SELECT RAISE(ABORT, 'injected cut failure'); END").await.unwrap();
    assert_error(
        app.post_json(
            &format!("/api/conversations/{id}/revert"),
            request(before.0, &user.id, ForkSide::Before),
        )
        .await,
        500,
        "internal",
    )
    .await;
    assert_eq!(snapshot(&app, &id).await, before);
    assert_eq!(
        conversations::get_conversation(&app.pool, &id)
            .await
            .unwrap()
            .unwrap()
            .history_epoch,
        0
    );
    assert_eq!(
        conversations::list_conversations_by_project(&app.pool, &project)
            .await
            .unwrap()
            .len(),
        1
    );
    app.pool.execute("DROP TRIGGER reject_cut").await.unwrap();
    successful_revert(&app, &id, &request(before.0, &user.id, ForkSide::Before)).await;
}

#[tokio::test]
async fn active_run_and_managed_recovery_block_then_retired_tasks_leave_current_view() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Hang(vec![text_chunk("live")])]).await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let id = app.create_conversation(&project).await;
    let first = append(&app, &id, Role::User, "first").await;
    append(&app, &id, Role::Assistant, "answer").await;
    let live = app
        .post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"content":"live", "provider_spec":"mock-llm:test-model"}),
        )
        .await;
    let mut stream = sse_payloads(live);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = stream.next().await {
            if event["type"] == "text_delta" {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_error(
        app.post_json(
            &format!("/api/conversations/{id}/revert"),
            request(snapshot(&app, &id).await.0, &first.id, ForkSide::Before),
        )
        .await,
        409,
        "already_running",
    )
    .await;
    app.post_json(&format!("/api/conversations/{id}/cancel"), json!({}))
        .await;
    stream.collect::<Vec<_>>().await;

    messages::insert_message(
        &app.pool,
        &id,
        &Message::new(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCall {
                id: "task-pending".into(),
                name: "task".into(),
                arguments: "{}".into(),
            })],
        ),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let parent = runs::create_run(&app.pool, new_run(&id)).await.unwrap();
    let mut tx = app.pool.begin().await.unwrap();
    let child = conversations::create_subagent_conversation_in_tx(
        &mut tx,
        conversations::NewSubagentConversation {
            project_id: &project,
            parent_conversation_id: &id,
            root_conversation_id: &id,
            agent_type: "worker",
            title: "child",
        },
    )
    .await
    .unwrap();
    let mut child_request = new_run(&child.id);
    child_request.kind = "subagent";
    child_request.parent_run_id = Some(parent.id.clone());
    child_request.root_run_id = Some(parent.id.clone());
    child_request.parent_tool_call_id = Some("task-pending".into());
    let child_run = runs::create_run_in_tx(&mut tx, child_request)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let revision = snapshot(&app, &id).await.0;
    assert_error(
        app.post_json(
            &format!("/api/conversations/{}/revert", child.id),
            request(0, &first.id, ForkSide::Before),
        )
        .await,
        409,
        "managed_conversation",
    )
    .await;
    assert_error(
        app.post_json(
            &format!("/api/conversations/{id}/revert"),
            request(revision, &first.id, ForkSide::Before),
        )
        .await,
        409,
        "pending_recovery",
    )
    .await;
    runs::finish_run(
        &app.pool,
        &child_run.id,
        runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    messages::answer_tool_call_if_absent(&app.pool, &id, "task-pending", "cancelled", true, None)
        .await
        .unwrap();
    runs::finish_run(
        &app.pool,
        &parent.id,
        runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    successful_revert(
        &app,
        &id,
        &request(snapshot(&app, &id).await.0, &first.id, ForkSide::Before),
    )
    .await;
    assert!(
        app.get_json(&format!("/api/conversations/{id}/subagents"))
            .await
            .json::<Vec<Value>>()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        app.get_json(&format!("/api/subagents/{}/messages", child.id))
            .await
            .status(),
        404
    );
    assert_eq!(
        app.post_json(&format!("/api/subagents/{}/cancel", child.id), json!({}))
            .await
            .status(),
        404
    );
    assert!(
        runs::get_run(&app.pool, &child_run.id)
            .await
            .unwrap()
            .is_some()
    );

    // Child epochs come from their root even though a managed child has no
    // independent revert. Its latest-run query must still support recovery.
    let root = runs::create_run(&app.pool, new_run(&id)).await.unwrap();
    let mut tx = app.pool.begin().await.unwrap();
    let child = conversations::create_subagent_conversation_in_tx(
        &mut tx,
        conversations::NewSubagentConversation {
            project_id: &project,
            parent_conversation_id: &id,
            root_conversation_id: &id,
            agent_type: "worker",
            title: "new child",
        },
    )
    .await
    .unwrap();
    let mut req = new_run(&child.id);
    req.kind = "subagent";
    req.parent_run_id = Some(root.id.clone());
    req.root_run_id = Some(root.id);
    req.parent_tool_call_id = Some("new-task".into());
    let run = runs::create_run_in_tx(&mut tx, req).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(run.history_epoch, 1);
    assert_eq!(
        runs::latest_run_for_conversation(&app.pool, &child.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        run.id
    );
}
