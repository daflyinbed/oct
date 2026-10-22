//! Startup ownership and completion ordering across the assembly/kernel boundary.
mod support;

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oct_agent::agent::{AgentEvent, EventHub, RunOutcome, RunStatus};
use oct_agent::application::conversation::{ConversationApplication, SendCommand};
use oct_agent::db::{messages, projects, runs};
use oct_agent::runtime::RootRunRequest;
use oct_agent::subagent::ChildRun;
use oct_llm_provider::core::{Message, Role};
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

async fn dropped_root_startup_waiter(cancel: bool) {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("startup still owns this run"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm).await;
    let project_id = app.create_project().await;
    let conversation = app.create_conversation(&project_id).await;
    let project = projects::get_project(&app.pool, &project_id)
        .await
        .unwrap()
        .unwrap();
    messages::insert_message(
        &app.pool,
        &conversation,
        &Message::text(Role::User, "owned startup"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    let (model, provider_id, model_id) = app
        .state
        .runs
        .resolve_model("mock-llm:test-model")
        .await
        .unwrap();
    let view = messages::load_llm_view(&app.pool, &conversation)
        .await
        .unwrap();
    let lock = Arc::new(Mutex::new(()));
    app.state
        .runs
        .prompt_env_locks
        .insert(conversation.clone(), lock.clone());
    let lock_guard = lock.lock().await;
    let guard = app.state.runs.occupy(&conversation).unwrap();
    let mut events = app
        .state
        .sessions
        .get(&conversation)
        .unwrap()
        .hub
        .subscribe();
    let service = app.state.runs.clone();
    let target = conversation.clone();
    let waiter = tokio::spawn(async move {
        service
            .start_root_run(RootRunRequest {
                guard,
                conversation_id: target,
                project,
                model,
                provider_id,
                model_id,
                view,
                context_window: None,
                idle: None,
                reasoning: None,
                attach_task_tool: false,
            })
            .await
    });
    // The database is the synchronization point: once registration is visible,
    // assembly is blocked by the held prompt lock. No arbitrary delay is used.
    let registered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(row) = runs::latest_run_for_conversation(&app.pool, &conversation)
                .await
                .unwrap()
            {
                break row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(registered.status, "running");
    assert!(llm.requests().is_empty());
    waiter.abort();
    assert!(waiter.await.is_err());
    assert!(app.state.sessions.contains_key(&conversation));

    if cancel {
        app.state
            .sessions
            .get(&conversation)
            .unwrap()
            .cancel
            .cancel();
        // Cancellation must settle even while assembly cannot acquire the lock.
        let mut observed = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.next().await {
                observed.push(event);
            }
        })
        .await
        .expect("cancelled startup must settle without unlocking assembly");
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, AgentEvent::Cancelled))
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, AgentEvent::RunMeta { .. }))
        );
        assert!(llm.requests().is_empty());
        drop(lock_guard);
    } else {
        drop(lock_guard);
        let observed = tokio::time::timeout(Duration::from_secs(10), events.collect::<Vec<_>>())
            .await
            .expect("detached startup must run through durable settlement and EOF");
        assert!(matches!(observed.first(), Some(AgentEvent::RunMeta { .. })));
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, AgentEvent::Finish))
        );
        assert_eq!(llm.requests().len(), 1);
    }
    let settled = runs::get_run(&app.pool, &registered.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        settled.status,
        if cancel { "cancelled" } else { "completed" }
    );
    assert!(settled.finished_at.is_some());
    assert!(!app.state.sessions.contains_key(&conversation));
}

#[tokio::test]
async fn dropped_root_startup_waiter_does_not_abandon_registered_execution() {
    dropped_root_startup_waiter(false).await;
}

#[tokio::test]
async fn root_startup_cancels_after_waiter_drop_without_releasing_assembly_lock() {
    dropped_root_startup_waiter(true).await;
}

