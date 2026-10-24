//! Owned root startup tasks bridge preparation/assembly and the execution kernel.
//! A caller owns a receiver, never the registered run's execution future.

use futures_util::FutureExt;

use super::services::ApplicationServices;
use crate::agent::RunOutcome;
use crate::agent::RunStatus;
use crate::db::runs as run_db;
use crate::error::{AppError, AppResult};
use crate::events::AgentEvent;
use crate::permissions::gate::publish_cancelled_approvals;
use crate::runtime::execution::{ExecutionIdentity, RunExecutor};
use crate::runtime::replay::ReplayBase;

use super::assembly::{RootAssembler, RootAssemblyInput};

/// Everything [`launch_root`] needs to launch one root run.
/// The application resolves the model/window/idle and performs its
/// ordering-sensitive writes (repair, user-message insert) BEFORE handing
/// the guard over — from here on the run is supervised end to end.
pub struct RootRunRequest {
    /// Consumed: moved into the supervisor so occupancy lasts the whole run
    /// (and auto-releases on any failure before supervision takes over).
    pub guard: crate::runtime::OccupancyGuard,
    pub conversation_id: String,
    pub project: crate::db::projects::Project,
    pub model: Box<dyn oct_llm_provider::model::ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub view: crate::context::LlmView,
    pub context_window: Option<u32>,
    /// How long the conversation sat idle BEFORE this run started — the
    /// microcompact idle trigger.
    pub idle: Option<std::time::Duration>,
    /// 用户为本次 run 选择的思考档位（send 请求携带；resume 从 user 消息
    /// details 恢复；wake 等内部启动为 `None`）。
    pub reasoning: Option<oct_llm_provider::core::ReasoningRequest>,
    /// Whether to attach the `task` delegation tool: root runs of kind
    /// `chat` conversations get it; resumed roots included. Workers never
    /// do (depth <= 1 is enforced server-side on top).
    pub attach_task_tool: bool,
}

/// A successfully launched run's event subscription and persisted identity.
/// The subscription receives events without owning the executing task.
pub struct StartedRun {
    pub events: crate::events::SubscribeStream,
    pub run_id: String,
}

/// Create the run's persistence row, subscribe to its event hub,
/// assemble the agent context, and hand the whole run to the supervisor.
/// The SSE response stream is live from the very first event. On any
/// assembly failure the row is settled as failed before the consumed
/// guard drops, releasing the conversation.
pub async fn launch_root(runs: &ApplicationServices, req: RootRunRequest) -> AppResult<StartedRun> {
    // Read the session entry the guard guarantees and subscribe to its
    // hub BEFORE spawning so the response stream is live from the very
    // first event (the hub log would replay them anyway — subscribing
    // first is just the cheaper path).
    let handle = runs
        .sessions
        .get(&req.conversation_id)
        .map(|entry| entry.value().clone())
        .expect("the occupancy guard guarantees the session entry");
    let stream = handle.hub.subscribe();

    let run_id = register_and_launch_root(runs, req).await?;

    Ok(StartedRun {
        events: stream,
        run_id,
    })
}

async fn register_and_launch_root(
    runs: &ApplicationServices,
    request: RootRunRequest,
) -> AppResult<String> {
    let assembler = RootAssembler {
        shared: runs.assembly_services(),
        background: runs.background.clone(),
        delegation: runs.delegation_service(),
    };
    let executor = RunExecutor::new(runs.pool.clone(), runs.approvals.clone());
    let lifecycle = runs.lifecycle.clone();
    // Own registration too: a COMMIT in flight must not lose its owner when
    // the HTTP waiter disappears. Drop of this JoinHandle detaches the task.
    tokio::spawn(async move {
        let RootRunRequest {
            guard,
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
            reasoning,
            attach_task_tool,
        } = request;
        let shared = &assembler.shared;
        let max_output_tokens = if model.info().provider_name == "anthropic" {
            crate::db::models::get_model(&shared.pool, &provider_id, &model_id)
                .await?
                .and_then(|row| row.limit_output)
                .and_then(|limit| u32::try_from(limit).ok())
                .filter(|limit| *limit > 0)
        } else {
            None
        };

        // Preparation has finished; capture the durable base before assembly
        // can publish content. Membership revisions survive COW/reordering.
        let replay_base = ReplayBase::load(&shared.pool, &conversation_id).await?;
        lifecycle.check("project", &project.id)?;
        let run = run_db::create_run(
            &shared.pool,
            run_db::NewRun {
                conversation_id: conversation_id.clone(),
                kind: "root",
                parent_run_id: None,
                root_run_id: None,
                parent_tool_call_id: None,
                provider_id: Some(provider_id.clone()),
                model_id: Some(model_id.clone()),
                context_window: context_window.map(i64::from),
                description: None,
            },
        )
        .await?;
        let handle = shared
            .sessions
            .get(&conversation_id)
            .expect("the occupancy guard guarantees the session entry")
            .value()
            .clone();
        let identity = ExecutionIdentity {
            run_id: run.id.clone(),
            conversation_id: conversation_id.clone(),
            epoch: None,
        };
        let assembly = assembler.assemble(RootAssemblyInput {
            run_id: run.id.clone(),
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
            reasoning,
            attach_task_tool,
            handle: handle.clone(),
            max_output_tokens,
        });
        let result = tokio::select! {
            biased;
            _ = handle.cancel.cancelled() => Err(startup_error("root startup cancelled")),
            result = std::panic::AssertUnwindSafe(assembly).catch_unwind() => {
                result.unwrap_or_else(|_| Err(startup_error("root startup panicked")))
            }
        };
        let assembled = match result {
            Ok(assembled) => assembled,
            Err(error) => {
                let outcome = startup_outcome(handle.cancel.is_cancelled(), &error);
                if outcome.status == RunStatus::Cancelled {
                    handle.hub.publish(AgentEvent::Cancelled);
                } else {
                    handle.hub.publish(AgentEvent::Error(error.to_string()));
                }
                executor
                    .finish(&identity, &outcome, &handle.hub, None)
                    .await?;
                publish_cancelled_approvals(&shared.approvals, &run.id, &handle.hub);
                drop(guard);
                // A cancelled preparation is still a valid cancelled event
                // subscription; ordinary assembly errors retain HTTP errors.
                return if outcome.status == RunStatus::Cancelled {
                    Ok(run.id)
                } else {
                    Err(error)
                };
            }
        };
        handle
            .hub
            .publish(replay_base.event(&run.id, run.execution_epoch));
        tokio::spawn(executor.supervise(identity, assembled, guard, handle, None));
        Ok(run.id)
    })
    .await
    .map_err(|error| startup_error(&format!("root launcher panicked: {error}")))?
}

fn startup_error(message: &str) -> AppError {
    AppError::Internal(anyhow::anyhow!(message.to_owned()))
}

fn startup_outcome(cancelled: bool, error: &AppError) -> RunOutcome {
    RunOutcome {
        status: if cancelled {
            RunStatus::Cancelled
        } else {
            RunStatus::Failed
        },
        final_message_id: None,
        final_text: None,
        partial: false,
        usage: None,
        error: Some(error.to_string()),
    }
}
