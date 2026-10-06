mod support;

use futures_util::StreamExt;
use oct_agent::db::{
    conversations,
    history::{ForkCut, ForkRequest, ForkResponse, ForkSide},
    messages,
};
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use support::{
    TestApp,
    llm::{self, LlmTurn, finish_chunk, text_chunk, tool_call_chunk, tool_call_chunk_at},
    mcp::{FakeMcpConnection, fake_tool},
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
    let response = app
        .get_json(&format!("/api/conversations/{id}/messages"))
        .await;
    assert_eq!(response.status(), 200);
    let revision = response.headers()["X-Context-Revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    (revision, response.json().await.unwrap())
}

fn request(revision: i64, cut: ForkCut) -> ForkRequest {
    ForkRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        expected_revision: revision,
        cut,
        title: None,
    }
}

async fn fork(app: &TestApp, id: &str, request: &ForkRequest) -> ForkResponse {
    let response = app
        .post_json(&format!("/api/conversations/{id}/fork"), request)
        .await;
    if response.status() != 201 {
        panic!("fork failed: {}", response.text().await.unwrap());
    }
    response.json().await.unwrap()
}

#[tokio::test]
async fn cuts_share_content_retry_once_and_continue_after_source_deletion() {
    let app = TestApp::new().await;
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("branch answer"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    app.set_permission_mode(&project, &source, "accept_edits")
        .await;
    let user = append(&app, &source, Role::User, "original question").await;
    let reply = append(&app, &source, Role::Assistant, "original answer").await;
    let next = append(&app, &source, Role::User, "editable question").await;
    let (revision, original) = snapshot(&app, &source).await;
    assert_eq!(revision, 3);
    let before = request(
        revision,
        ForkCut::Message {
            message_id: next.id.clone(),
            side: ForkSide::Before,
        },
    );
    let branch = fork(&app, &source, &before).await;
    assert_eq!(branch.draft.as_deref(), Some("editable question"));
    assert_eq!(branch.context_revision, 2);
    assert_eq!(branch.conversation.permission_mode, "accept_edits");
    assert_eq!(branch.conversation.title, "e2e（分支）");
    let (branch_revision, shared) = snapshot(&app, &branch.conversation.id).await;
    assert_eq!(branch_revision, 2);
    assert_eq!(
        shared[0]["ordering"], 1,
        "fork slots remain compatible with compaction parking"
    );
    assert_eq!(
        shared
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![user.id.as_str(), reply.id.as_str()]
    );
    assert!(shared.iter().all(|row| row["inherited"] == true));
    assert!(llm.requests().is_empty(), "fork must not invoke a model");
    assert!(
        oct_agent::db::runs::latest_run_for_conversation(&app.pool, &branch.conversation.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fork(&app, &source, &before).await.conversation.id,
        branch.conversation.id
    );
    let mut mismatch = before.clone();
    mismatch.cut = ForkCut::Head;
    let response = app
        .post_json(&format!("/api/conversations/{source}/fork"), &mismatch)
        .await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "operation_conflict"
    );
    let after = fork(
        &app,
        &source,
        &request(
            revision,
            ForkCut::Message {
                message_id: reply.id,
                side: ForkSide::After,
            },
        ),
    )
    .await;
    assert_eq!(after.draft, None);
    assert_eq!(snapshot(&app, &after.conversation.id).await.1.len(), 2);
    let head = fork(&app, &source, &request(revision, ForkCut::Head)).await;
    assert_eq!(snapshot(&app, &head.conversation.id).await.1.len(), 3);
    assert_eq!(
        snapshot(&app, &source).await.1,
        original,
        "source history unchanged"
    );
    let response = app
        .http_delete(&format!("/api/projects/{project}/conversations/{source}"))
        .await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        messages::list_messages(&app.pool, &branch.conversation.id)
            .await
            .unwrap()
            .len(),
        2
    );
    let metadata = conversations::get_conversation(&app.pool, &branch.conversation.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.forked_from_conversation_id, None);
    let response = app
        .send_message(&branch.conversation.id, "new branch question")
        .await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    let input = requests[0]["messages"].to_string();
    assert!(
        input.contains("original question")
            && input.contains("original answer")
            && input.contains("new branch question")
    );
    assert!(!input.contains("editable question"));
    let (revision, continued) = snapshot(&app, &branch.conversation.id).await;
    assert_eq!(revision, 4);
    assert!(continued[3]["fork_cut"].is_object());
    assert_eq!(continued[3]["inherited"], false);
    assert_eq!(
        snapshot(&app, &head.conversation.id).await.1.len(),
        3,
        "other fork unchanged"
    );
}

