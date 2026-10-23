//! Execution supervision, conversation occupancy and startup recovery.
//! Application launchers own registration/assembly, transfer occupancy to the
//! common supervisor, and leave it held until durable settlement and SSE EOF.
//! Children use a child cancellation token and the same guarded supervisor.

pub(crate) mod execution;
pub(crate) mod replay;
mod resolve;
pub use resolve::ModelResolver;
pub mod background;
pub mod lifecycle;
pub(crate) mod worker_config;

use crate::agent::RunHandle;
use crate::db::runs as run_db;
use crate::error::AppError;
use crate::events::EventHub;
use dashmap::DashMap;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Owns a conversation's occupancy for as long as it lives: dropping it
/// removes the session entry (and with it the run's event hub — the last
/// `Arc` goes away and every SSE stream attached to it reaches EOF).
/// Returned by [`RunOccupancy::occupy`]; moving it into the application
/// launcher keeps the conversation locked for the
/// whole run while every early error path releases it simply by returning.
pub struct OccupancyGuard {
    sessions: Arc<DashMap<String, RunHandle>>,
    conv_id: String,
}

impl Drop for OccupancyGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.conv_id);
    }
}

/// Server-startup recovery: `agent_runs` rows a previous
/// process left `running` are marked `lost` — the executing instance is
/// gone, while the persisted history and the conversation's resume
/// eligibility stay untouched. The same sweep covers `background_tasks`
/// rows: PDEATHSIG already terminated the commands when the
/// process died; their notifications are delivered by the send/resume
/// catch-up. Call once per process, before traffic.
pub async fn bootstrap(pool: &SqlitePool) -> anyhow::Result<()> {
    match run_db::mark_stale_runs_lost(pool).await {
        Ok(stale) => {
            for run in &stale {
                tracing::warn!(
                    run_id = %run.id,
                    conversation_id = %run.conversation_id,
                    "marking agent run left running by a previous process as lost"
                );
            }
        }
        Err(e) => return Err(e),
    }
    match background::task_sweep(pool).await {
        Ok(stale) => {
            for task in &stale {
                tracing::warn!(
                    task_id = %task.id,
                    conversation_id = %task.conversation_id,
                    "marking background task left running by a previous process as lost"
                );
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Cloneable conversation admission without model/tool/application services.
#[derive(Clone)]
pub struct RunOccupancy {
    sessions: Arc<DashMap<String, RunHandle>>,
    lifecycle: lifecycle::Lifecycle,
}

impl RunOccupancy {
    pub fn new(sessions: Arc<DashMap<String, RunHandle>>, lifecycle: lifecycle::Lifecycle) -> Self {
        Self {
            sessions,
            lifecycle,
        }
    }

    /// Reserve a conversation with a fresh hub and cancellation token. The
    /// application transfers the returned guard through startup and supervision.
    pub fn occupy(&self, id: &str) -> Result<OccupancyGuard, AppError> {
        self.occupy_with(id, CancellationToken::new())
    }

    pub fn occupy_with(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<OccupancyGuard, AppError> {
        self.lifecycle.check("conversation", id)?;
        self.occupy_unchecked(id, cancel)
    }

    pub(crate) fn occupy_unchecked(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<OccupancyGuard, AppError> {
        match self.sessions.entry(id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(AppError::SessionBusy(
                "Agent is already running for this conversation".into(),
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(RunHandle {
                    hub: Arc::new(EventHub::new()),
                    cancel,
                });
                Ok(OccupancyGuard {
                    sessions: self.sessions.clone(),
                    conv_id: id.to_string(),
                })
            }
        }
    }
}
