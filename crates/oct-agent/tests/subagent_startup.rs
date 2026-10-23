//! Cancellation at the durable registration/launch boundary, using SQLite
//! hooks and pool reservations to stop real startup without timing sleeps.
mod support;

use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures_util::StreamExt;
use oct_agent::application::delegation::TaskSpawnConfig;
use oct_agent::db::{messages, projects, runs};
use oct_agent::events::AgentEvent;
use oct_agent::events::EventHub;
use oct_agent::tool_contract::AgentTool;
use oct_agent::tools::shared::{AgentReadState, WorkspaceFileLocks};
use oct_agent::tools::workspace::WorkspaceRoots;
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall};
use sqlx::Sqlite;
use sqlx::pool::PoolConnection;
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};
use tokio_util::sync::CancellationToken;

async fn spawn_config(app: &support::TestApp) -> (TaskSpawnConfig, Arc<Vec<Arc<dyn AgentTool>>>) {
    let project_id = app.create_project().await;
    let conv = app.create_conversation(&project_id).await;
    let project = projects::get_project(&app.pool, &project_id)
        .await
        .unwrap()
        .unwrap();
    messages::insert_message(
        &app.pool,
        &conv,
        &Message::text(Role::User, "delegate"),
        Some("mock-llm"),
        Some("test-model"),
        None,
    )
    .await
    .unwrap();
    messages::insert_message(
        &app.pool,
        &conv,
        &Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: "startup-task".into(),
                name: "task".into(),
                arguments: r#"{"description":"startup","prompt":"delegation"}"#.into(),
            })],
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let parent = runs::create_run(
        &app.pool,
        runs::NewRun {
            conversation_id: conv.clone(),
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
    let roots = WorkspaceRoots::new(app.dir.path().to_owned(), vec![]);
    let locks = Arc::new(WorkspaceFileLocks::new());
    let tools: Arc<Vec<Arc<dyn AgentTool>>> = Arc::new(
        oct_agent::tools::default_tools_with(
            roots.clone(),
            locks.clone(),
            Arc::new(AgentReadState::new()),
        )
        .into_iter()
        .map(Arc::from)
        .collect(),
    );
    let env = app
        .state
        .conversations
        .reload_instructions(&conv)
        .await
        .unwrap();
    (
        TaskSpawnConfig {
            parent_conversation_id: conv.clone(),
            parent_run_id: parent.id.clone(),
            root_conversation_id: conv,
            root_run_id: parent.id,
            project,
            roots,
            locks,
            provider_id: "mock-llm".into(),
            model_id: "test-model".into(),
            context_window: None,
            tool_mode: oct_agent::tool_contract::McpToolMode::Direct,
            parent_cancel: CancellationToken::new(),
            parent_hub: Arc::new(EventHub::new()),
            inheritance: Arc::new(
                oct_agent::application::inheritance::WorkerInheritance::capture(&tools),
            ),
            parent_env: Arc::new(env),
        },
        tools,
    )
}

/// Reserve four connections and make the fifth announce the registration
/// INSERT. The hook waits on a test-owned gate so we can enqueue an acquire
/// before COMMIT releases that last connection back into the pool.
async fn registration_gate(
    app: &support::TestApp,
) -> (
    Vec<PoolConnection<Sqlite>>,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let mut held = Vec::new();
    for _ in 0..app.pool.options().get_max_connections() {
        held.push(app.pool.acquire().await.unwrap());
    }
    let mut registration = held.pop().unwrap();
    let (inserted_tx, inserted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut inserted_tx = Some(inserted_tx);
    registration
        .lock_handle()
        .await
        .unwrap()
        .set_update_hook(move |event| {
            if event.table == "agent_runs"
                && matches!(event.operation, sqlx::sqlite::SqliteOperation::Insert)
                && let Some(inserted_tx) = inserted_tx.take()
            {
                let _ = inserted_tx.send(());
                // SQLite runs this on its dedicated worker thread. Dropping the
                // test's sender also releases it if an assertion unwinds.
                let _ = release_rx.recv();
            }
        });
    drop(registration);
    (held, inserted_rx, release_tx)
}

fn start_task(
    app: &support::TestApp,
    spawn: &TaskSpawnConfig,
) -> tokio::task::JoinHandle<Result<runs::AgentRun, oct_agent::tool_contract::ports::TaskError>> {
    let manager = app.state.delegation.clone();
    let spawn = spawn.clone();
    tokio::spawn(async move {
        manager
            .run_task(&spawn, "startup-task", "startup", "delegation")
            .await
    })
}

async fn assert_settled(
    app: &support::TestApp,
    spawn: &TaskSpawnConfig,
    events: &mut oct_agent::events::SubscribeStream,
    status: &str,
) -> runs::AgentRun {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.next().await {
            if let AgentEvent::SubagentUpdated {
                status: observed, ..
            } = event
                && serde_json::to_value(observed).unwrap() == status
            {
                return;
            }
        }
        panic!("parent hub ended before child settlement");
    })
    .await
    .expect("child startup must settle after its caller is dropped");
    let run = runs::find_run_by_parent_call(&app.pool, &spawn.parent_run_id, "startup-task")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, status);
    assert!(run.finished_at.is_some());
    assert!(!app.state.sessions.contains_key(&run.conversation_id));
    assert!(!app.state.subagents.is_active(&run.conversation_id));
    run
}

