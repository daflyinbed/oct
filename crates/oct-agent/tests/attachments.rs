//! 附件端到端：multipart 上传 → staged 行 → send 认领装配 → 请求水合 /
//! 门控 → sweep 回收。LLM 是本地 mock，存储用临时文件 SQLite。

mod support;

use base64::Engine;
use oct_agent::attachments::{TEXT_INLINE_MAX_BYTES, text_inline_token_budget};
use serde_json::Value;
use sqlx::Row;

use oct_llm_provider::core::{ContentPart, Message, Role};
use support::TestApp;
use support::llm::{self, LlmTurn, finish_chunk, text_chunk};

/// A fully decodable PNG: ingress validation now checks the complete image.
fn png_bytes() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

async fn upload(
    app: &TestApp,
    conv_id: &str,
    filename: &str,
    mime: &str,
    bytes: Vec<u8>,
) -> reqwest::Response {
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(mime)
            .expect("mime"),
    );
    app.post_multipart(&format!("/api/conversations/{conv_id}/attachments"), form)
        .await
}

async fn stored_messages(app: &TestApp, conv_id: &str) -> Vec<Value> {
    let resp = app
        .get_json(&format!("/api/conversations/{conv_id}/messages"))
        .await;
    assert_eq!(resp.status(), 200);
    resp.json::<Vec<Value>>().await.expect("messages json")
}

/// 发送一条带附件的消息并 drain SSE 到 EOF。
async fn send_with_attachments(app: &TestApp, conv_id: &str, content: &str, ids: &[String]) {
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({
                "content": content,
                "provider_spec": "mock-llm:test-model",
                "attachment_ids": ids,
            }),
        )
        .await;
    assert_eq!(resp.status(), 200, "send should succeed");
    app.read_sse(resp).await;
}

#[tokio::test]
async fn image_upload_roundtrip_and_send_hydrates_data_url() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("看到了"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let bytes = png_bytes();
    let resp = upload(&app, &conv_id, "shot.png", "image/png", bytes.clone()).await;
    assert_eq!(resp.status(), 201, "upload should succeed");
    let dto = resp.json::<Value>().await.expect("dto json");
    let id = dto["id"].as_str().expect("id").to_string();
    assert_eq!(dto["filename"], "shot.png");
    assert_eq!(dto["mime_type"], "image/png");
    assert_eq!(dto["size_bytes"], bytes.len() as i64);

    // 字节读取：内容一致 + immutable 缓存头。
    let read = app.get_raw(&format!("/api/attachments/{id}")).await;
    assert_eq!(read.status(), 200);
    assert_eq!(read.headers()["content-type"], "image/png");
    assert_eq!(read.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        read.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        read.bytes().await.expect("body bytes").as_ref(),
        bytes.as_slice()
    );

    // 发送：user 消息 = 正文 Text + ImageUrl 引用；details 带展示元数据。
    send_with_attachments(&app, &conv_id, "看这张图", std::slice::from_ref(&id)).await;

    let stored = stored_messages(&app, &conv_id).await;
    let user = stored
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message");
    assert_eq!(user["parts"][0]["type"], "text");
    assert_eq!(user["parts"][0]["text"], "看这张图");
    assert_eq!(user["parts"][1]["type"], "image_url");
    assert_eq!(user["parts"][1]["url"], format!("oct-attachment://{id}"));
    let details = &user["details"]["extra"]["attachments"];
    assert_eq!(details.as_array().expect("attachments").len(), 1);
    assert_eq!(details[0]["id"], id.as_str());
    assert_eq!(details[0]["filename"], "shot.png");

    // 模型请求里引用已水合为 data URL（base64 与原始字节一致）。
    let requests = llm_server.requests();
    assert_eq!(requests.len(), 1);
    let content = &requests[0]["messages"][1]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "看这张图");
    assert_eq!(content[1]["type"], "image_url");
    let expected = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    assert_eq!(content[1]["image_url"]["url"], expected.as_str());
}

#[tokio::test]
async fn text_file_is_inlined_into_the_prompt() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = upload(
        &app,
        &conv_id,
        "notes.txt",
        "text/plain",
        b"hello oct".to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    send_with_attachments(&app, &conv_id, "这是什么", &[id]).await;

    let requests = llm_server.requests();
    let content = &requests[0]["messages"][1]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "这是什么");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "[attachment: notes.txt]\nhello oct");
}

#[tokio::test]
async fn missing_text_bytes_reject_the_send_and_keep_the_entire_batch_staged() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let mut ids = Vec::new();
    for (filename, mime, bytes) in [
        ("shot.png", "image/png", png_bytes()),
        ("notes.txt", "text/plain", b"important notes".to_vec()),
    ] {
        let response = upload(&app, &conv, filename, mime, bytes).await;
        assert_eq!(response.status(), 201);
        ids.push(
            response.json::<Value>().await.unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let path = app.attachments.upload_path(&conv, &ids[1]);
    tokio::fs::remove_file(&path).await.unwrap();
    let response = app
        .post_json(
            &format!("/api/conversations/{conv}/messages"),
            serde_json::json!({
                "content": "read these",
                "provider_spec": "mock-llm:test-model",
                "attachment_ids": ids,
            }),
        )
        .await;
    assert_eq!(response.status(), 400);
    assert!(
        response.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("notes.txt")
    );
    assert!(model.requests().is_empty());
    assert!(stored_messages(&app, &conv).await.is_empty());
    for id in &ids {
        assert_eq!(
            oct_agent::db::attachments::get(&app.pool, id)
                .await
                .unwrap()
                .unwrap()
                .state,
            "staged"
        );
    }
    // Restoring the bytes permits retrying the exact same ids.
    tokio::fs::write(path, b"important notes").await.unwrap();
    send_with_attachments(&app, &conv, "read these", &ids).await;
    assert_eq!(model.requests().len(), 1);
    assert_eq!(
        model.requests()[0]["messages"][1]["content"][2]["text"],
        "[attachment: notes.txt]\nimportant notes"
    );
}

#[tokio::test]
async fn pending_uploads_survive_sweeps_without_blocking_other_conversations() {
    use tokio::io::AsyncWriteExt;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let pending_conv = app.create_conversation(&project).await;
    let deleting_conv = app.create_conversation(&project).await;
    let other_conv = app.create_conversation(&project).await;
    let store = app.attachments.as_ref().clone();
    let id = uuid::Uuid::new_v4().to_string();
    let (pending, mut file) = store.begin_upload(&pending_conv, &id).await.unwrap();
    file.write_all(b"partial upload").await.unwrap();
    file.sync_all().await.unwrap();
    let path = store.upload_path(&pending_conv, &format!("{id}.part"));

    // Keep the upload alive across the real deletion's global sweep. A
    // publication read lock held over this interval would deadlock it.
    let deletion = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        app.http_delete(&format!(
            "/api/projects/{project}/conversations/{deleting_conv}"
        )),
    )
    .await
    .expect("pending uploads must not block deletion");
    assert_eq!(deletion.status(), 204);
    assert!(
        path.exists(),
        "a sweep through a store clone must preserve active files"
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        upload(
            &app,
            &other_conv,
            "other.txt",
            "text/plain",
            b"independent".to_vec(),
        ),
    )
    .await
    .expect("pending uploads must not block other uploads");
    assert_eq!(response.status(), 201);
    drop(file);
    drop(pending);
    assert!(!path.exists(), "abandoned partial uploads are cleaned up");
}