#[tokio::test]
async fn historical_cuts_restore_late_cow_and_remove_inserted_summary() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    append(&app, &source, Role::User, "read").await;
    let call = Message::new(
        Role::Assistant,
        vec![ContentPart::ToolCall(ToolCall {
            id: "read-1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        })],
    );
    messages::insert_message(&app.pool, &source, &call, None, None, None)
        .await
        .unwrap();
    let result = |content: &str| {
        Message::new(
            Role::Tool,
            vec![ContentPart::ToolResult(ToolResult {
                call_id: "read-1".into(),
                content: json!(content),
                is_error: false,
                images: vec![],
            })],
        )
    };
    let original = messages::insert_message(
        &app.pool,
        &source,
        &result("full original file"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let reply = append(&app, &source, Role::Assistant, "read done").await;
    let next = append(&app, &source, Role::User, "next question").await;
    let swaps = messages::apply_cow_swaps(
        &app.pool,
        &source,
        &[messages::CowSwap {
            call_id: "read-1".into(),
            copy: result("[Old tool result content cleared]"),
            details: Some(json!({"kind":"microcompact_cleared"})),
        }],
    )
    .await
    .unwrap();
    let marker = messages::insert_message_at_ordering(
        &app.pool,
        &source,
        &Message::text(Role::User, "summary"),
        None,
        None,
        Some(&json!({"kind":"compaction"})),
        reply.ordering,
    )
    .await
    .unwrap();
    let (revision, before) = snapshot(&app, &source).await;
    let past = fork(
        &app,
        &source,
        &request(
            revision,
            ForkCut::Message {
                message_id: reply.id,
                side: ForkSide::After,
            },
        ),
    )
    .await;
    let restored = messages::list_messages(&app.pool, &past.conversation.id)
        .await
        .unwrap();
    assert_eq!(restored.len(), 4);
    assert_eq!(restored[2].id, original.id);
    assert_eq!(restored[2].swapped_at_revision, None);
    assert!(restored[2].parts_json.contains("full original file"));
    assert!(restored.iter().all(|row| row.id != marker.id));
    let current = fork(&app, &source, &request(revision, ForkCut::Head)).await;
    let kept = messages::list_messages(&app.pool, &current.conversation.id)
        .await
        .unwrap();
    assert!(kept.iter().any(|row| row.id == swaps[0].copy.id));
    assert!(kept.iter().any(|row| row.id == marker.id));
    // A later before-user cut also reconstructs the earlier, uncompressed prefix.
    let user_cut = fork(
        &app,
        &source,
        &request(
            revision,
            ForkCut::Message {
                message_id: next.id,
                side: ForkSide::Before,
            },
        ),
    )
    .await;
    assert_eq!(
        messages::list_messages(&app.pool, &user_cut.conversation.id)
            .await
            .unwrap()[2]
            .id,
        original.id
    );
    assert_eq!(snapshot(&app, &source).await.1, before);
    let view = messages::load_llm_view(&app.pool, &past.conversation.id)
        .await
        .unwrap();
    assert_eq!(view.messages.len(), 4);
    assert_eq!(
        app.http_delete(&format!("/api/projects/{project}/conversations/{source}"))
            .await
            .status(),
        204
    );
    let response = app
        .get_json(&format!(
            "/api/conversations/{}/messages/{}/original",
            current.conversation.id, swaps[0].copy.id
        ))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["id"], original.id);
    assert_eq!(
        messages::list_messages(&app.pool, &past.conversation.id)
            .await
            .unwrap()[2]
            .id,
        original.id
    );
}

#[tokio::test]
async fn rejects_stale_cross_conversation_and_unfinished_cuts_without_mutation() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    let other = app.create_conversation(&project).await;
    let foreign = append(&app, &other, Role::User, "foreign").await;
    append(&app, &source, Role::User, "prompt").await;
    let call = messages::insert_message(
        &app.pool,
        &source,
        &Message::new(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCall {
                id: "dangling".into(),
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
    let (revision, original) = snapshot(&app, &source).await;
    for (body, status, code) in [
        (request(revision - 1, ForkCut::Head), 409, "stale_revision"),
        (
            request(
                revision,
                ForkCut::Message {
                    message_id: foreign.id,
                    side: ForkSide::Before,
                },
            ),
            404,
            "not_found",
        ),
        (
            request(
                revision,
                ForkCut::Message {
                    message_id: call.id,
                    side: ForkSide::After,
                },
            ),
            400,
            "invalid_cut",
        ),
        (request(revision, ForkCut::Head), 400, "invalid_cut"),
    ] {
        let response = app
            .post_json(&format!("/api/conversations/{source}/fork"), body)
            .await;
        assert_eq!(response.status(), status);
        assert_eq!(response.json::<Value>().await.unwrap()["code"], code);
    }
    assert_eq!(snapshot(&app, &source).await.1, original);
    assert_eq!(
        conversations::list_conversations_by_project(&app.pool, &project)
            .await
            .unwrap()
            .len(),
        2
    );
    // Empty head is a useful independent branch too.
    let empty = app.create_conversation(&project).await;
    assert_eq!(
        fork(&app, &empty, &request(0, ForkCut::Head))
            .await
            .context_revision,
        0
    );
}

#[tokio::test]
async fn rejects_active_source_but_replays_committed_operation() {
    let app = TestApp::new().await;
    let (gate, receiver) = llm::gate();
    let llm = llm::spawn_llm_mock(vec![LlmTurn::Gated {
        pre: vec![text_chunk("working")],
        gate: receiver,
        post: vec![finish_chunk("stop")],
    }])
    .await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    let committed = request(0, ForkCut::Head);
    let branch = fork(&app, &source, &committed).await;
    let stream = app.send_message(&source, "start").await;
    assert_eq!(stream.status(), 200);
    let response = app
        .post_json(
            &format!("/api/conversations/{source}/fork"),
            request(1, ForkCut::Head),
        )
        .await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "already_running"
    );
    assert_eq!(
        fork(&app, &source, &committed).await.conversation.id,
        branch.conversation.id
    );
    gate.send(true).unwrap();
    app.read_sse(stream).await;
    assert_eq!(llm.requests().len(), 1);
    assert_eq!(
        app.http_delete(&format!(
            "/api/projects/{project}/conversations/{}",
            branch.conversation.id
        ))
        .await
        .status(),
        204
    );
    let response = app
        .post_json(&format!("/api/conversations/{source}/fork"), &committed)
        .await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "operation_conflict"
    );
}

#[tokio::test]
async fn inherited_reads_do_not_authorize_edits_in_a_new_run() {
    let app = TestApp::new().await;
    std::fs::write(app.dir.path().join("a.txt"), "original file").unwrap();
    let llm = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk("read-source", "read_file", r#"{"path":"a.txt"}"#),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("read done"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "edit-fork",
                "edit_file",
                r#"{"file_path":"a.txt","old_string":"original","new_string":"changed"}"#,
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("need a fresh read"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    app.set_permission_mode(&project, &source, "bypass").await;
    let response = app.send_message(&source, "read file").await;
    app.read_sse(response).await;
    let (revision, source_history) = snapshot(&app, &source).await;
    assert!(source_history.iter().any(|row| row["role"] == "tool"));
    let branch = fork(&app, &source, &request(revision, ForkCut::Head)).await;
    assert_eq!(llm.requests().len(), 2);
    let response = app
        .send_message(&branch.conversation.id, "edit that file")
        .await;
    app.read_sse(response).await;
    assert_eq!(
        std::fs::read_to_string(app.dir.path().join("a.txt")).unwrap(),
        "original file"
    );
    let branch_history = snapshot(&app, &branch.conversation.id).await.1;
    let error = branch_history
        .iter()
        .find(|row| row["parts"][0]["call_id"] == "edit-fork")
        .unwrap();
    assert_eq!(error["parts"][0]["is_error"], true);
    assert!(
        error["parts"][0]["content"]
            .as_str()
            .unwrap()
            .contains("has not been read")
    );
    assert_eq!(snapshot(&app, &source).await.1, source_history);
}

#[tokio::test]
async fn managed_children_and_pending_task_recovery_cannot_be_forked() {
    use oct_agent::db::runs::{self, NewRun};
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    let user = append(&app, &source, Role::User, "delegate").await;
    messages::insert_message(
        &app.pool,
        &source,
        &Message::new(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCall {
                id: "pending-task".into(),
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
    let root = runs::create_run(
        &app.pool,
        NewRun {
            conversation_id: source.clone(),
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
    let mut tx = app.pool.begin().await.unwrap();
    let child = conversations::create_subagent_conversation_in_tx(
        &mut tx,
        conversations::NewSubagentConversation {
            project_id: &project,
            parent_conversation_id: &source,
            root_conversation_id: &source,
            agent_type: "worker",
            title: "pending child",
        },
    )
    .await
    .unwrap();
    runs::create_run_in_tx(
        &mut tx,
        NewRun {
            conversation_id: child.id.clone(),
            kind: "subagent",
            parent_run_id: Some(root.id.clone()),
            root_run_id: Some(root.id),
            parent_tool_call_id: Some("pending-task".into()),
            provider_id: None,
            model_id: None,
            context_window: None,
            description: Some("pending child".into()),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    runs::mark_stale_runs_lost(&app.pool).await.unwrap();
    for (id, body, code) in [
        (
            &source,
            request(
                2,
                ForkCut::Message {
                    message_id: user.id,
                    side: ForkSide::Before,
                },
            ),
            "pending_recovery",
        ),
        (&child.id, request(0, ForkCut::Head), "managed_conversation"),
    ] {
        let response = app
            .post_json(&format!("/api/conversations/{id}/fork"), body)
            .await;
        assert_eq!(response.status(), 409);
        assert_eq!(response.json::<Value>().await.unwrap()["code"], code);
    }
    assert_eq!(
        conversations::list_conversations_by_project(&app.pool, &project)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn aborted_tool_round_forks_at_its_closed_boundary_and_resumes_without_replaying_tools() {
    let app = TestApp::new().await;
    std::fs::write(app.dir.path().join("a.txt"), "completed read").unwrap();
    let llm = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk_at(0, "fast", "read_file", r#"{"path":"a.txt"}"#),
            tool_call_chunk_at(1, "slow", "mcp__fake__slow", "{}"),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("continued branch"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm).await;
    let server: Value = app
        .post_json(
            "/api/mcp/servers",
            json!({
                "name": "fake", "transport": "stdio", "command": "unused-fork-test",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let tools = vec![fake_tool("slow", None)];
    let fake = Arc::new(FakeMcpConnection::new(tools.clone()).with_hang("slow"));
    app.mcp.insert_connection_for_test(
        server["id"].as_str().unwrap(),
        "fake",
        None,
        fake.clone(),
        tools,
    );
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    app.set_permission_mode(&project, &source, "bypass").await;
    let mut events = sse_payloads(app.send_message(&source, "run both").await);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut slow_started = false;
        let mut fast_finished = false;
        while let Some(event) = events.next().await {
            slow_started |= event["type"] == "tool_call_start" && event["data"]["id"] == "slow";
            fast_finished |= event["type"] == "tool_result" && event["data"]["call_id"] == "fast";
            if slow_started && fast_finished {
                return;
            }
        }
        panic!("run ended before the tools overlapped");
    })
    .await
    .expect("wait for overlapping tools");
    assert_eq!(app.cancel(&source).await.status(), 200);
    let remaining = tokio::time::timeout(Duration::from_secs(10), events.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(remaining.iter().any(|event| event["type"] == "cancelled"));
    let (revision, original) = snapshot(&app, &source).await;
    let fast = original
        .iter()
        .find(|row| row["parts"][0]["call_id"] == "fast")
        .unwrap();
    assert!(
        fast["fork_cut"].is_null(),
        "parallel batch cannot be split after its first result"
    );
    let cut = ForkCut::Message {
        message_id: fast["id"].as_str().unwrap().into(),
        side: ForkSide::After,
    };
    let response = app
        .post_json(
            &format!("/api/conversations/{source}/fork"),
            request(revision, cut),
        )
        .await;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "invalid_cut"
    );
    let last = original.last().unwrap();
    assert_eq!(last["role"], "tool");
    assert!(last["fork_cut"].is_object());
    let checkpoint = fork(
        &app,
        &source,
        &request(
            revision,
            serde_json::from_value(last["fork_cut"].clone()).unwrap(),
        ),
    )
    .await;
    let branch = fork(&app, &source, &request(revision, ForkCut::Head)).await;
    assert_eq!(
        snapshot(&app, &checkpoint.conversation.id).await.1.len(),
        original.len()
    );
    assert_eq!(llm.requests().len(), 1, "fork does not run a model");
    assert!(
        oct_agent::db::runs::latest_run_for_conversation(&app.pool, &branch.conversation.id)
            .await
            .unwrap()
            .is_none()
    );
    let state: Value = app
        .get_json(&format!(
            "/api/conversations/{}/run",
            branch.conversation.id
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(state["resumable"], true);
    let resumed = app
        .post_json(
            &format!("/api/conversations/{}/resume", branch.conversation.id),
            json!(null),
        )
        .await;
    assert_eq!(resumed.status(), 200);
    app.read_sse(resumed).await;
    assert_eq!(llm.requests().len(), 2);
    assert_eq!(
        fake.calls().len(),
        1,
        "source tool was not replayed in the fork"
    );
    let input = llm.requests()[1]["messages"].to_string();
    assert!(input.contains("completed read"));
    assert!(input.contains("用户取消了执行") || input.contains("cancelled"));
    assert_eq!(snapshot(&app, &source).await.1, original);
    let state: Value = app
        .get_json(&format!(
            "/api/conversations/{}/run",
            branch.conversation.id
        ))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(state["resumable"], false);
}
