//! Legacy notification-order repair preserves immutable content and forks.
mod support;

use oct_agent::db::{background, history, messages};
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
use serde_json::json;
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};
use support::{TestApp, TestEnv, new_conversation};

async fn insert(pool: &sqlx::SqlitePool, id: &str, message: &Message) -> String {
    messages::insert_message(pool, id, message, None, None, None)
        .await
        .unwrap()
        .id
}

async fn notification(pool: &sqlx::SqlitePool, id: &str) -> String {
    messages::insert_message(
        pool,
        id,
        &Message::text(Role::User, "<task-notification>done</task-notification>"),
        None,
        None,
        Some(&json!({"kind": "background_task_notification"})),
    )
    .await
    .unwrap()
    .id
}

fn calls(ids: &[&str]) -> Message {
    Message {
        role: Role::Assistant,
        parts: ids
            .iter()
            .map(|id| {
                ContentPart::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                })
            })
            .collect(),
    }
}

fn result(id: &str) -> Message {
    Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: id.into(),
            content: json!("original result"),
            is_error: false,
            images: Vec::new(),
        })],
    }
}

async fn revision(pool: &sqlx::SqlitePool, id: &str) -> i64 {
    sqlx::query_scalar!(
        "SELECT context_revision FROM conversations WHERE id = $1",
        id
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn legacy_parallel_batch_repair_is_atomic_idempotent_and_fork_scoped() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let call = insert(&env.pool, &conv, &calls(&["a", "b"])).await;
    let a = insert(&env.pool, &conv, &result("a")).await;
    let b = insert(&env.pool, &conv, &result("b")).await;
    let note1 = notification(&env.pool, &conv).await;
    let note2 = notification(&env.pool, &conv).await;
    let user = insert(&env.pool, &conv, &Message::text(Role::User, "later user")).await;
    let original_revision = revision(&env.pool, &conv).await;
    let source = oct_agent::db::conversations::get_conversation(&env.pool, &conv)
        .await
        .unwrap()
        .unwrap();
    let fork = history::fork(
        &env.pool,
        &source,
        &oct_agent::history::ForkRequest {
            operation_id: "legacy-history-fork".into(),
            expected_revision: original_revision,
            cut: oct_agent::history::HistoryCut::Head,
            title: None,
        },
    )
    .await
    .unwrap();

    // Forks reject already-corrupt tool batches. Create a valid shared
    // timeline first, then reproduce legacy ordering in only the source.
    let mut tx = env.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let first = 1_i64;
    sqlx::query!(
        "UPDATE conversation_messages SET ordering = -ordering WHERE conversation_id = $1 AND ordering >= $2",
        conv, first,
    ).execute(&mut *tx).await.unwrap();
    for (index, id) in [&call, &note1, &a, &note2, &b, &user]
        .into_iter()
        .enumerate()
    {
        let ordering = index as i64 + 1;
        sqlx::query!(
            "UPDATE conversation_messages SET ordering = $1 WHERE conversation_id = $2 AND message_id = $3",
            ordering, conv, id,
        ).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let before = messages::list_messages(&env.pool, &conv).await.unwrap();
    let fork_before = messages::list_messages(&env.pool, &fork.conversation.id)
        .await
        .unwrap();
    let fork_revision = revision(&env.pool, &fork.conversation.id).await;

    assert!(
        messages::repair_interleaved_notifications(&env.pool, &conv)
            .await
            .unwrap()
    );
    let after = messages::list_messages(&env.pool, &conv).await.unwrap();
    assert_eq!(
        after.iter().map(|m| &m.id).collect::<Vec<_>>(),
        vec![&call, &a, &b, &note1, &note2, &user]
    );
    assert_eq!(revision(&env.pool, &conv).await, original_revision + 1);
    for old in &before {
        let new = after.iter().find(|m| m.id == old.id).unwrap();
        assert_eq!(new.parts_json, old.parts_json);
        assert_eq!(new.details_json, old.details_json);
        assert_eq!(new.appended_at_revision, old.appended_at_revision);
        assert_eq!(new.swapped_at_revision, old.swapped_at_revision);
    }
    let fork_rows = messages::list_messages(&env.pool, &fork.conversation.id)
        .await
        .unwrap();
    assert_eq!(
        fork_rows.iter().map(|m| &m.id).collect::<Vec<_>>(),
        fork_before.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    assert_eq!(
        revision(&env.pool, &fork.conversation.id).await,
        fork_revision
    );
    assert!(
        !messages::repair_interleaved_notifications(&env.pool, &conv)
            .await
            .unwrap()
    );
    assert_eq!(revision(&env.pool, &conv).await, original_revision + 1);
}

#[tokio::test]
async fn incomplete_and_user_interrupted_batches_are_not_reordered() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    insert(&env.pool, &conv, &calls(&["a", "b"])).await;
    notification(&env.pool, &conv).await;
    insert(&env.pool, &conv, &result("a")).await;
    assert!(
        !messages::repair_interleaved_notifications(&env.pool, &conv)
            .await
            .unwrap()
    );
    insert(&env.pool, &conv, &Message::text(Role::User, "actual input")).await;
    insert(&env.pool, &conv, &result("b")).await;
    let before = messages::list_messages(&env.pool, &conv).await.unwrap();
    let rev = revision(&env.pool, &conv).await;
    assert!(
        !messages::repair_interleaved_notifications(&env.pool, &conv)
            .await
            .unwrap()
    );
    assert_eq!(revision(&env.pool, &conv).await, rev);
    assert_eq!(
        messages::list_messages(&env.pool, &conv)
            .await
            .unwrap()
            .iter()
            .map(|m| &m.id)
            .collect::<Vec<_>>(),
        before.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn send_repairs_legacy_notification_even_when_no_tool_is_dangling() {
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("continued"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    insert(&app.pool, &conv, &calls(&["a"])).await;
    notification(&app.pool, &conv).await;
    insert(&app.pool, &conv, &result("a")).await;
    assert!(
        messages::find_dangling_tool_calls(&app.pool, &conv)
            .await
            .unwrap()
            .is_empty()
    );
    let response = app.send_message(&conv, "continue").await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let request = llm.requests().remove(0);
    let wire = request["messages"].as_array().unwrap();
    let roles: Vec<_> = wire.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "assistant", "tool", "user", "user"]);
}

#[tokio::test]
async fn cancellation_retires_only_the_admitted_preparing_wake() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let first = background::insert_notification_marking_tasks(
        &env.pool,
        &conv,
        &Message::text(Role::User, "first"),
        None,
        &[],
    )
    .await
    .unwrap();
    let second = background::insert_notification_marking_tasks(
        &env.pool,
        &conv,
        &Message::text(Role::User, "second"),
        None,
        &[],
    )
    .await
    .unwrap();
    background::cancel_preparing_wake(&env.pool, &conv, &first)
        .await
        .unwrap();
    assert_eq!(
        background::pending_wake(&env.pool, &conv).await.unwrap(),
        Some(second.clone())
    );
    let failed_attempt = oct_agent::db::runs::create_run(
        &env.pool,
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
    oct_agent::db::runs::finish_run(
        &env.pool,
        &failed_attempt.id,
        oct_agent::db::runs::RunFinalStatus::Failed,
        None,
        Some("temporary failure"),
        None,
        None,
        Some(failed_attempt.execution_epoch),
    )
    .await
    .unwrap();
    assert_eq!(
        background::pending_wake(&env.pool, &conv).await.unwrap(),
        Some(second.clone())
    );
    // Cancelling preparation of the retry also retires an intent still
    // linked to this previous failed attempt.
    background::cancel_preparing_wake(&env.pool, &conv, &second)
        .await
        .unwrap();
    assert!(
        background::pending_wake(&env.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cancelling_wake_during_oauth_preparation_releases_occupancy_without_retry() {
    use std::time::Duration;
    use support::oauth_as::OAuthAsMock;

    let llm = spawn_llm_mock(vec![]).await;
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    messages::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "original request"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();

    let oauth = OAuthAsMock::spawn().await;
    let mcp =
        support::mcp_http::McpHttpMock::spawn_with_oauth("Bearer oauth-access-1", &oauth.base_url)
            .await;
    let row: serde_json::Value = app
        .post_json(
            "/api/mcp/servers",
            json!({
                "name": "wake-oauth", "transport": "http", "url": mcp.url,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let server = row["id"].as_str().unwrap();
    let start: serde_json::Value = app
        .post_json(
            &format!("/api/mcp/servers/{server}/oauth/start"),
            json!(null),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        reqwest::Client::new()
            .get(start["authorization_url"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut token = oct_agent::db::mcp_tokens::get_mcp_server_token(&app.pool, server)
        .await
        .unwrap()
        .unwrap();
    token.expires_at = Some(chrono::Utc::now().naive_utc() - chrono::Duration::seconds(1));
    oct_agent::db::mcp_tokens::save_mcp_server_token(
        &app.pool,
        &oct_agent::db::mcp_tokens::SaveTokenRequest {
            server_id: token.server_id,
            client_id: token.client_id,
            client_secret: token.client_secret,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            token_type: token.token_type,
            scope: token.scope,
            expires_at: token.expires_at,
        },
    )
    .await
    .unwrap();
    mcp.set_expected_token("Bearer oauth-access-2");
    let (entered, release) = oauth.gate_next_refresh();
    background::insert_notification_marking_tasks(
        &app.pool,
        &conv,
        &Message::text(Role::User, "<task-notification>done</task-notification>"),
        None,
        &[],
    )
    .await
    .unwrap();
    app.background.kick_notification(&conv);
    tokio::time::timeout(Duration::from_secs(5), entered.cancelled())
        .await
        .unwrap();
    assert_eq!(app.cancel(&conv).await.status(), 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status: serde_json::Value = app.run_status(&conv).await.json().await.unwrap();
            if status["running"] == false {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancel must not wait for the shared OAuth refresh");
    assert!(
        background::pending_wake(&app.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
    release.cancel();
    app.mcp.ensure_connections(&app.pool, Some(&project)).await;
    assert!(
        oct_agent::db::runs::latest_run_for_conversation(&app.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        llm.requests().is_empty(),
        "a cancelled wake must not automatically retry the model"
    );
}

#[tokio::test]
async fn preparing_wake_cancel_retries_persistence_before_releasing_ownership() {
    use std::time::Duration;
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let message = background::insert_notification_marking_tasks(
        &env.pool,
        &conv,
        &Message::text(Role::User, "notification"),
        None,
        &[],
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_wake_cancel BEFORE DELETE ON wake_intents BEGIN SELECT RAISE(ABORT, 'temporary storage failure'); END")
        .execute(&env.pool).await.unwrap();
    let mut cancellation = Box::pin(background::cancel_preparing_wake(
        &env.pool, &conv, &message,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut cancellation)
            .await
            .is_err(),
        "storage failure must retain the cancellation owner instead of reviving the wake"
    );
    assert_eq!(
        background::pending_wake(&env.pool, &conv).await.unwrap(),
        Some(message.clone())
    );
    sqlx::raw_sql("DROP TRIGGER reject_wake_cancel")
        .execute(&env.pool)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), cancellation)
        .await
        .unwrap()
        .unwrap();
    assert!(
        background::pending_wake(&env.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
}