#[tokio::test]
async fn before_user_fork_and_revert_restore_attachments_as_sendable_copies() {
    // Exercise image-only requests and mixed text/image requests through
    // both history operations, including receipt replay and source deletion.
    for revert in [false, true] {
        for mixed in [false, true] {
            let app = TestApp::new().await;
            let model = llm::spawn_llm_mock(vec![
                LlmTurn::Complete(vec![text_chunk("original"), finish_chunk("stop")]),
                LlmTurn::Complete(vec![text_chunk("restored"), finish_chunk("stop")]),
            ])
            .await;
            app.register_llm_provider(&model).await;
            let project = app.create_project().await;
            let source = app.create_conversation(&project).await;
            let image = upload(&app, &source, "shot.png", "image/png", png_bytes())
                .await
                .json::<Value>()
                .await
                .unwrap();
            let mut originals = vec![image];
            if mixed {
                originals.push(
                    upload(
                        &app,
                        &source,
                        "notes.txt",
                        "text/plain",
                        b"attachment body".to_vec(),
                    )
                    .await
                    .json::<Value>()
                    .await
                    .unwrap(),
                );
            }
            let ids = originals
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let content = if mixed { "editable request" } else { "" };
            send_with_attachments(&app, &source, content, &ids).await;
            let snapshot = app
                .get_json(&format!("/api/conversations/{source}/messages"))
                .await;
            let revision: i64 = snapshot.headers()["x-context-revision"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let history = snapshot.json::<Vec<Value>>().await.unwrap();
            let user_id = history.iter().find(|row| row["role"] == "user").unwrap()["id"]
                .as_str()
                .unwrap();
            let operation = if revert { "revert" } else { "fork" };
            let request = serde_json::json!({
                "operation_id": uuid::Uuid::new_v4().to_string(),
                "expected_revision": revision,
                "cut": { "kind": "message", "message_id": user_id, "side": "before" },
            });
            let response = app
                .post_json(
                    &format!("/api/conversations/{source}/{operation}"),
                    &request,
                )
                .await;
            assert_eq!(response.status(), if revert { 200 } else { 201 });
            let result = response.json::<Value>().await.unwrap();
            assert_eq!(
                result["draft"], content,
                "attachment bodies must not be duplicated in the draft"
            );
            assert_eq!(result["draft_attachments"], serde_json::json!(originals));
            let target = result["conversation"]["id"].as_str().unwrap();
            let mut copies = Vec::new();
            for id in &ids {
                let response = app
                    .post_json(
                        &format!("/api/conversations/{target}/attachments/{id}/copy"),
                        serde_json::json!(null),
                    )
                    .await;
                assert_eq!(response.status(), 201);
                let copy = response.json::<Value>().await.unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                assert!(!ids.contains(&copy));
                let row = oct_agent::db::attachments::get(&app.pool, &copy)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(row.conversation_id, target);
                assert_eq!(row.state, "staged");
                copies.push(copy);
            }
            let replay = app
                .post_json(
                    &format!("/api/conversations/{source}/{operation}"),
                    &request,
                )
                .await
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(replay, result);
            let to_delete = if revert {
                result["backup_conversation_id"].as_str().unwrap()
            } else {
                source.as_str()
            };
            let deletion = app
                .http_delete(&format!(
                    "/api/projects/{project}/conversations/{to_delete}"
                ))
                .await;
            assert_eq!(deletion.status(), 204);
            assert_eq!(
                app.get_raw(&format!("/api/attachments/{}", ids[0]))
                    .await
                    .status(),
                200 // the new ticket pins the SAME immutable asset
            );
            assert_eq!(
                app.get_raw(&format!("/api/attachments/{}", copies[0]))
                    .await
                    .bytes()
                    .await
                    .unwrap()
                    .as_ref(),
                png_bytes().as_slice()
            );
            send_with_attachments(&app, target, content, &copies).await;
            let requests = model
                .requests()
                .into_iter()
                .filter(|request| request["stream"] == true)
                .collect::<Vec<_>>();
            assert_eq!(requests.len(), 2);
            let parts = requests[1]["messages"][1]["content"].as_array().unwrap();
            assert_eq!(parts.len(), if mixed { 3 } else { 1 });
            if mixed {
                assert_eq!(parts[0]["text"], content);
                assert_eq!(parts[2]["text"], "[attachment: notes.txt]\nattachment body");
            }
        }
    }
}

#[tokio::test]
async fn failed_message_persistence_keeps_the_attachment_batch_staged_for_retry() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let mut ids = Vec::new();
    for (filename, mime, bytes) in [
        ("notes.txt", "text/plain", b"hello".to_vec()),
        ("shot.png", "image/png", png_bytes()),
    ] {
        let response = upload(&app, &conv, filename, mime, bytes).await;
        assert_eq!(response.status(), 201);
        ids.push(
            response.json::<Value>().await.unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    // Fail after the content row was inserted, while appending its membership.
    // Claims, content, membership and revision must all roll back together.
    sqlx::query(
        "CREATE TRIGGER attachment_fail_membership BEFORE INSERT ON conversation_messages \
         BEGIN SELECT RAISE(FAIL, 'simulated message persistence failure'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    let body = serde_json::json!({
        "content": "read the attachments",
        "provider_spec": "mock-llm:test-model",
        "attachment_ids": ids,
        "expected_revision": 0,
    });
    let response = app
        .post_json(&format!("/api/conversations/{conv}/messages"), body.clone())
        .await;
    assert_eq!(response.status(), 500);
    assert!(stored_messages(&app, &conv).await.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "failed membership must not orphan a content row");
    for id in &ids {
        let row = oct_agent::db::attachments::get(&app.pool, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "staged");
    }
    app.attachments.sweep().await;
    for id in &ids {
        assert_eq!(
            app.get_raw(&format!("/api/attachments/{id}"))
                .await
                .status(),
            200
        );
    }

    sqlx::query("DROP TRIGGER attachment_fail_membership")
        .execute(&app.pool)
        .await
        .unwrap();
    // Retry the same ids and expected revision, without another upload.
    let response = app
        .post_json(&format!("/api/conversations/{conv}/messages"), body)
        .await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    assert_eq!(model.requests().len(), 1);
    for id in &ids {
        let row = oct_agent::db::attachments::get(&app.pool, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "sent");
    }
}

#[tokio::test]
async fn partial_attachment_claim_failure_rolls_back_the_whole_batch() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let response = upload(&app, &conv, "notes.txt", "text/plain", b"hello".to_vec()).await;
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = upload(&app, &conv, "second.txt", "text/plain", b"second".to_vec()).await;
    let second = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let prepared = app
        .attachments
        .prepare_message(
            &conv,
            "read the attachments",
            &[id.clone(), second.clone()],
            None,
        )
        .await
        .unwrap();
    // Another commit consumes the second ticket after our read phase. The
    // first claim of our failed transaction must roll back as well.
    app.attachments
        .prepare_message(&conv, "other send", std::slice::from_ref(&second), None)
        .await
        .unwrap()
        .commit("mock", "test", serde_json::json!({}))
        .await
        .unwrap();
    let result = prepared.commit("mock", "test", serde_json::json!({})).await;
    assert!(matches!(
        result,
        Err(oct_agent::attachments::ServiceError::Invalid(_))
    ));
    assert_eq!(
        oct_agent::db::attachments::get(&app.pool, &id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "staged"
    );
    assert_eq!(stored_messages(&app, &conv).await.len(), 1);
}

#[tokio::test]
async fn run_creation_failure_keeps_attachment_receipt_and_can_resume_without_resending() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("resumed"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let response = upload(
        &app,
        &conv,
        "notes.txt",
        "text/plain",
        b"important contents".to_vec(),
    )
    .await;
    assert_eq!(response.status(), 201);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    sqlx::query("CREATE TRIGGER attachment_fail_run BEFORE INSERT ON agent_runs BEGIN SELECT RAISE(FAIL, 'simulated run persistence failure'); END")
        .execute(&app.pool).await.unwrap();
    let response = app
        .post_json(
            &format!("/api/conversations/{conv}/messages"),
            serde_json::json!({
                "content": "read notes", "provider_spec": "mock-llm:test-model",
                "client_request_id": "attachment-committed-error", "attachment_ids": [id],
            }),
        )
        .await;
    assert_eq!(response.status(), 500);
    let stored = stored_messages(&app, &conv).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0]["details"]["extra"]["client_request_id"],
        "attachment-committed-error"
    );
    assert_eq!(
        oct_agent::db::attachments::get(&app.pool, &id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "sent"
    );
    let status = app
        .get_json(&format!("/api/conversations/{conv}/run"))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["running"], false);
    assert_eq!(status["resumable"], true);

    sqlx::query("DROP TRIGGER attachment_fail_run")
        .execute(&app.pool)
        .await
        .unwrap();
    let response = app
        .post_json(
            &format!("/api/conversations/{conv}/resume"),
            serde_json::json!({}),
        )
        .await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    assert_eq!(model.requests().len(), 1);
    assert!(
        model.requests()[0]
            .to_string()
            .contains("important contents")
    );
    let stored = stored_messages(&app, &conv).await;
    assert_eq!(stored.iter().filter(|row| row["role"] == "user").count(), 1);
}

