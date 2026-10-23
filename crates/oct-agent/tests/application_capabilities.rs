//! Scoped tool ports and composition-root notification ownership.
mod support;

use std::sync::Arc;
use std::time::Duration;

use oct_agent::application::notifications::WakeDrainer;
use oct_agent::application::services::ApplicationServices;
use oct_agent::db::background::{self, NewBackgroundTask, TaskFinalStatus};

fn service(
    env: &support::TestEnv,
) -> (
    Arc<ApplicationServices>,
    oct_agent::application::services::ServiceResources,
) {
    let resources = oct_agent::application::services::ServiceResources {
        lifecycle: Default::default(),
        pool: env.pool.clone(),
        registry: Arc::new(oct_llm_provider::providers::default_registry()),
        mcp: Arc::new(oct_agent::mcp::McpManager::default()),
        sessions: Arc::new(dashmap::DashMap::new()),
        file_locks: Arc::new(dashmap::DashMap::new()),
        subagents: Arc::new(oct_agent::subagent::SubagentManager::default()),
        background: Arc::new(oct_agent::runtime::background::BackgroundTaskManager::new(
            env.pool.clone(),
            env.dir.path().join("exec"),
        )),
        attachments: Arc::new(oct_agent::attachments::AttachmentService::new(
            env.dir.path().join("attachments"),
            env.pool.clone(),
        )),
        approvals: Arc::new(oct_agent::permissions::registry::ApprovalRegistry::new()),
        gates: Arc::new(dashmap::DashMap::new()),
        prompt_envs: Arc::new(dashmap::DashMap::new()),
        prompt_env_locks: Arc::new(dashmap::DashMap::new()),
    };
    (
        Arc::new(ApplicationServices::new(resources.clone())),
        resources,
    )
}

#[tokio::test]
async fn startup_reconciles_without_a_signal_and_shutdown_releases_busy_waiters() {
    let env = support::TestEnv::new().await;
    let conv = support::new_conversation(&env.pool, env.dir.path()).await;
    let (runs, resources) = service(&env);
    let guard = runs.occupancy().occupy(&conv).unwrap();
    let task = background::create_task(
        &env.pool,
        NewBackgroundTask {
            id: "durable-task".into(),
            conversation_id: conv.clone(),
            parent_run_id: None,
            parent_call_id: "call".into(),
            command: "echo completed".into(),
            working_dir: None,
            output_path: env.dir.path().join("output.log").display().to_string(),
        },
    )
    .await
    .unwrap();
    background::finish_task(
        &env.pool,
        &task.id,
        TaskFinalStatus::Completed,
        Some(0),
        None,
        0,
    )
    .await
    .unwrap();

    // No manager settlement/kick occurs. Startup must discover the durable
    // row; the occupied conversation keeps the delivery worker in its wait.
    let drainer = WakeDrainer::start(runs.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&runs) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup reconciliation must own a delivery waiter");
    let weak = Arc::downgrade(&runs);
    let background = resources.background.clone();
    let subagents = resources.subagents.clone();
    drop(runs);
    drainer.shutdown().await;
    assert!(
        weak.upgrade().is_none(),
        "managers and delivery waiters must not retain the application"
    );
    assert!(!subagents.is_active(&conv));
    assert!(background.active_task(&task.id).is_none());
    let pending = background::list_pending_notifications(&env.pool, &conv)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "shutdown leaves durable work recoverable");
    assert!(
        background::pending_wake(&env.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
    drop(guard);
}
