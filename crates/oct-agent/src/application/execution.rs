//! Owned startup tasks bridge preparation/assembly and the execution kernel.
//! A caller owns a receiver, never the registered run's execution future.

use std::sync::Arc;

use futures_util::FutureExt;
use tokio::sync::watch;

use crate::agent::{AgentEvent, RunOutcome, RunStatus, SubagentRunStatus};
use crate::db::runs as run_db;
use crate::error::{AppError, AppResult};
use crate::permissions::gate::publish_cancelled_approvals;
use crate::runtime::execution::{ExecutionIdentity, RunExecutor};
use crate::runtime::replay::ReplayBase;
use crate::runtime::{RootRunRequest, RunService};
use crate::subagent::registry::ChildCompletion;
use crate::subagent::{ChildHandle, ChildRun, ChildRunRequest};

use super::assembly::{RootAssembler, RootAssemblyInput, WorkerAssembler, WorkerAssemblyInput};

pub(crate) async fn launch_root(runs: &RunService, request: RootRunRequest) -> AppResult<String> {
    let assembler = RootAssembler {
        shared: runs.assembly_services(),
        background: runs.background.clone(),
        subagents: runs.subagents.clone(),
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

pub(crate) async fn launch_child(
    runs: &RunService,
    request: ChildRunRequest,
) -> AppResult<ChildHandle> {
    let assembler = WorkerAssembler {
        shared: runs.assembly_services(),
    };
    let executor = RunExecutor::new(runs.pool.clone(), runs.approvals.clone());
    let registry = runs.subagents.registry.clone();
    tokio::spawn(async move {
        let ChildRunRequest {
            guard,
            run_id,
            epoch,
            conversation_id,
            start_message_id,
            view,
            context_window,
            agent_config,
            cancel,
            parent_hub,
            parent_call_id,
            description,
            roots,
            locks,
            parent_toolset,
            parent_env,
            project,
            model,
            provider_id,
            model_id,
        } = request;
        let shared = &assembler.shared;
        let handle = shared
            .sessions
            .get(&conversation_id)
            .expect("the occupancy guard guarantees the session entry")
            .value()
            .clone();
        // Publish startup ownership before assembly can await. The same done
        // watch, guard and token transfer to supervision or startup settlement.
        let (done_tx, done_rx) = watch::channel(false);
        let agent_id = conversation_id.clone();
        registry.register(ChildRun {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            description: description.clone(),
            cancel: cancel.clone(),
            done: done_rx.clone(),
            hub: Arc::downgrade(&handle.hub),
            epoch,
        });
        parent_hub.publish(AgentEvent::SubagentUpdated {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            parent_call_id: parent_call_id.clone(),
            status: SubagentRunStatus::Running,
            execution_epoch: u64::try_from(epoch).unwrap_or(u64::MAX),
            description: description.clone(),
            activity: None,
        });
        let identity = ExecutionIdentity {
            run_id: run_id.clone(),
            conversation_id: conversation_id.clone(),
            epoch: Some(epoch),
        };
        let completion = ChildCompletion {
            registry,
            agent_id: agent_id.clone(),
            parent_call_id,
            description,
            parent_hub,
            done: done_tx,
        };
        let preparation = async {
            let mut replay = ReplayBase::load(&shared.pool, &conversation_id).await?;
            replay.trigger_message_id = Some(start_message_id);
            let assembled = assembler
                .assemble(WorkerAssemblyInput {
                    run_id: run_id.clone(),
                    epoch,
                    conversation_id,
                    project,
                    model,
                    provider_id,
                    model_id,
                    view,
                    context_window,
                    agent_config,
                    roots,
                    locks,
                    parent_toolset,
                    parent_env,
                })
                .await?;
            Ok::<_, AppError>((assembled, replay))
        };
        let prepared = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(startup_error("subagent startup cancelled")),
            result = std::panic::AssertUnwindSafe(preparation).catch_unwind() => {
                result.unwrap_or_else(|_| Err(startup_error("subagent startup panicked")))
            }
        };
        let (assembled, replay) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let outcome = startup_outcome(cancel.is_cancelled(), &error);
                if outcome.status == RunStatus::Cancelled {
                    handle.hub.publish(AgentEvent::Cancelled);
                } else {
                    handle.hub.publish(AgentEvent::Error(error.to_string()));
                }
                publish_cancelled_approvals(&shared.approvals, &run_id, &handle.hub);
                executor
                    .finish(&identity, &outcome, &handle.hub, Some(&completion))
                    .await?;
                drop(guard);
                return Err(error);
            }
        };
        handle.hub.publish(replay.event(&run_id, epoch));
        let hub = handle.hub.clone();
        tokio::spawn(executor.supervise(
            identity,
            assembled,
            guard,
            handle,
            Some(Box::new(completion)),
        ));
        Ok(ChildHandle {
            run_id,
            agent_id,
            done: done_rx,
            cancel,
            hub,
        })
    })
    .await
    .map_err(|error| startup_error(&format!("child launcher panicked: {error}")))?
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