#[tokio::test]
async fn text_attachments_share_a_window_relative_budget_in_the_first_model_request() {
    // Cover Unicode, mixed text, multiple files, and uncatalogued models.
    for (window, file_count, unit) in [
        (Some(8192u32), 1, "汉"),
        (Some(8192), 8, "abc汉😀"),
        (None, 8, "a"),
    ] {
        let app = TestApp::new().await;
        let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
            text_chunk("ok"),
            finish_chunk("stop"),
        ])])
        .await;
        app.register_llm_provider(&model).await;
        if let Some(window) = window {
            sqlx::query(
                "INSERT INTO models (id, provider_id, model_id, name, limit_context, source) \
                 VALUES ('mock-llm:test-model', 'mock-llm', 'test-model', 'Test', $1, 'custom')",
            )
            .bind(i64::from(window))
            .execute(&app.pool)
            .await
            .unwrap();
        }
        let project = app.create_project().await;
        let conv = app.create_conversation(&project).await;
        let bytes = unit
            .repeat(TEXT_INLINE_MAX_BYTES / unit.len() + 1)
            .into_bytes();
        let mut ids = Vec::new();
        for index in 0..file_count {
            let response = upload(
                &app,
                &conv,
                &format!("notes-{index}.txt"),
                "text/plain",
                bytes.clone(),
            )
            .await;
            assert_eq!(response.status(), 201);
            ids.push(
                response.json::<Value>().await.unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        send_with_attachments(&app, &conv, "read all the files", &ids).await;
        let requests = model.requests();
        assert_eq!(
            requests.len(),
            1,
            "the first turn cannot rely on compaction"
        );
        let content = requests[0]["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), file_count + 1);
        let mut tokens = 0;
        for (part, id) in content[1..].iter().zip(&ids) {
            let text = part["text"].as_str().unwrap();
            tokens += oct_agent::context::estimate_str(text);
            assert!(text.contains("[truncated at"));
            assert!(text.contains(&format!("oct-attachment://{id}")));
            assert!(
                text.contains(unit),
                "each file receives some inline body space"
            );
        }
        assert!(
            tokens <= text_inline_token_budget(window),
            "{tokens} tokens exceed the shared budget"
        );
        if let Some(window) = window {
            assert!(oct_agent::context::estimate_str(&requests[0].to_string()) < u64::from(window));
        }
        // Budgeting must preserve the complete stored bytes for read_file.
        for id in &ids {
            let response = app.get_raw(&format!("/api/attachments/{id}")).await;
            assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn uploads_create_private_files_and_harden_existing_attachment_directories() {
    use std::os::unix::fs::PermissionsExt;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let conv_dir = app.attachments.root().join(&conv);
    tokio::fs::create_dir_all(&conv_dir).await.unwrap();
    for path in [app.attachments.root(), conv_dir.as_path()] {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
    }
    let response = upload(
        &app,
        &conv,
        "notes.txt",
        "text/plain",
        b"private contents".to_vec(),
    )
    .await;
    assert_eq!(response.status(), 201);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    for path in [app.attachments.root(), conv_dir.as_path()] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let file = app.attachments.upload_path(&conv, &id);
    assert_eq!(
        std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn html_and_xml_attachments_are_served_as_plain_text() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let bytes = b"<!doctype html><script>document.title='attachment'</script>";
    for mime in [
        "text/html",
        "application/xhtml+xml",
        "application/octet-stream",
    ] {
        let response = upload(&app, &conv, "page.html", mime, bytes.to_vec()).await;
        assert_eq!(response.status(), 201);
        let dto = response.json::<Value>().await.unwrap();
        let id = dto["id"].as_str().unwrap();
        let response = app.get_raw(&format!("/api/attachments/{id}")).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    }
}

#[tokio::test]
async fn long_text_attachment_survives_sweeps_and_forks_until_its_last_reference_dies() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let other = app.create_conversation(&project).await;
    let bytes = vec![b'x'; TEXT_INLINE_MAX_BYTES + 20];
    let response = upload(&app, &conv, "large.txt", "text/plain", bytes.clone()).await;
    assert_eq!(response.status(), 201);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    send_with_attachments(
        &app,
        &conv,
        "Read the complete file",
        std::slice::from_ref(&id),
    )
    .await;

    let snapshot = app
        .get_json(&format!("/api/conversations/{conv}/messages"))
        .await;
    let revision: i64 = snapshot.headers()["x-context-revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let messages = snapshot.json::<Vec<Value>>().await.unwrap();
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    let view = oct_agent::db::messages::load_messages_for_llm(&app.pool, &conv)
        .await
        .unwrap();
    let ContentPart::Text(text) = &view[0].parts[1] else {
        panic!("expected frozen text attachment")
    };
    assert!(text.contains("[truncated at"));
    assert_eq!(user["user_content"]["body"], "Read the complete file");
    assert!(user["details"]["extra"].get("user_content").is_none());
    assert!(text.contains(&format!("oct-attachment://{id}")));
    assert_eq!(user["details"]["extra"]["attachments"][0]["id"], id);

    // 启动回收和删除无关会话都必须保留文本附件的全文。
    app.attachments.sweep().await;
    let deletion = app
        .http_delete(&format!("/api/projects/{project}/conversations/{other}"))
        .await;
    assert_eq!(deletion.status(), 204);
    let response = app.get_raw(&format!("/api/attachments/{id}")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());

    // 真 fork 共享同一内容行与 details；删来源后仍可读，删最后引用才回收。
    let fork = app
        .post_json(
            &format!("/api/conversations/{conv}/fork"),
            serde_json::json!({
                "operation_id": uuid::Uuid::new_v4().to_string(),
                "expected_revision": revision,
                "cut": { "kind": "head" },
            }),
        )
        .await;
    assert_eq!(fork.status(), 201);
    let fork_id = fork.json::<Value>().await.unwrap()["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let deletion = app
        .http_delete(&format!("/api/projects/{project}/conversations/{conv}"))
        .await;
    assert_eq!(deletion.status(), 204);
    let response = app.get_raw(&format!("/api/attachments/{id}")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    let deletion = app
        .http_delete(&format!("/api/projects/{project}/conversations/{fork_id}"))
        .await;
    assert_eq!(deletion.status(), 204);
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{id}"))
            .await
            .status(),
        404
    );
    assert!(!app.attachments.upload_path(&conv, &id).exists());
}

#[tokio::test]
async fn upload_publication_across_service_clones_survives_a_concurrent_sweep() {
    use tokio::io::AsyncWriteExt;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let service = app.attachments.as_ref().clone();
    let id = uuid::Uuid::new_v4().to_string();
    let (upload, mut file) = service.begin_upload(&conv, &id).await.unwrap();
    file.write_all(b"hello").await.unwrap();
    file.sync_all().await.unwrap();
    drop(file);
    let (published, ()) = tokio::join!(
        service.finish_upload(&conv, &id, "notes.txt", "text/plain", upload),
        app.attachments.sweep()
    );
    published.unwrap();
    let response = app.get_raw(&format!("/api/attachments/{id}")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn sweep_waits_for_claimed_attachment_message_publication() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let bytes = png_bytes();
    let response = upload(&app, &conv, "shot.png", "image/png", bytes.clone()).await;
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let prepared = app
        .attachments
        .prepare_message(&conv, "", std::slice::from_ref(&id), None)
        .await
        .unwrap();
    let message = prepared.content().to_message();
    let sweep = app.attachments.sweep();
    tokio::pin!(sweep);
    // Explicit polling observes the lease; no sleep-based race coordination.
    assert!(futures_util::poll!(&mut sweep).is_pending());
    prepared
        .commit("mock", "test", serde_json::json!({}))
        .await
        .unwrap();
    sweep.await;
    let hydrated = app.attachments.hydrate_messages(vec![message]).await;
    let ContentPart::ImageUrl { url } = &hydrated[0].parts[0] else {
        panic!("a published image must still hydrate");
    };
    assert_eq!(
        url,
        &format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    );
}

#[tokio::test]
async fn uploads_are_validated() {
    let app = TestApp::new().await;
    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    // v1 不收的显式类型。
    let resp = upload(&app, &conv_id, "clip.mp4", "video/mp4", b"0000".to_vec()).await;
    assert_eq!(resp.status(), 400);

    // 声称是 png 实为垃圾：magic 嗅探拒绝。
    let resp = upload(
        &app,
        &conv_id,
        "junk.png",
        "image/png",
        b"not a png".to_vec(),
    )
    .await;
    assert_eq!(resp.status(), 400);

    // 空文件。
    let resp = upload(&app, &conv_id, "empty.txt", "text/plain", Vec::new()).await;
    assert_eq!(resp.status(), 400);

    // 文本候选里的 NUL 字节（二进制伪装文本）。
    let resp = upload(
        &app,
        &conv_id,
        "bin.txt",
        "application/octet-stream",
        vec![b'h', 0, b'i'],
    )
    .await;
    assert_eq!(resp.status(), 400);

    // 无效 UTF-8 的文本。
    let resp = upload(&app, &conv_id, "bad.txt", "text/plain", vec![0xff, 0xfe]).await;
    assert_eq!(resp.status(), 400);

    // 超过 10 MiB：中间件硬顶（+slack）之内到达 handler，被精确上限拒绝。
    let oversize = vec![b'a'; 10 * 1024 * 1024 + 1];
    let resp = upload(&app, &conv_id, "big.txt", "text/plain", oversize).await;
    assert_eq!(resp.status(), 400);

    // 未知会话。
    let resp = upload(&app, "no-such-conv", "a.png", "image/png", png_bytes()).await;
    assert_eq!(resp.status(), 404);

    // 合法上传仍然可用（确认上面的拒绝没有误伤校验逻辑）。
    let resp = upload(&app, &conv_id, "ok.png", "image/png", png_bytes()).await;
    assert_eq!(resp.status(), 201);
}

#[tokio::test]
async fn claimed_attachment_cannot_be_reused_or_sent_from_another_conversation() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![text_chunk("一"), finish_chunk("stop")]),
        LlmTurn::Complete(vec![text_chunk("二"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_a = app.create_conversation(&project_id).await;
    let conv_b = app.create_conversation(&project_id).await;

    let resp = upload(&app, &conv_a, "a.png", "image/png", png_bytes()).await;
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 外会话引用：整体 400，行保持 staged。
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_b}/messages"),
            serde_json::json!({
                "content": "hi",
                "provider_spec": "mock-llm:test-model",
                "attachment_ids": [id],
            }),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // 首次发送成功；同一 id 再发：已被认领，400。
    send_with_attachments(&app, &conv_a, "第一条", std::slice::from_ref(&id)).await;
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_a}/messages"),
            serde_json::json!({
                "content": "第二条",
                "provider_spec": "mock-llm:test-model",
                "attachment_ids": [id],
            }),
        )
        .await;
    assert_eq!(resp.status(), 400);

    // 未知 id 同样 400。
    let resp = app
        .post_json(
            &format!("/api/conversations/{conv_a}/messages"),
            serde_json::json!({
                "content": "第三条",
                "provider_spec": "mock-llm:test-model",
                "attachment_ids": ["00000000-0000-0000-0000-000000000000"],
            }),
        )
        .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn sweep_keeps_referenced_and_reclaims_expired_staged_and_orphans() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("好"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm_server).await;
    let project_id = app.create_project().await;
    let conv_a = app.create_conversation(&project_id).await;

    // 已发送并被消息引用的图片。
    let resp = upload(&app, &conv_a, "kept.png", "image/png", png_bytes()).await;
    let kept_id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    send_with_attachments(&app, &conv_a, "留着的", std::slice::from_ref(&kept_id)).await;

    // staged 未发送：回拨 created_at 模拟超过 TTL。
    let resp = upload(&app, &conv_a, "expired.png", "image/png", png_bytes()).await;
    let expired_id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    sqlx::query(
        "UPDATE staged_uploads SET created_at = datetime('now', '-25 hours') WHERE id = $1",
    )
    .bind(&expired_id)
    .execute(&app.pool)
    .await
    .expect("backdate");

    // 无行孤儿文件（行被历史 sweep 清掉后留下的字节）。
    let orphan_path = app
        .dir
        .path()
        .join("attachments")
        .join(&conv_a)
        .join("orphan");
    std::fs::write(&orphan_path, b"orphan bytes").expect("write orphan");

    // fork 共享模拟：另一个会话的 user 消息引用同一附件（fork 复制的是
    // membership，内容行/引用不变——直接造一条引用消息等价地建立引用）。
    let conv_b = app.create_conversation(&project_id).await;
    let shared = Message {
        role: Role::User,
        parts: vec![ContentPart::ImageUrl {
            url: format!("oct-attachment://{kept_id}"),
        }],
    };
    oct_agent::db::messages::insert_message(&app.pool, &conv_b, &shared, None, None, None)
        .await
        .expect("seed referencing message");

    // 删除上传会话（attachments 行无外键级联——删除不触碰行），然后 sweep。
    let resp = app
        .http_delete(&format!(
            "/api/projects/{project_id}/conversations/{conv_a}"
        ))
        .await;
    assert_eq!(resp.status(), 204, "conversation delete should succeed");
    oct_agent::attachments::AttachmentService::new(
        app.dir.path().join("attachments"),
        app.pool.clone(),
    )
    .sweep()
    .await;

    // 被引用的行与文件存活：跨会话引用仍然可读。
    let read = app.get_raw(&format!("/api/attachments/{kept_id}")).await;
    assert_eq!(read.status(), 200, "referenced attachment must survive");

    // 过期 staged 的行与文件已回收。
    let row = sqlx::query("SELECT COUNT(*) AS n FROM staged_uploads WHERE id = $1")
        .bind(&expired_id)
        .fetch_one(&app.pool)
        .await
        .expect("count expired");
    let n: i64 = row.get("n");
    assert_eq!(n, 0, "expired staged row must be reclaimed");
    assert!(
        !app.dir
            .path()
            .join("attachments")
            .join(conv_a)
            .join(&expired_id)
            .exists(),
        "expired staged file must be removed"
    );

    // 无行且无引用的孤儿文件已回收。
    assert!(!orphan_path.exists(), "orphan file must be removed");
}

#[tokio::test]
async fn text_only_model_gets_a_note_instead_of_the_image() {
    let app = TestApp::new().await;
    let llm_server = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("好的"),
        finish_chunk("stop"),
    ])])
    .await;
    // 模型行带 modalities_input=["text"]：直接插 DB（POST 端点只建
    // openai_compatible provider，模型行补上模态元数据）。
    sqlx::query(
        "INSERT INTO providers (id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at) \
         VALUES ('mock-llm', 'Mock LLM', 'openai_compatible', $1, 'test-key', NULL, 'custom', datetime('now'), datetime('now'))",
    )
    .bind(&llm_server.url)
    .execute(&app.pool)
    .await
    .expect("insert provider");
    sqlx::query(
        "INSERT INTO models (id, provider_id, model_id, name, modalities_input, source) \
         VALUES ('mock-llm:test-model', 'mock-llm', 'test-model', 'Test Model', '[\"text\"]', 'custom')",
    )
    .execute(&app.pool)
    .await
    .expect("insert model");

    let project_id = app.create_project().await;
    let conv_id = app.create_conversation(&project_id).await;

    let resp = upload(&app, &conv_id, "shot.png", "image/png", png_bytes()).await;
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    send_with_attachments(&app, &conv_id, "看这张图", &[id]).await;

    // 存储侧仍是 ImageUrl 引用（门控只在请求侧）……
    let stored = stored_messages(&app, &conv_id).await;
    let user = stored.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(user["parts"][1]["type"], "image_url");

    // ……模型请求侧图片被替换为文本注记。
    let requests = llm_server.requests();
    let content = &requests[0]["messages"][1]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "看这张图");
    assert_eq!(content[1]["type"], "text");
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .contains("does not accept image input")
    );
}

