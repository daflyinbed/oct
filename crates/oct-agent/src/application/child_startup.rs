//! Child startup owns read-only recovery preparation, execution claims and the
//! handoff to supervision. Callers receive the settled row, never an execution
//! future whose drop could abandon a registered or claimed run.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use futures_util::FutureExt;
use oct_llm_provider::{core::ContentPart, model::ChatModel};
use tokio::sync::{OwnedMutexGuard, watch};
use tokio_util::sync::CancellationToken;

use super::assembly::{AssemblyServices, WorkerAssembler, WorkerAssemblyInput, workspace_roots};
use super::inheritance::WorkerInheritance;
use crate::agent::{RunOutcome, RunStatus, repo_instructions::FrozenPromptEnv};
use crate::agent_settings::WorkerSnapshot;
use crate::db::{messages as msg_db, projects::Project, runs as run_db};
use crate::events::{AgentEvent, EventHub, SubagentRunStatus};
use crate::permissions::gate::publish_cancelled_approvals;
use crate::runtime::{OccupancyGuard, RunOccupancy};
use crate::runtime::{execution::ExecutionIdentity, execution::RunExecutor, replay::ReplayBase};
use crate::subagent::manager::is_terminal_status;
use crate::subagent::registry::ChildCompletion;
use crate::subagent::{ChildRun, SubagentManager};

/// The delegating run's OWN frozen tools and prompt environment. Recovery has
/// neither: crashed instances are rebuilt from the worker's sealed snapshot
/// and the root conversation's prompt cache instead.
pub(crate) struct LiveParent {
    pub inheritance: Arc<WorkerInheritance>,
    pub prompt_env: Arc<FrozenPromptEnv>,
}

/// Parent facts that cannot be recovered from a child's persisted run alone.
/// The child derives its own token; cancelling it never cancels its parent.
pub(crate) struct ChildParent {
    pub project: Project,
    pub provider_id: String,
    pub model_id: String,
    pub cancel: CancellationToken,
    pub hub: Arc<EventHub>,
    pub live: Option<LiveParent>,
}

#[derive(Clone)]
pub(crate) struct ChildStartup {
    shared: AssemblyServices,
    occupancy: RunOccupancy,
    subagents: Arc<SubagentManager>,
    executor: RunExecutor,
}

impl ChildStartup {
    pub(crate) fn new(
        shared: AssemblyServices,
        occupancy: RunOccupancy,
        subagents: Arc<SubagentManager>,
    ) -> Self {
        let executor = RunExecutor::new(shared.pool.clone(), shared.approvals.clone());
        Self {
            shared,
            occupancy,
            subagents,
            executor,
        }
    }

