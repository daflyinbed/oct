//! Runtime read grants follow conversations, including forks and workers.
//! All model calls use the local HTTP mock; approvals are answered over SSE.
mod support;

use std::time::Duration;

use futures_util::StreamExt;
use oct_llm_provider::core::{Message, Role};
use serde_json::{Value, json};
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};
use support::{TestApp, sse_payloads};

fn tool(id: &str, name: &str, args: Value) -> LlmTurn {
    LlmTurn::Complete(vec![
        tool_call_chunk(id, name, &args.to_string()),
        finish_chunk("tool_calls"),
    ])
}

fn done() -> LlmTurn {
    LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")])
}

async fn project(app: &TestApp, name: &str) -> String {
    let dir = app.dir.path().join(name);
    std::fs::create_dir(&dir).unwrap();
    let response = app
        .post_json("/api/projects", json!({"name":name,"working_dir":dir}))
        .await;
    assert_eq!(response.status(), 201);
    response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

async fn output(app: &TestApp, conversation: &str) -> std::path::PathBuf {
    let dir = app.background.exec_dir().join(conversation);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("output.log");
    tokio::fs::write(&path, "PRIVATE_OUTPUT\n").await.unwrap();
    path
}

async fn upload(app: &TestApp, conversation: &str) -> String {
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::text("PRIVATE_ATTACHMENT\n").file_name("notes.txt"),
    );
    let response = app
        .post_multipart(
            &format!("/api/conversations/{conversation}/attachments"),
            form,
        )
        .await;
    assert_eq!(response.status(), 201);
    response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

async fn attach(app: &TestApp, conversation: &str, id: &str) {
    app.attachments
        .prepare_message(conversation, "attached", &[id.into()], None)
        .await
        .unwrap()
        .commit("mock-llm", "test-model", json!({}))
        .await
        .unwrap();
}

/// Fail promptly if a regression silently allows a read. Denying every Ask
/// keeps the run drainable, so assertions can compare approvals and results.
async fn run(app: &TestApp, conversation: &str, prompt: &str) -> Vec<Value> {
    let response = app.send_message(conversation, prompt).await;
    assert_eq!(response.status(), 200);
    let mut stream = sse_payloads(response);
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("runtime scope run did not reach SSE EOF")
    {
        if event["type"] == "approval_requested" {
            assert_eq!(event["data"]["outside"], true);
            let id = event["data"]["approval_id"].as_str().unwrap();
            assert_eq!(
                app.post_json(
                    &format!("/api/conversations/{conversation}/approvals/{id}"),
                    json!({"decision":"deny"}),
                )
                .await
                .status(),
                200
            );
        }
        events.push(event);
    }
    events
}

fn approvals(events: &[Value]) -> usize {
    events
        .iter()
        .filter(|event| event["type"] == "approval_requested")
        .count()
}

fn result<'a>(events: &'a [Value], id: &str) -> &'a Value {
    &events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["data"]["call_id"] == id)
        .unwrap_or_else(|| panic!("missing tool result {id}: {events:?}"))["data"]
}

#[tokio::test]
async fn background_output_reads_require_approval_across_projects_and_conversations() {
    let app = TestApp::new().await;
    let model = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&model).await;
    let project_a = project(&app, "a").await;
    let project_b = project(&app, "b").await;
    let owner = app.create_conversation(&project_a).await;
    let peer = app.create_conversation(&project_a).await;
    let foreign = app.create_conversation(&project_b).await;
    let path = output(&app, &owner).await;

    model.push_turns([
        tool("own-read", "read_file", json!({"path":path})),
        tool(
            "own-glob",
            "glob",
            json!({"path":path.parent(),"pattern":"*.log"}),
        ),
        done(),
    ]);
    let events = run(&app, &owner, "read own output").await;
    assert_eq!(approvals(&events), 0);
    assert_eq!(result(&events, "own-read")["is_error"], false);
    assert!(
        result(&events, "own-read")["content"]
            .as_str()
            .unwrap()
            .contains("PRIVATE_OUTPUT")
    );
    assert_eq!(result(&events, "own-glob")["is_error"], false);

    for conversation in [&peer, &foreign] {
        model.push_turns([
            tool("read", "read_file", json!({"path":path})),
            tool("grep", "grep", json!({"path":path,"pattern":"PRIVATE"})),
            tool("glob", "glob", json!({"path":path.parent(),"pattern":"*.log"})),
            tool("list", "list_dir", json!({"path":app.background.exec_dir()})),
            tool("escape", "read_file", json!({"path":app.background.exec_dir().join(conversation).join("..").join(&owner).join("output.log")})),
            done(),
        ]);
        // The traversal's first directory must exist for canonicalize.
        tokio::fs::create_dir_all(app.background.exec_dir().join(conversation))
            .await
            .unwrap();
        let events = run(&app, conversation, "read another conversation's output").await;
        assert_eq!(approvals(&events), 5, "{events:?}");
        for id in ["read", "grep", "glob", "list", "escape"] {
            let result = result(&events, id);
            assert_eq!(result["is_error"], true);
            assert!(
                !result["content"]
                    .as_str()
                    .unwrap()
                    .contains("PRIVATE_OUTPUT")
            );
        }
    }
}