#[tokio::test]
async fn normalization_preserves_original_bytes_and_sends_a_stable_bounded_derivative() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let mut image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2500, 2)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let original = image.into_inner();
    let response = upload(&app, &conv, "wide.png", "image/png", original.clone()).await;
    assert_eq!(response.status(), 201);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let asset = app.attachments.asset(&id).await.unwrap();
    assert_eq!(
        asset.normalizer_version,
        oct_agent::attachments::normalize::NORMALIZER_VERSION
    );
    assert!(asset.width.unwrap() <= 2000);
    assert_ne!(asset.storage_key, asset.model_storage_key);
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{id}"))
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        original
    );
    send_with_attachments(&app, &conv, "look", std::slice::from_ref(&id)).await;
    let requests = model.requests();
    let url = requests[0]["messages"][1]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap();
    let model_bytes = base64::engine::general_purpose::STANDARD
        .decode(url.split_once(',').unwrap().1)
        .unwrap();
    assert!(image::load_from_memory(&model_bytes).unwrap().width() <= 2000);
    assert_ne!(model_bytes, original);

    let corrupt = upload(
        &app,
        &conv,
        "broken.png",
        "image/png",
        b"\x89PNG\r\n\x1a\nfake-body".to_vec(),
    )
    .await;
    assert_eq!(
        corrupt.status(),
        400,
        "magic alone no longer validates an image"
    );
}