    /// Transfer a committed registration and its election directly to an owned
    /// startup task. Occupancy precedes asynchronous preparation, making this
    /// interval cancellable even before a live child handle has been published.
    pub(crate) async fn launch_registered(
        &self,
        run: &run_db::AgentRun,
        start_message_id: String,
        parent: ChildParent,
        registration: OwnedMutexGuard<()>,
    ) -> Result<run_db::AgentRun> {
        let startup = self.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let guard = match startup
                .occupancy
                .occupy_with(&run.conversation_id, parent.cancel.child_token())
            {
                Ok(guard) => guard,
                Err(error) => {
                    // A busy child belongs to another launcher; a lifecycle
                    // rejection without an owner must settle our registration.
                    if !startup.shared.sessions.contains_key(&run.conversation_id) {
                        run_db::finish_run(
                            &startup.shared.pool,
                            &run.id,
                            run_db::RunFinalStatus::Failed,
                            None,
                            Some(&error.to_string()),
                            None,
                            None,
                            Some(run.execution_epoch),
                        )
                        .await?;
                    }
                    return Err(error.into());
                }
            };
            startup
                .run_claimed(
                    run,
                    parent,
                    guard,
                    ChildPreparation::Registered(start_message_id),
                    registration,
                )
                .await
        })
        .await
        .context("registered child startup panicked")?
    }

    /// Reuse terminal results, attach to an active child, or continue the SAME
    /// run with a new epoch. Preparation is read-only until occupancy and the
    /// execution claim succeed; missing models/snapshots leave lost rows
    /// recoverable, and a concurrent lost-run cancellation may still win.
    pub(crate) async fn recover(
        &self,
        run: &run_db::AgentRun,
        parent: ChildParent,
    ) -> Result<run_db::AgentRun> {
        let startup = self.clone();
        let run_id = run.id.clone();
        tokio::spawn(async move { startup.recover_owned(&run_id, parent).await })
            .await
            .context("child recovery startup panicked")?
    }

    async fn recover_owned(&self, run_id: &str, parent: ChildParent) -> Result<run_db::AgentRun> {
        let run = self.read_run(run_id).await?;
        if is_terminal_status(&run.status) {
            // 子已完成父未接收: the settled row IS the answer.
            return Ok(run);
        }
        if let Some(child) = self.subagents.child_by_agent(&run.conversation_id) {
            return self.wait_settled(&run.id, child.done.clone()).await;
        }

        // Do not hold the registration election during read-only preparation:
        // cancelling an unowned lost/running row must remain possible here.
        let dangling =
            msg_db::find_dangling_tool_calls(&self.shared.pool, &run.conversation_id).await?;
        let messages = msg_db::list_messages(&self.shared.pool, &run.conversation_id).await?;
        // A crash after the final answer persisted needs no model or tool
        // configuration, even when its provider was removed in the meantime.
        let preparation = if let Some(last) = messages.last()
            && dangling.is_empty()
            && last.role == "assistant"
            && !parts_carry_tool_call(&last.parts_json)
        {
            ChildPreparation::Completed(last.id.clone())
        } else {
            let worker = self.resolve_worker(&run, &parent, None).await?;
            // Validate before claiming; repair reloads under the same occupancy.
            msg_db::load_llm_view(&self.shared.pool, &run.conversation_id)
                .await
                .context("validating the child view")?;
            ChildPreparation::Resume(Box::new(worker))
        };

        let registration = self.subagents.registration.clone().lock_owned().await;
        let mut run = self.read_run(run_id).await?;
        // A cancellation or another startup may have won during preparation.
        if is_terminal_status(&run.status) {
            return Ok(run);
        }
        if let Some(child) = self.subagents.child_by_agent(&run.conversation_id) {
            drop(registration);
            return self.wait_settled(&run.id, child.done.clone()).await;
        }
        let guard = self
            .occupancy
            .occupy_with(&run.conversation_id, parent.cancel.child_token())?;
        // Own the claim COMMIT too: cancellation must not drop a write in flight
        // and leave a running epoch without its startup task.
        let Some(epoch) = run_db::bump_execution_epoch(&self.shared.pool, &run.id).await? else {
            return self.read_run(&run.id).await;
        };
        run.execution_epoch = epoch;
        self.run_claimed(run, parent, guard, preparation, registration)
            .await
    }

    /// From registration/claim through supervision, this task is the execution
    /// owner. All fallible preparation shares one cancellation/panic/settlement
    /// path, including repair/view failures before worker assembly begins.
    async fn run_claimed(
        &self,
        run: run_db::AgentRun,
        parent: ChildParent,
        guard: OccupancyGuard,
        preparation: ChildPreparation,
        registration: OwnedMutexGuard<()>,
    ) -> Result<run_db::AgentRun> {
        let handle = self
            .shared
            .sessions
            .get(&run.conversation_id)
            .expect("the occupancy guard guarantees the session entry")
            .value()
            .clone();
        let identity = ExecutionIdentity {
            run_id: run.id.clone(),
            conversation_id: run.conversation_id.clone(),
            epoch: Some(run.execution_epoch),
        };
        // The same done watch, guard and token transfer to supervision or
        // startup settlement. The child index only holds weak hub references.
        let (done_tx, done_rx) = watch::channel(false);
        let completion = ChildCompletion {
            registry: self.subagents.registry.clone(),
            agent_id: run.conversation_id.clone(),
            parent_call_id: run.parent_tool_call_id.clone().unwrap_or_default(),
            description: run.description.clone().unwrap_or_default(),
            parent_hub: parent.hub.clone(),
            done: done_tx,
        };
        if let ChildPreparation::Completed(final_id) = preparation {
            let outcome = RunOutcome {
                status: RunStatus::Completed,
                final_message_id: Some(final_id),
                final_text: None,
                partial: false,
                usage: None,
                error: None,
            };
            self.executor
                .finish(&identity, &outcome, &handle.hub, Some(&completion))
                .await?;
            drop(guard);
            drop(registration);
            return self.read_run(&run.id).await;
        }

        let assemble = async {
            let worker = match preparation {
                ChildPreparation::Registered(start) => {
                    self.resolve_worker(&run, &parent, Some(start)).await?
                }
                ChildPreparation::Resume(worker) => *worker,
                ChildPreparation::Completed(_) => unreachable!("settled above"),
            };
            if run.execution_epoch > 1 {
                self.shared
                    .repair_dangling_tools(&run.conversation_id, &[])
                    .await
                    .context("repairing the child conversation's dangling tool calls")?;
            }
            let view = msg_db::load_llm_view(&self.shared.pool, &run.conversation_id)
                .await
                .context("loading the child view")?;

            // Publish live ownership before assembly can await. Earlier model
            // preparation remains cancellable through the occupancy entry.
            self.subagents.register_child(ChildRun {
                run_id: run.id.clone(),
                agent_id: run.conversation_id.clone(),
                description: completion.description.clone(),
                cancel: handle.cancel.clone(),
                done: done_rx.clone(),
                hub: Arc::downgrade(&handle.hub),
                epoch: run.execution_epoch,
            });
            parent.hub.publish(AgentEvent::SubagentUpdated {
                run_id: run.id.clone(),
                agent_id: run.conversation_id.clone(),
                parent_call_id: completion.parent_call_id.clone(),
                status: SubagentRunStatus::Running,
                execution_epoch: u64::try_from(run.execution_epoch).unwrap_or(u64::MAX),
                description: completion.description.clone(),
                activity: None,
            });
            let mut replay = ReplayBase::load(&self.shared.pool, &run.conversation_id).await?;
            replay.trigger_message_id = Some(worker.start_message_id);
            let (roots, _additional) = workspace_roots(&parent.project);
            let assembled = WorkerAssembler {
                shared: self.shared.clone(),
            }
            .assemble(WorkerAssemblyInput {
                run_id: run.id.clone(),
                epoch: run.execution_epoch,
                hub: handle.hub.clone(),
                conversation_id: run.conversation_id.clone(),
                project: parent.project.clone(),
                model: worker.model,
                provider_id: worker.provider_id,
                model_id: worker.model_id,
                view,
                context_window: worker.context_window,
                agent_config: worker.agent_config,
                roots,
                locks: self.shared.file_locks_for(&parent.project.id),
                inheritance: parent.live.as_ref().map(|live| live.inheritance.clone()),
                parent_env: parent.live.as_ref().map(|live| live.prompt_env.clone()),
            })
            .await?;
            Ok::<_, anyhow::Error>((assembled, replay))
        };
        let prepared = tokio::select! {
            biased;
            _ = handle.cancel.cancelled() => Err(anyhow!("subagent startup cancelled")),
            result = std::panic::AssertUnwindSafe(assemble).catch_unwind() => {
                result.unwrap_or_else(|_| Err(anyhow!("subagent startup panicked")))
            }
        };
        let (assembled, replay) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let outcome = RunOutcome {
                    status: if handle.cancel.is_cancelled() {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Failed
                    },
                    final_message_id: None,
                    final_text: None,
                    partial: false,
                    usage: None,
                    error: Some(format!("{error:#}")),
                };
                if outcome.status == RunStatus::Cancelled {
                    handle.hub.publish(AgentEvent::Cancelled);
                } else {
                    handle.hub.publish(AgentEvent::Error(format!("{error:#}")));
                }
                // Preserve worker forwarding order: close approvals before the
                // parent observes the guarded durable terminal transition.
                publish_cancelled_approvals(&self.shared.approvals, &run.id, &handle.hub);
                self.executor
                    .finish(&identity, &outcome, &handle.hub, Some(&completion))
                    .await
                    .context("settling the child startup failure")?;
                drop(guard);
                // Live delegation consumes the cancelled envelope regardless
                // of whether cancellation won during startup or in the loop.
                // Tree recovery still reports its interrupted startup instead
                // of continuing the parent after a child startup error.
                if outcome.status == RunStatus::Cancelled && parent.live.is_some() {
                    return self.read_run(&run.id).await;
                }
                return Err(error);
            }
        };
        handle
            .hub
            .publish(replay.event(&run.id, run.execution_epoch));
        tokio::spawn(self.executor.clone().supervise(
            identity,
            assembled,
            guard,
            handle,
            Some(Box::new(completion)),
        ));
        // Independent children remain parallel: release the startup election
        // before waiting. Settlement itself never depends on the waiting caller.
        drop(registration);
        self.wait_settled(&run.id, done_rx).await
    }

    async fn resolve_worker(
        &self,
        run: &run_db::AgentRun,
        parent: &ChildParent,
        start_message_id: Option<String>,
    ) -> Result<PreparedWorker> {
        let provider_id = run.provider_id.as_ref().unwrap_or(&parent.provider_id);
        let model_id = run.model_id.as_ref().unwrap_or(&parent.model_id);
        let spec = format!("{provider_id}:{model_id}");
        let (model, provider_id, model_id) = self
            .shared
            .models
            .resolve_model(&spec)
            .await
            .with_context(|| format!("resolving the child model {spec} (run {})", run.id))?;
        let agent_config = WorkerSnapshot::decode(run.agent_config_json.as_deref())?;
        let stored_window = run.context_window.and_then(|v| u32::try_from(v).ok());
        let context_window = if agent_config.is_some() || parent.live.is_none() {
            stored_window
        } else {
            // Legacy live delegations have no sealed snapshot; keep their
            // existing model-window fallback without changing crash recovery.
            self.shared
                .models
                .resolve_context_window(model.as_ref(), &provider_id, &model_id)
                .await
                .or(stored_window)
        };
        // Existing children start with their original delegation user row.
        let start_message_id = match start_message_id {
            Some(id) => id,
            None => msg_db::first_user_message_id(&self.shared.pool, &run.conversation_id)
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "child conversation {} has no delegation message",
                        run.conversation_id
                    )
                })?,
        };
        Ok(PreparedWorker {
            model,
            provider_id,
            model_id,
            context_window,
            agent_config,
            start_message_id,
        })
    }

    async fn wait_settled(
        &self,
        run_id: &str,
        mut done: watch::Receiver<bool>,
    ) -> Result<run_db::AgentRun> {
        // A watch, never polling. The persisted row is the source of truth for
        // both the task envelope and recovery's parent-history backfill.
        done.wait_for(|settled| *settled)
            .await
            .context("child execution ended before settlement")?;
        self.read_run(run_id).await
    }

    async fn read_run(&self, run_id: &str) -> Result<run_db::AgentRun> {
        run_db::get_run(&self.shared.pool, run_id)
            .await?
            .ok_or_else(|| anyhow!("child run row {run_id} vanished"))
    }
}

enum ChildPreparation {
    Registered(String),
    Resume(Box<PreparedWorker>),
    Completed(String),
}

struct PreparedWorker {
    model: Box<dyn ChatModel>,
    provider_id: String,
    model_id: String,
    context_window: Option<u32>,
    agent_config: Option<WorkerSnapshot>,
    start_message_id: String,
}

/// Whether a serialized assistant message carries any ToolCall part.
fn parts_carry_tool_call(parts_json: &str) -> bool {
    serde_json::from_str::<Vec<ContentPart>>(parts_json)
        .map(|parts| {
            parts
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall(_)))
        })
        .unwrap_or(false)
}