#[tokio::test]
async fn dropped_send_waiter_during_mcp_preparation_retains_cancellable_ownership() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
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
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let response = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name":"owned-send-handshake", "transport":"http", "url":url, "enabled":true,
            }),
        )
        .await;
    assert_eq!(response.status(), 201);

    let application = ConversationApplication::new(app.state.runs.clone());
    let target = conversation.clone();
    let waiter = tokio::spawn(async move {
        application
            .send(SendCommand {
                conversation_id: target,
                content: "committed preparation".into(),
                provider_spec: "mock-llm:test-model".into(),
                expected_revision: None,
                client_request_id: Some("dropped-waiter".into()),
                reasoning: None,
                attachment_ids: vec![],
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), requested.notified())
        .await
        .expect("send reached MCP preparation after committing its message");
    assert_eq!(
        messages::count_user_messages(&app.pool, &conversation)
            .await
            .unwrap(),
        1
    );
    assert!(
        runs::latest_run_for_conversation(&app.pool, &conversation)
            .await
            .unwrap()
            .is_none()
    );
    waiter.abort();
    assert!(waiter.await.is_err());
    let mut events = {
        let handle = app
            .state
            .sessions
            .get(&conversation)
            .expect("a committed send retains its owner when its waiter is dropped");
        handle.hub.subscribe()
    };
    app.cancel(&conversation).await;
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut observed = Vec::new();
        while let Some(event) = events.next().await {
            observed.push(event);
        }
        observed
    })
    .await
    .expect("preparing send must remain cancellable and reach EOF");
    assert!(
        observed
            .iter()
            .any(|event| matches!(event, AgentEvent::Cancelled))
    );
    assert!(!app.state.sessions.contains_key(&conversation));
    assert!(llm.requests().is_empty());
    assert!(
        oct_agent::application::conversation::preparation::can_resume(&app.pool, &conversation)
            .await
            .unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn stale_child_completion_cannot_publish_or_remove_the_current_execution() {
    let app = support::TestApp::new().await;
    let project = app.create_project().await;
    let conversation = app.create_conversation(&project).await;
    let row = runs::create_run(
        &app.pool,
        runs::NewRun {
            conversation_id: conversation.clone(),
            kind: "subagent",
            parent_run_id: None,
            root_run_id: None,
            parent_tool_call_id: None,
            provider_id: None,
            model_id: None,
            context_window: None,
            description: Some("epoch guard".into()),
        },
    )
    .await
    .unwrap();
    let epoch = runs::bump_execution_epoch(&app.pool, &row.id)
        .await
        .unwrap()
        .unwrap();
    let (done, current) = watch::channel(false);
    let (old_done, old) = watch::channel(false);
    let child_hub = Arc::new(EventHub::new());
    let parent_hub = Arc::new(EventHub::new());
    app.state.runs.subagents.register_child(ChildRun {
        run_id: row.id.clone(),
        agent_id: conversation.clone(),
        description: "epoch guard".into(),
        cancel: CancellationToken::new(),
        done: current.clone(),
        hub: Arc::downgrade(&child_hub),
        epoch,
    });
    let outcome = RunOutcome {
        status: RunStatus::Completed,
        final_message_id: None,
        final_text: None,
        partial: false,
        usage: None,
        error: None,
    };
    app.state
        .runs
        .subagents
        .settle(
            &app.pool,
            &row.id,
            &conversation,
            "epoch guard",
            epoch - 1,
            "call",
            &outcome,
            &parent_hub,
            &child_hub,
            &old_done,
        )
        .await;
    assert!(!*old.borrow());
    assert!(!*current.borrow());
    assert!(parent_hub.snapshot().is_empty());
    assert!(app.state.runs.subagents.is_active(&conversation));
    assert_eq!(
        runs::get_run(&app.pool, &row.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );

    app.state
        .runs
        .subagents
        .settle(
            &app.pool,
            &row.id,
            &conversation,
            "epoch guard",
            epoch,
            "call",
            &outcome,
            &parent_hub,
            &child_hub,
            &done,
        )
        .await;
    assert!(*current.borrow());
    assert!(!app.state.runs.subagents.is_active(&conversation));
    let settled = runs::get_run(&app.pool, &row.id).await.unwrap().unwrap();
    assert_eq!(settled.status, "completed");
    let observed = parent_hub.snapshot();
    assert_eq!(observed.len(), 1);
    assert!(
        matches!(&observed[0], AgentEvent::SubagentUpdated { execution_epoch, .. } if *execution_epoch == epoch as u64)
    );
}