#[tokio::test]
async fn copied_draft_tickets_share_assets_and_expire_even_while_the_original_is_referenced() {
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("ok"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&model).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let target = app.create_conversation(&project).await;
    let response = upload(&app, &conv, "notes.txt", "text/plain", b"notes".to_vec()).await;
    let asset_id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send_with_attachments(&app, &conv, "read", std::slice::from_ref(&asset_id)).await;
    let ticket = app
        .attachments
        .copy_attachment(&target, &asset_id)
        .await
        .unwrap();
    let row = oct_agent::db::attachments::get(&app.pool, &ticket.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.asset_id, asset_id);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachment_assets")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "draft copies do not duplicate immutable bytes");
    sqlx::query("UPDATE staged_uploads SET created_at = datetime('now','-25 hours') WHERE id = $1")
        .bind(&ticket.id)
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(
        app.attachments
            .prepare_message(&target, "retry", std::slice::from_ref(&ticket.id), None)
            .await
            .is_err()
    );
    app.attachments.sweep().await;
    assert!(
        oct_agent::db::attachments::get(&app.pool, &ticket.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{asset_id}"))
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn frozen_text_and_logical_read_references_survive_storage_relocation() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let bytes = vec![b'x'; 40_000];
    let response = upload(&app, &conv, "notes.txt", "text/plain", bytes.clone()).await;
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.attachments
        .prepare_message(
            &conv,
            "original body",
            std::slice::from_ref(&id),
            Some(8192),
        )
        .await
        .unwrap()
        .commit("mock", "test", serde_json::json!({}))
        .await
        .unwrap();
    let before = oct_agent::db::messages::load_messages_for_llm(&app.pool, &conv)
        .await
        .unwrap();
    let old_root = app.attachments.root().to_path_buf();
    let new_root = app.dir.path().join("relocated");
    tokio::fs::rename(&old_root, &new_root).await.unwrap();
    let service =
        oct_agent::attachments::AttachmentService::new(new_root.clone(), app.pool.clone());
    let resources = service.resource_paths(&conv).await.unwrap();
    let roots =
        oct_agent::tools::workspace::WorkspaceRoots::new(app.dir.path().join("project"), vec![])
            .with_resource_paths(resources);
    let resolved = roots
        .resolve_path(&format!("oct-attachment://{id}"))
        .unwrap();
    assert!(resolved.path.starts_with(new_root));
    assert!(!resolved.outside);
    assert_eq!(tokio::fs::read(resolved.path).await.unwrap(), bytes);
    let after = oct_agent::db::messages::load_messages_for_llm(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(before, after, "relocation cannot change frozen prompt text");
    let ContentPart::Text(snapshot) = &after[0].parts[1] else {
        panic!("text snapshot")
    };
    assert!(snapshot.contains(&format!("oct-attachment://{id}")));
    assert!(!snapshot.contains(old_root.to_str().unwrap()));
    let api = stored_messages(&app, &conv).await;
    assert_eq!(api[0]["user_content"]["body"], "original body");
    assert!(api[0]["user_content"].get("snapshot").is_none());
    assert!(api[0]["details"]["extra"].get("user_content").is_none());
}

#[tokio::test]
async fn logical_resources_use_the_existing_read_and_read_only_permission_boundary() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let response = upload(
        &app,
        &conv,
        "notes.txt",
        "text/plain",
        b"FULL_FILE_MARKER\n".to_vec(),
    )
    .await;
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let resource = format!("oct-attachment://{id}");
    let model = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            llm::tool_call_chunk(
                "read-1",
                "read_file",
                &serde_json::json!({"path":resource}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![
            llm::tool_call_chunk(
                "write-1",
                "write_file",
                &serde_json::json!({"path":resource,"content":"changed"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&model).await;
    app.set_permission_mode(&project, &conv, "bypass").await;
    send_with_attachments(&app, &conv, "read it", std::slice::from_ref(&id)).await;
    let rows = oct_agent::db::messages::list_messages(&app.pool, &conv)
        .await
        .unwrap();
    let results = rows
        .iter()
        .filter(|row| row.role == "tool")
        .map(|row| row.to_message().unwrap())
        .collect::<Vec<_>>();
    let ContentPart::ToolResult(read) = &results[0].parts[0] else {
        panic!("read result")
    };
    assert!(!read.is_error);
    assert!(read.content.as_str().unwrap().contains("FULL_FILE_MARKER"));
    let ContentPart::ToolResult(write) = &results[1].parts[0] else {
        panic!("write result")
    };
    assert!(
        write.is_error,
        "runtime assets stay read-only even in bypass mode"
    );
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{id}"))
            .await
            .text()
            .await
            .unwrap(),
        "FULL_FILE_MARKER\n"
    );
}

#[tokio::test]
async fn tool_images_use_the_shared_normalizer_and_remain_reachable_through_cow_originals() {
    use std::sync::Arc;
    use support::mcp::{FakeMcpConnection, fake_tool};
    let app = TestApp::new().await;
    let model = llm::spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            llm::tool_call_chunk("shot-1", "mcp__media__shot", "{}"),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("seen"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&model).await;
    let configured=app.post_json("/api/mcp/servers",serde_json::json!({"name":"media","transport":"stdio","command":"oct-test-nonexistent"})).await;
    assert_eq!(configured.status(), 201);
    let server_id = configured.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.patch_json(
        "/api/settings",
        serde_json::json!({"mcp_tool_mode":"direct"}),
    )
    .await;
    let listing = vec![fake_tool("shot", Some("screenshot"))];
    let fake = Arc::new(FakeMcpConnection::new(listing.clone()).with_result(
        "shot",
        oct_mcp::McpCallResult {
            text: "screenshot".into(),
            is_error: false,
            structured_json: None,
            images: vec![oct_mcp::McpImage {
                mime_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(png_bytes()),
            }],
        },
    ));
    app.mcp
        .insert_connection_for_test(&server_id, "media", None, fake, listing);
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let other = app.create_conversation(&project).await;
    let resources = app.attachments.resource_paths(&conv).await.unwrap();
    let other_resources = app.attachments.resource_paths(&other).await.unwrap();
    assert!(resources.is_empty());
    app.set_permission_mode(&project, &conv, "bypass").await;
    let response = app.send_message(&conv, "take a screenshot").await;
    app.read_sse(response).await;
    let rows = oct_agent::db::messages::list_messages(&app.pool, &conv)
        .await
        .unwrap();
    let row = rows.iter().find(|row| row.role == "tool").unwrap();
    let tool = row.to_message().unwrap();
    let ContentPart::ToolResult(result) = &tool.parts[0] else {
        panic!("tool result")
    };
    assert_eq!(
        result.images.len(),
        1,
        "valid tool media must survive normalization"
    );
    let asset_id =
        oct_agent::attachment_types::parse_attachment_id(&result.images[0].data).unwrap();
    assert!(
        resources.contains_key(&result.images[0].data),
        "committed tool media must reach the run's existing resource map"
    );
    assert!(
        other_resources.is_empty(),
        "tool media stays conversation-scoped"
    );
    assert_eq!(
        app.attachments
            .asset(asset_id)
            .await
            .unwrap()
            .normalizer_version,
        oct_agent::attachments::normalize::NORMALIZER_VERSION
    );
    let wire = model.requests();
    let delivered = wire[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|part| {
            part["image_url"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        });
    assert!(
        delivered,
        "the provider gets bytes, never storage references"
    );
    oct_agent::db::messages::apply_cow_swaps(
        &app.pool,
        &conv,
        &[oct_agent::db::messages::CowSwap {
            call_id: "shot-1".into(),
            copy: oct_agent::agent::micro::cleared_copy(&tool),
            details: Some(serde_json::json!({"kind":"microcompact_cleared"})),
        }],
    )
    .await
    .unwrap();
    let reloaded = oct_agent::attachments::AttachmentService::new(
        app.attachments.root().to_path_buf(),
        app.pool.clone(),
    );
    assert!(
        reloaded
            .resource_paths(&conv)
            .await
            .unwrap()
            .contains_key(&result.images[0].data),
        "COW originals retain the conversation's attachment read grants"
    );
    app.attachments.sweep().await;
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{asset_id}"))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.http_delete(&format!("/api/projects/{project}/conversations/{conv}"))
            .await
            .status(),
        204
    );
    assert_eq!(
        app.get_raw(&format!("/api/attachments/{asset_id}"))
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn legacy_attachment_migration_backfills_ownership_without_rewriting_content() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let migrator = sqlx::migrate!("../../migrations");
    migrator.undo(&app.pool, 20261015120000).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let message_id = uuid::Uuid::new_v4().to_string();
    let parts = serde_json::to_string(&vec![
        ContentPart::Text("legacy body".into()),
        ContentPart::Text("[attachment: notes.txt]\nlegacy bytes".into()),
    ])
    .unwrap();
    let details=serde_json::json!({"attachments":[{"id":id,"filename":"notes.txt","mime_type":"text/plain","size_bytes":12}]}).to_string();
    sqlx::query("INSERT INTO attachments (id,conversation_id,filename,mime_type,size_bytes,state) VALUES ($1,$2,'notes.txt','text/plain',12,'sent')").bind(&id).bind(&conv).execute(&app.pool).await.unwrap();
    sqlx::query("INSERT INTO messages (id,role,parts_json,details_json) VALUES ($1,'user',$2,$3)")
        .bind(&message_id)
        .bind(&parts)
        .bind(&details)
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversation_messages (conversation_id,message_id,ordering,appended_at_revision) VALUES ($1,$2,1,1)").bind(&conv).bind(&message_id).execute(&app.pool).await.unwrap();
    migrator.run(&app.pool).await.unwrap();
    let indexed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_assets WHERE message_id=$1 AND asset_id=$2",
    )
    .bind(&message_id)
    .bind(&id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(indexed, 1);
    let stored = oct_agent::db::messages::list_messages(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(stored[0].parts_json, parts);
    assert_eq!(stored[0].details_json.as_deref(), Some(details.as_str()));
    let decoded =
        oct_agent::attachment_types::UserContent::from_stored(&parts, Some(&details)).unwrap();
    assert_eq!(decoded.body, "legacy body");
    assert_eq!(decoded.attachments[0].asset.id, id);
}

#[tokio::test]
async fn cow_reload_deducts_the_measured_projection_instead_of_omitted_images() {
    use oct_agent::attachments::RequestAssembler;
    use oct_agent::context::{ContextState, projection::MediaPolicy};
    use oct_agent::db::messages;
    use oct_llm_provider::core::{Message, Role, ToolCall, ToolResult, ToolResultImage};
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    let response = upload(&app, &conv, "shot.png", "image/png", png_bytes()).await;
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tool = Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: "shot".into(),
            content: serde_json::json!("tool output ".repeat(100)),
            is_error: false,
            images: vec![ToolResultImage {
                mime_type: "image/png".into(),
                data: format!("oct-attachment://{id}"),
            }],
        })],
    };
    let seed = [
        Message::text(Role::User, "go"),
        Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "shot".into(),
                name: "screenshot".into(),
                arguments: "{}".into(),
            })],
        },
        tool.clone(),
        Message::text(Role::Assistant, "round complete"),
    ];
    for message in &seed {
        messages::insert_message(&app.pool, &conv, message, None, None, None)
            .await
            .unwrap();
    }
    let policy = MediaPolicy {
        max_encoded_bytes: u64::MAX,
        max_images: 0,
    };
    let assembler = RequestAssembler::new(Some(&app.attachments), policy);
    let before = ContextState::new(
        "system".into(),
        messages::load_llm_view(&app.pool, &conv).await.unwrap(),
    );
    let plan = assembler.plan(&before).await.unwrap();
    assert_eq!(plan.image_count, 0);
    let measured = plan.snapshot.entry(3).unwrap().estimated_tokens;
    assert!(measured < oct_agent::context::estimate_message(&tool));
    let details = serde_json::json!({"projection":plan.snapshot,"input_modalities":oct_agent::modality::InputModalities::Unknown,"prompt_generation":1});
    let anchor = messages::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::Assistant, "anchor"),
        None,
        None,
        Some(&details),
    )
    .await
    .unwrap();
    messages::update_message_usage(&app.pool, &anchor.id, Some(10000), Some(10), None)
        .await
        .unwrap();
    let mut live = ContextState::new(
        "system".into(),
        messages::load_llm_view(&app.pool, &conv).await.unwrap(),
    );
    live.apply_projection(assembler.plan(&live).await.unwrap().snapshot);
    assert_eq!(live.anchor().unwrap().tokens, 10010);
    let placeholder = oct_agent::agent::micro::PLACEHOLDER_CONTENT;
    live.apply_clears(
        &[(2, oct_agent::context::estimate_message(&tool))],
        placeholder,
    );
    messages::apply_cow_swaps(
        &app.pool,
        &conv,
        &[messages::CowSwap {
            call_id: "shot".into(),
            copy: oct_agent::agent::micro::cleared_copy(&tool),
            details: Some(serde_json::json!({"kind":"microcompact_cleared"})),
        }],
    )
    .await
    .unwrap();
    let mut reloaded = ContextState::new(
        "system".into(),
        messages::load_llm_view(&app.pool, &conv).await.unwrap(),
    );
    assert_eq!(
        reloaded.anchor().unwrap().tokens,
        live.anchor().unwrap().tokens
    );
    reloaded.apply_projection(assembler.plan(&reloaded).await.unwrap().snapshot);
    assert!(
        reloaded.anchor().is_some(),
        "the adjusted COW prefix remains valid on reload"
    );
    assert_eq!(reloaded.estimate(&[]), live.estimate(&[]));
}

