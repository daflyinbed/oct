//! Message timestamps use temporary SQLite and scripted models; no real LLMs.
mod support;

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use futures_util::StreamExt;
use oct_agent::db::messages;
use oct_llm_provider::core::{Message, Role};
use serde_json::Value;
use std::time::Duration;
use support::TestApp;

async fn history(app: &TestApp, conversation: &str) -> Vec<Value> {
    app.get_json(&format!("/api/conversations/{conversation}/messages"))
        .await
        .json()
        .await
        .unwrap()
}

fn timestamp(row: &Value, field: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(row[field].as_str().expect("UTC timestamp")).unwrap()
}

fn assert_message_times(rows: &[Value]) {
    for row in rows {
        let created = NaiveDateTime::parse_from_str(
            row["created_at"].as_str().unwrap(),
            "%Y-%m-%dT%H:%M:%S%.f",
        )
        .unwrap();
        match row["role"].as_str().unwrap() {
            "user" => {
                assert_eq!(timestamp(row, "started_at").naive_utc(), created);
                assert!(row.get("finished_at").is_none());
            }
            "assistant" => {
                assert_eq!(timestamp(row, "finished_at").naive_utc(), created);
                assert!(row.get("started_at").is_none());
            }
            _ => {
                assert!(row.get("started_at").is_none());
                assert!(row.get("finished_at").is_none());
            }
        }
    }
}

#[tokio::test]
async fn history_timestamps_are_stable_without_run_rows() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    for (role, text) in [(Role::User, "question"), (Role::Assistant, "answer")] {
        messages::insert_message(
            &app.pool,
            &conversation,
            &Message::text(role, text),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let rows = history(&app, &conversation).await;
    assert_message_times(&rows);
    assert_eq!(rows, history(&app, &conversation).await);
}

#[tokio::test]
async fn reconnect_and_completion_keep_the_same_message_start() {
    let (gate_tx, gate_rx) = support::llm::gate();
    let model = support::llm::spawn_llm_mock(vec![support::llm::LlmTurn::Gated {
        pre: vec![support::llm::text_chunk("partial")],
        gate: gate_rx,
        post: vec![
            support::llm::text_chunk(" done"),
            support::llm::finish_chunk("stop"),
        ],
    }])
    .await;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    app.register_llm_provider(&model).await;
    let mut original = support::sse_payloads(app.send_message(&conversation, "question").await);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = original.next().await {
            if event["type"] == "text_delta" {
                return;
            }
        }
        panic!("model did not stream text");
    })
    .await
    .unwrap();
    let before = history(&app, &conversation).await;
    assert_eq!(before.len(), 1, "the live response is not persisted yet");
    assert_message_times(&before);
    let reconnected = app.run_events(&conversation).await;
    gate_tx.send(true).unwrap();
    let events = app.read_sse(reconnected).await;
    assert_eq!(events.first().unwrap()["type"], "run_meta");
    assert_eq!(events.last().unwrap()["type"], "finish");
    let _: Vec<_> = tokio::time::timeout(Duration::from_secs(10), original.collect())
        .await
        .unwrap();
    let after = history(&app, &conversation).await;
    assert_message_times(&after);
    assert_eq!(after[0]["started_at"], before[0]["started_at"]);
    assert!(timestamp(after.last().unwrap(), "finished_at") >= timestamp(&after[0], "started_at"));
}

#[tokio::test]
async fn cancellation_needs_no_finished_timestamp() {
    let model = support::llm::spawn_llm_mock(vec![support::llm::LlmTurn::Hang(vec![
        support::llm::text_chunk("unfinished"),
    ])])
    .await;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    app.register_llm_provider(&model).await;
    let mut stream = support::sse_payloads(app.send_message(&conversation, "question").await);
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["type"], "text_delta");
    assert_eq!(app.cancel(&conversation).await.status(), 200);
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(10), stream.collect())
        .await
        .unwrap();
    assert_eq!(events.last().unwrap()["type"], "cancelled");
    let rows = history(&app, &conversation).await;
    assert_eq!(rows.len(), 1);
    assert_message_times(&rows);
}

#[tokio::test]
async fn child_and_parent_messages_have_independent_timestamps() {
    let model = support::llm::spawn_llm_mock(vec![
        support::llm::LlmTurn::Complete(vec![
            support::llm::tool_call_chunk(
                "task-1",
                "task",
                r#"{"description":"worker","prompt":"answer the question"}"#,
            ),
            support::llm::finish_chunk("tool_calls"),
        ]),
        support::llm::LlmTurn::Complete(vec![
            support::llm::text_chunk("child done"),
            support::llm::finish_chunk("stop"),
        ]),
        support::llm::LlmTurn::Complete(vec![
            support::llm::text_chunk("parent done"),
            support::llm::finish_chunk("stop"),
        ]),
    ])
    .await;
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    app.register_llm_provider(&model).await;
    let events = app
        .read_sse(app.send_message(&conversation, "delegate").await)
        .await;
    let child = events
        .iter()
        .find(|event| event["type"] == "subagent_updated")
        .unwrap()["data"]["agent_id"]
        .as_str()
        .unwrap();
    let child_rows: Vec<Value> = app
        .get_json(&format!("/api/subagents/{child}/messages"))
        .await
        .json()
        .await
        .unwrap();
    let parent_rows = history(&app, &conversation).await;
    assert_message_times(&child_rows);
    assert_message_times(&parent_rows);
    assert_ne!(child_rows[0]["id"], parent_rows[0]["id"]);
}
