//! Admission and occupancy for history commands, shared independently of HTTP.
use std::sync::Arc;

use crate::db::{conversations, history};
use crate::error::{AppError, AppResult};
use crate::history::{ForkRequest, RevertRequest};
use crate::runtime::RunService;

#[derive(Clone)]
pub struct HistoryApplication {
    runs: Arc<RunService>,
}

impl HistoryApplication {
    pub fn new(runs: Arc<RunService>) -> Self {
        Self { runs }
    }

    pub async fn fork(
        &self,
        source_id: &str,
        request: ForkRequest,
    ) -> AppResult<history::ForkResponse> {
        validate_operation(&request.operation_id, request.expected_revision)?;
        if request
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(AppError::BadRequest("Fork title cannot be empty".into()));
        }
        let runs = &self.runs;
        let _admission = runs.lifecycle.admission.read().await;
        runs.lifecycle.check("conversation", source_id)?;
        let source = conversations::get_conversation(&runs.pool, source_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
        runs.lifecycle.check("project", &source.project_id)?;
        if source.kind != "chat" {
            return Err(AppError::ManagedConversation(
                "Managed subagent conversations cannot be forked".into(),
            ));
        }
        // Completed operations replay before occupancy, even if the source
        // has since started another run. The write transaction checks again.
        if let Some(response) = history::replay(&runs.pool, source_id, &request).await? {
            return Ok(response);
        }
        let _occupancy = runs.occupy(source_id)?;
        super::conversation::preparation::ensure_no_pending_recovery(&runs.pool, source_id).await?;
        if !runs
            .subagents
            .active_children_of_root(&runs.pool, source_id)
            .await?
            .is_empty()
        {
            return Err(AppError::SessionBusy(
                "Subagent tasks are still running".into(),
            ));
        }
        history::fork(&runs.pool, &source, &request).await
    }

    pub async fn revert(
        &self,
        source_id: &str,
        request: RevertRequest,
    ) -> AppResult<history::RevertResponse> {
        validate_operation(&request.operation_id, request.expected_revision)?;
        let runs = &self.runs;
        let _admission = runs.lifecycle.admission.read().await;
        runs.lifecycle.check("conversation", source_id)?;
        let source = conversations::get_conversation(&runs.pool, source_id)
            .await?
            .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
        runs.lifecycle.check("project", &source.project_id)?;
        if source.kind != "chat" {
            return Err(AppError::ManagedConversation(
                "Managed subagent conversations cannot be reverted".into(),
            ));
        }
        source.ensure_writable()?;
        if let Some(response) = history::replay_revert(&runs.pool, source_id, &request).await? {
            return Ok(response);
        }
        let _occupancy = runs.occupy(source_id)?;
        super::conversation::preparation::ensure_no_pending_recovery(&runs.pool, source_id).await?;
        if !runs
            .subagents
            .active_children_of_root(&runs.pool, source_id)
            .await?
            .is_empty()
        {
            return Err(AppError::PendingRecovery(
                "Subagent tasks must settle before reverting".into(),
            ));
        }
        history::revert(&runs.pool, &source, &request).await
    }
}

fn validate_operation(id: &str, revision: i64) -> AppResult<()> {
    if uuid::Uuid::parse_str(id).is_err() || revision < 0 {
        return Err(AppError::BadRequest(
            "operation_id must be a UUID and expected_revision must be nonnegative".into(),
        ));
    }
    Ok(())
}