#[tokio::test]
async fn missing_image_usage_is_reused_only_while_the_delivered_prefix_is_unchanged() {
    use oct_agent::agent::{
        AgentContext, EventHub, RunHandle, RunStatus, loop_runner::run_agent_loop,
    };
    use oct_agent::db::messages;
    use oct_llm_provider::core::FinishReason;
    use std::sync::Arc;
    use support::{ScriptedModel, Turn, finish, text, tool_call, usage};

    for (reload, restored) in [(false, false), (true, false), (true, true)] {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let conv = app.create_conversation(&project).await;
        // Local estimates stay far below the 85k compaction threshold, but the history
        // exceeds the 10k verbatim tail so an accurate usage anchor can compact it.
        for _ in 0..8 {
            for role in [Role::User, Role::Assistant] {
                messages::insert_message(
                    &app.pool,
                    &conv,
                    &Message::text(role, "history ".repeat(750)),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            }
        }
        let response = upload(&app, &conv, "shot.png", "image/png", png_bytes()).await;
        assert_eq!(response.status(), 201);
        let id = response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let prepared = app
            .attachments
            .prepare_message(&conv, "inspect the image", std::slice::from_ref(&id), None)
            .await
            .unwrap();
        prepared
            .commit("scripted", "test-model", serde_json::json!({}))
            .await
            .unwrap();
        let asset = app.attachments.asset(&id).await.unwrap();
        let model_path = app.attachments.root().join(asset.model_storage_key);
        tokio::fs::remove_file(&model_path).await.unwrap();

        let context = |model: ScriptedModel, context_window| AgentContext {
            model: Arc::new(model),
            tools: Arc::new(
                oct_agent::tools::default_tools(oct_agent::tools::workspace::WorkspaceRoots::new(
                    app.dir.path().to_path_buf(),
                    vec![],
                ))
                .into_iter()
                .map(Arc::from)
                .collect(),
            ),
            pool: app.pool.clone(),
            system_prompt: "system".into(),
            context_window,
            idle: None,
            reasoning: None,
            input_modalities: oct_agent::modality::InputModalities::Unknown,
            max_output_tokens: None,
            attachments: Some(app.attachments.clone()),
            permission: oct_agent::permissions::gate::PermissionGate::permissive(),
            prompt_env_refresh: None,
            prompt_generation: 0,
        };
        let handle = || RunHandle {
            hub: Arc::new(EventHub::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        if reload {
            let first = ScriptedModel::new(vec![Turn::Stream(vec![
                text("first answer"),
                usage(90_000, 10),
                finish(FinishReason::Stop),
            ])]);
            let outcome = run_agent_loop(
                context(first, None),
                conv.clone(),
                messages::load_llm_view(&app.pool, &conv).await.unwrap(),
                handle(),
            )
            .await;
            assert_eq!(outcome.status, RunStatus::Completed);
            let view = messages::load_llm_view(&app.pool, &conv).await.unwrap();
            assert_eq!(view.anchor.unwrap().tokens, 90_010);
        }
        if restored {
            // A recovered image changes the delivered prefix and must drop
            // usage measured against the missing-file annotation.
            tokio::fs::write(&model_path, png_bytes()).await.unwrap();
        }
        let mut turns = Vec::new();
        if !reload {
            turns.push(Turn::Stream(vec![
                tool_call("check", "list_dir", serde_json::json!({"path":"."})),
                usage(90_000, 10),
                finish(FinishReason::ToolCalls),
            ]));
        }
        turns.push(Turn::Stream(vec![text("done"), finish(FinishReason::Stop)]));
        let model = ScriptedModel::new(turns);
        model.push_generation(Ok("Summary of earlier history".into()));
        let observer = model.observer();
        let run = handle();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_agent_loop(
                context(model, Some(100_000)),
                conv.clone(),
                messages::load_llm_view(&app.pool, &conv).await.unwrap(),
                run.clone(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(
            observer.generate_requests().len(),
            usize::from(!restored),
            "only unchanged measured prefixes trigger compaction (reload={reload}, restored={restored})"
        );
        assert_eq!(
            run.hub.snapshot().iter().any(|event| matches!(event,
                oct_agent::agent::AgentEvent::Compacted { est_tokens_before, .. } if *est_tokens_before >= 90_010
            )),
            !restored,
            "compaction must use matching measured usage (reload={reload}, restored={restored})"
        );
        let requests = observer.requests();
        assert_eq!(requests.len(), if reload { 1 } else { 2 });
        for request in &requests {
            assert_eq!(
                request
                    .messages
                    .iter()
                    .flat_map(|message| &message.parts)
                    .any(|part| matches!(
                        part, ContentPart::Text(note) if note.contains("could not be read")
                    )),
                !restored,
                "only missing images are replaced by omission notes"
            );
            assert_eq!(
                request
                    .messages
                    .iter()
                    .flat_map(|message| &message.parts)
                    .any(|part| matches!(part, ContentPart::ImageUrl { .. })),
                restored,
                "recovered images must be delivered to the provider"
            );
        }
    }
}

#[tokio::test]
async fn attachment_migration_refuses_a_lossy_downgrade_after_v2_content() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    app.attachments
        .prepare_message(&conv, "preserve this", &[], None)
        .await
        .unwrap()
        .commit("mock", "test", serde_json::json!({}))
        .await
        .unwrap();
    let migrator = sqlx::migrate!("../../migrations");
    let error = migrator
        .undo(&app.pool, 20261015120000)
        .await
        .expect_err("v2 content cannot be represented in v1");
    assert!(
        error
            .to_string()
            .contains("cannot_downgrade_v2_attachment_content")
    );
    let rows = oct_agent::db::messages::list_messages(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].to_message().unwrap().parts,
        vec![ContentPart::Text("preserve this".into())]
    );
    let assets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachment_assets")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        assets, 0,
        "the failed migration rolls back its schema changes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn orphan_collection_does_not_follow_directory_symlinks_outside_the_asset_root() {
    let app = TestApp::new().await;
    let external = app.dir.path().join("external-files");
    tokio::fs::create_dir(&external).await.unwrap();
    let file = external.join("keep.txt");
    tokio::fs::write(&file, b"keep this data").await.unwrap();
    app.attachments.ensure_root().await.unwrap();
    std::os::unix::fs::symlink(&external, app.attachments.root().join("stray-link")).unwrap();
    app.attachments.sweep().await;
    assert_eq!(tokio::fs::read(&file).await.unwrap(), b"keep this data");
}