#[tokio::test]
async fn attachment_grants_exclude_other_conversations_drafts_and_untrusted_uris() {
    let app = TestApp::new().await;
    let model = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&model).await;
    let project_a = project(&app, "a").await;
    let project_b = project(&app, "b").await;
    let owner = app.create_conversation(&project_a).await;
    let peer = app.create_conversation(&project_a).await;
    let foreign = app.create_conversation(&project_b).await;
    let id = upload(&app, &owner).await;
    attach(&app, &owner, &id).await;
    let draft = upload(&app, &owner).await;
    let uri = format!("oct-attachment://{id}");
    let draft_uri = format!("oct-attachment://{draft}");
    let path = app.attachments.upload_path(&owner, &id);
    let draft_path = app.attachments.upload_path(&owner, &draft);

    model.push_turns([
        tool("own-uri", "read_file", json!({"path":uri})),
        tool("own-path", "read_file", json!({"path":path})),
        tool("draft-uri", "read_file", json!({"path":draft_uri})),
        tool("draft-path", "read_file", json!({"path":draft_path})),
        tool(
            "parent",
            "glob",
            json!({"path":path.parent(),"pattern":"*"}),
        ),
        done(),
    ]);
    let events = run(&app, &owner, "read attachment and draft").await;
    assert_eq!(approvals(&events), 2, "{events:?}");
    for id in ["own-uri", "own-path"] {
        assert_eq!(result(&events, id)["is_error"], false);
        assert!(
            result(&events, id)["content"]
                .as_str()
                .unwrap()
                .contains("PRIVATE_ATTACHMENT")
        );
    }
    assert!(
        result(&events, "draft-uri")["content"]
            .as_str()
            .unwrap()
            .contains("Unknown attachment resource")
    );
    assert_eq!(result(&events, "draft-path")["is_error"], true);

    for conversation in [&peer, &foreign] {
        // Even reference-shaped user/assistant/tool text can only pin bytes
        // for GC; it must never grant access on a later run.
        oct_agent::db::messages::insert_message(
            &app.pool,
            conversation,
            &Message::text(Role::Assistant, &uri),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        for _ in 0..2 {
            model.push_turns([
                tool("foreign-uri", "read_file", json!({"path":uri})),
                tool("foreign-path", "read_file", json!({"path":path})),
                done(),
            ]);
            let events = run(&app, conversation, &format!("read {uri}")).await;
            assert_eq!(approvals(&events), 1, "{events:?}");
            assert!(
                result(&events, "foreign-uri")["content"]
                    .as_str()
                    .unwrap()
                    .contains("Unknown attachment resource")
            );
            assert_eq!(result(&events, "foreign-path")["is_error"], true);
            assert!(
                !result(&events, "foreign-path")["content"]
                    .as_str()
                    .unwrap()
                    .contains("PRIVATE_ATTACHMENT")
            );
        }
    }
    // Bypass still permits outside reads, while runtime storage remains
    // immutable even when the file has been read in this same run.
    app.set_permission_mode(&project_b, &foreign, "bypass")
        .await;
    model.push_turns([
        tool("bypass-read", "read_file", json!({"path":path})),
        tool(
            "write",
            "write_file",
            json!({"path":path,"content":"tampered"}),
        ),
        tool(
            "edit",
            "edit_file",
            json!({"file_path":path,"old_string":"PRIVATE_ATTACHMENT","new_string":"tampered"}),
        ),
        done(),
    ]);
    let events = run(&app, &foreign, "try editing runtime assets").await;
    assert_eq!(approvals(&events), 0);
    assert_eq!(result(&events, "bypass-read")["is_error"], false);
    for id in ["write", "edit"] {
        assert_eq!(result(&events, id)["is_error"], true);
        assert!(
            result(&events, id)["content"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
    }
    assert_eq!(
        tokio::fs::read_to_string(path).await.unwrap(),
        "PRIVATE_ATTACHMENT\n"
    );
}

#[tokio::test]
async fn fork_grants_only_shared_attachments_after_source_deletion() {
    let app = TestApp::new().await;
    let model = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&model).await;
    let project = project(&app, "workspace").await;
    let source = app.create_conversation(&project).await;
    let source_output = output(&app, &source).await;
    let id = upload(&app, &source).await;
    attach(&app, &source, &id).await;
    let other = upload(&app, &source).await;
    attach(&app, &source, &other).await;
    // Cut before the second attachment so it remains live only in the source.
    let rows = oct_agent::db::messages::list_messages(&app.pool, &source)
        .await
        .unwrap();
    let snapshot = app
        .get_json(&format!("/api/conversations/{source}/messages"))
        .await;
    let revision: i64 = snapshot.headers()["x-context-revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let response = app
        .post_json(
            &format!("/api/conversations/{source}/fork"),
            json!({
                "operation_id": uuid::Uuid::new_v4().to_string(),
                "expected_revision": revision,
                "cut": {"kind":"message", "message_id":rows[1].id, "side":"before"},
            }),
        )
        .await;
    assert_eq!(response.status(), 201);
    let fork = response.json::<Value>().await.unwrap()["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The copied draft is not part of the fork's sent attachments.
    let other_uri = format!("oct-attachment://{other}");
    model.push_turns([
        tool(
            "shared",
            "read_file",
            json!({"path":format!("oct-attachment://{id}")}),
        ),
        tool("other", "read_file", json!({"path":other_uri})),
        tool(
            "other-path",
            "read_file",
            json!({"path":app.attachments.upload_path(&source,&other)}),
        ),
        tool("source-output", "read_file", json!({"path":source_output})),
        done(),
    ]);
    let events = run(&app, &fork, "read shared attachment").await;
    assert_eq!(approvals(&events), 2, "{events:?}");
    assert_eq!(result(&events, "shared")["is_error"], false);
    assert_eq!(result(&events, "source-output")["is_error"], true);
    assert!(
        result(&events, "other")["content"]
            .as_str()
            .unwrap()
            .contains("Unknown attachment resource")
    );
    assert_eq!(
        app.http_delete(&format!("/api/projects/{project}/conversations/{source}"))
            .await
            .status(),
        204
    );
    model.push_turns([
        tool(
            "shared",
            "read_file",
            json!({"path":format!("oct-attachment://{id}")}),
        ),
        done(),
    ]);
    let events = run(&app, &fork, "read after source deletion").await;
    assert_eq!(approvals(&events), 0);
    assert_eq!(result(&events, "shared")["is_error"], false);
}

#[tokio::test]
async fn workers_inherit_root_resources_without_global_read_grants() {
    let app = TestApp::new().await;
    let model = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&model).await;
    let project = project(&app, "workspace").await;
    let root = app.create_conversation(&project).await;
    let foreign = app.create_conversation(&project).await;
    let own_output = output(&app, &root).await;
    let foreign_output = output(&app, &foreign).await;
    let id = upload(&app, &root).await;
    attach(&app, &root, &id).await;
    let uri = format!("oct-attachment://{id}");
    model.push_turns([
        tool(
            "delegate",
            "task",
            json!({"description":"scoped worker","prompt":"WORKER_SCOPE_MARKER"}),
        ),
        done(),
    ]);
    model.push_marker_turns(
        "WORKER_SCOPE_MARKER",
        [
            tool("root-output", "read_file", json!({"path":own_output})),
            tool("root-attachment", "read_file", json!({"path":uri})),
            tool("root-grep", "grep", json!({"path":uri,"pattern":"PRIVATE"})),
            tool(
                "foreign-output",
                "read_file",
                json!({"path":foreign_output}),
            ),
            done(),
        ],
    );
    let events = run(&app, &root, "delegate").await;
    assert_eq!(approvals(&events), 1, "{events:?}");
    let requested = events
        .iter()
        .find(|event| event["type"] == "approval_requested")
        .unwrap();
    let child = requested["data"]["agent_id"].as_str().unwrap();
    let rows = oct_agent::db::messages::list_messages(&app.pool, child)
        .await
        .unwrap();
    let results: Vec<_> = rows
        .iter()
        .filter(|row| row.role == "tool")
        .map(|row| row.to_message().unwrap())
        .collect();
    assert_eq!(results.len(), 4);
    for (index, message) in results.iter().enumerate() {
        let oct_llm_provider::core::ContentPart::ToolResult(result) = &message.parts[0] else {
            panic!("tool result")
        };
        assert_eq!(result.is_error, index == 3, "{result:?}");
    }
}