#[tokio::test]
async fn dropping_tool_future_after_registration_still_settles_cancelled_child() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    let (spawn, tools) = spawn_config(&app).await;
    let parent_guard = app
        .services
        .occupancy()
        .occupy_with(&spawn.parent_conversation_id, spawn.parent_cancel.clone())
        .unwrap();
    let mut events = spawn.parent_hub.subscribe();
    let (mut held, inserted, release) = registration_gate(&app).await;
    let task = start_task(&app, &spawn);
    tokio::time::timeout(Duration::from_secs(10), inserted)
        .await
        .unwrap()
        .unwrap();

    // Queue ahead of launch's first model/view query. Once acquired, this
    // connection proves COMMIT finished and keeps all preparation blocked.
    let mut next_connection = Box::pin(app.pool.acquire());
    assert!(
        poll_fn(|cx| Poll::Ready(next_connection.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    release.send(()).unwrap();
    let mut committed = tokio::time::timeout(Duration::from_secs(10), next_connection)
        .await
        .unwrap()
        .unwrap();
    let child: (String, String) =
        sqlx::query_as("SELECT id, conversation_id FROM agent_runs WHERE parent_run_id = ?")
            .bind(&spawn.parent_run_id)
            .fetch_one(&mut *committed)
            .await
            .unwrap();
    assert!(
        !app.state.subagents.is_active(&child.1),
        "supervision has not taken over"
    );

    spawn.parent_cancel.cancel();
    task.abort(); // Same drop performed by the parent's tool JoinSet shutdown.
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    drop(tools);
    held.push(committed);
    // Reproduce the parent closing the call, removing recovery's dangling
    // signal even though the child's startup has not yet settled.
    drop(held);
    messages::answer_tool_call_if_absent(
        &app.pool,
        &spawn.parent_conversation_id,
        "startup-task",
        "用户取消了执行",
        true,
        None,
    )
    .await
    .unwrap();
    runs::finish_run(
        &app.pool,
        &spawn.parent_run_id,
        runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    drop(parent_guard);
    let run = assert_settled(&app, &spawn, &mut events, "cancelled").await;
    assert_eq!(run.id, child.0);
    assert_eq!(run.execution_epoch, 1);
    assert!(
        messages::find_dangling_tool_calls(&app.pool, &spawn.parent_conversation_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        llm.requests().is_empty(),
        "cancelled startup must not call a model"
    );
}

#[tokio::test]
async fn dropping_tool_future_during_registration_does_not_abandon_commit() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    let (spawn, tools) = spawn_config(&app).await;
    let _parent_guard = app
        .services
        .occupancy()
        .occupy_with(&spawn.parent_conversation_id, spawn.parent_cancel.clone())
        .unwrap();
    let mut events = spawn.parent_hub.subscribe();
    let (held, inserted, release) = registration_gate(&app).await;
    let task = start_task(&app, &spawn);
    tokio::time::timeout(Duration::from_secs(10), inserted)
        .await
        .unwrap()
        .unwrap();
    spawn.parent_cancel.cancel();
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    drop(tools);
    release.send(()).unwrap();
    drop(held);
    assert_settled(&app, &spawn, &mut events, "cancelled").await;
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn dropped_waiter_and_duplicate_delegation_share_one_execution() {
    let app = support::TestApp::new().await;
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("done"),
        finish_chunk("stop"),
    ])])
    .await;
    app.register_llm_provider(&llm).await;
    let (spawn, tools) = spawn_config(&app).await;
    let _parent_guard = app
        .services
        .occupancy()
        .occupy_with(&spawn.parent_conversation_id, spawn.parent_cancel.clone())
        .unwrap();
    let (held, inserted, release) = registration_gate(&app).await;
    let first = start_task(&app, &spawn);
    tokio::time::timeout(Duration::from_secs(10), inserted)
        .await
        .unwrap()
        .unwrap();
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    let delegation = app.state.delegation.clone();
    let mut duplicate =
        Box::pin(delegation.run_task(&spawn, "startup-task", "startup", "delegation"));
    assert!(
        poll_fn(|cx| Poll::Ready(duplicate.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(tools); // The independent owners retain worker inheritance.
    release.send(()).unwrap();
    drop(held);
    let run = tokio::time::timeout(Duration::from_secs(10), duplicate)
        .await
        .unwrap()
        .unwrap_or_else(|error| panic!("delegation failed: {error}"));
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.execution_epoch, 1);
    let rows = runs::list_subagent_runs(&app.pool, &spawn.root_conversation_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, run.id);
    assert_eq!(
        llm.requests().len(),
        1,
        "reattachment cannot restart a child"
    );
}
