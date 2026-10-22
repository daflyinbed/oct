//! Application commands exercise the same invariants without the HTTP adapter.
mod support;

use std::time::Duration;

use futures_util::StreamExt;
use oct_agent::agent::AgentEvent;
use oct_agent::application::conversation::{ConversationApplication, ResumeCommand, SendCommand};
use oct_agent::db::{messages, runs};
use oct_agent::error::AppError;
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall};
use serde_json::json;
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};

fn command(conversation_id: &str) -> SendCommand {
    SendCommand {
        conversation_id: conversation_id.into(),
        content: "durable request".into(),
        provider_spec: "mock-llm:test-model".into(),
        expected_revision: None,
        client_request_id: Some("application-receipt".into()),
        reasoning: None,
        attachment_ids: vec![],
    }
}

#[tokio::test]
async fn direct_send_checks_revision_before_repair_and_releases_occupancy() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    messages::insert_message(
        &app.pool,
        &conversation,
        &Message::text(Role::User, "interrupted request"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    messages::insert_message(
        &app.pool,
        &conversation,
        &Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "unanswered".into(),
                name: "read_file".into(),
                arguments: json!({"path":"missing.txt"}).to_string(),
            })],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let application = ConversationApplication::new(app.state.runs.clone());
    let mut request = command(&conversation);
    request.expected_revision = Some(0);
    let Err(error) = application.send(request).await else {
        panic!("stale command must fail before repairing existing history");
    };
    assert!(matches!(error, AppError::StaleRevision(_)));
    assert_eq!(
        messages::find_dangling_tool_calls(&app.pool, &conversation)
            .await
            .unwrap(),
        ["unanswered"]
    );
    assert_eq!(
        messages::count_user_messages(&app.pool, &conversation)
            .await
            .unwrap(),
        1
    );
    assert!(llm.requests().is_empty());
    assert!(
        runs::latest_run_for_conversation(&app.pool, &conversation)
            .await
            .unwrap()
            .is_none()
    );
    let _guard = app.state.runs.occupy(&conversation).unwrap();
}

#[tokio::test]
async fn direct_resume_continues_a_committed_send_after_assembly_failure() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("resumed through the application"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    let application = ConversationApplication::new(app.state.runs.clone());

    sqlx::query("ALTER TABLE permission_rules RENAME TO unavailable_rules")
        .execute(&app.pool)
        .await
        .unwrap();
    let result = application.send(command(&conversation)).await;
    sqlx::query("ALTER TABLE unavailable_rules RENAME TO permission_rules")
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(matches!(result, Err(AppError::Internal(_))));
    assert!(llm.requests().is_empty());
    let failed = runs::latest_run_for_conversation(&app.pool, &conversation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, "failed");

    let history = messages::list_messages(&app.pool, &conversation)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    let details: serde_json::Value =
        serde_json::from_str(history[0].details_json.as_deref().unwrap()).unwrap();
    assert_eq!(details["client_request_id"], "application-receipt");

    let subscription = application
        .resume(ResumeCommand {
            conversation_id: conversation.clone(),
        })
        .await
        .unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        subscription.events.collect::<Vec<_>>(),
    )
    .await
    .expect("application subscription must reach EOF after durable settlement");
    assert!(matches!(events.first(), Some(AgentEvent::RunMeta { .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Finish)));
    let resumed = runs::latest_run_for_conversation(&app.pool, &conversation)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(resumed.id, failed.id);
    assert_eq!(resumed.status, "completed");
    assert_eq!(llm.requests().len(), 1);
    assert_eq!(
        messages::count_user_messages(&app.pool, &conversation)
            .await
            .unwrap(),
        1
    );
    assert!(!app.state.sessions.contains_key(&conversation));
}
