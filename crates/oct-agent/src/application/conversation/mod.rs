//! Conversation commands. RunService remains the execution backend while its
//! assembly and supervision are migrated independently of these entry points.

mod compact;
pub mod preparation;
pub mod recovery;
mod resume;
mod send;
mod validation;
mod wake;

use std::sync::Arc;

use oct_llm_provider::core::ReasoningRequest;

use crate::agent::events::SubscribeStream;
use crate::db::conversations::Conversation;
use crate::error::{AppError, AppResult};
use crate::runtime::{RootRunRequest, RunService, StartedRun};
use validation::{conversation_idle, validate_reasoning};

#[derive(Clone)]
pub struct ConversationApplication {
    runs: Arc<RunService>,
}

pub struct SendCommand {
    pub conversation_id: String,
    pub content: String,
    pub provider_spec: String,
    pub expected_revision: Option<i64>,
    pub client_request_id: Option<String>,
    pub reasoning: Option<ReasoningRequest>,
    pub attachment_ids: Vec<String>,
}

pub struct ResumeCommand {
    pub conversation_id: String,
}

pub struct CompactCommand {
    pub conversation_id: String,
    pub provider_spec: Option<String>,
}

pub struct WakeCommand {
    pub conversation_id: String,
}

/// A subscription may begin during recovery or end during preparation,
/// before a new root run has been registered. It never owns execution.
pub struct RunSubscription {
    pub events: SubscribeStream,
    pub run_id: Option<String>,
}

impl From<StartedRun> for RunSubscription {
    fn from(started: StartedRun) -> Self {
        Self {
            events: started.events,
            run_id: Some(started.run_id),
        }
    }
}

pub struct CompactOutcome {
    pub summary_message_id: String,
    pub compacted_messages: u32,
    pub est_tokens_before: u64,
    pub est_tokens_after: u64,
    pub summary: String,
}

impl ConversationApplication {
    pub fn new(runs: Arc<RunService>) -> Self {
        Self { runs }
    }

    pub async fn send(&self, command: SendCommand) -> AppResult<RunSubscription> {
        send::send(&self.runs, command).await
    }

    pub async fn resume(&self, command: ResumeCommand) -> AppResult<RunSubscription> {
        resume::resume(&self.runs, command).await
    }

    pub async fn compact(&self, command: CompactCommand) -> AppResult<CompactOutcome> {
        compact::compact(&self.runs, command).await
    }

    /// Deliver durable notifications and wait for the wake execution to settle.
    /// The drainer owns retries; this operation never duplicates a committed
    /// notification or consumes an intent merely because startup succeeded.
    pub async fn wake(&self, command: WakeCommand) -> AppResult<()> {
        wake::wake(&self.runs, &command.conversation_id).await
    }
}

/// Shared handoff for send, recovered roots and background wake executions.
/// Registration, replay metadata and supervised settlement remain in runtime.
async fn launch_root(runs: &RunService, request: RootRunRequest) -> AppResult<StartedRun> {
    runs.start_root_run(request).await
}

/// 409 when the conversation is a managed subagent session: send / resume /
/// compact must go through the manager (or the parent), never start a run
/// behind its back.
pub(crate) fn ensure_not_managed(conversation: &Conversation) -> AppResult<()> {
    if conversation.kind == "subagent" {
        return Err(AppError::ManagedConversation(
            "Conversation is a managed subagent session; drive it from the parent conversation"
                .into(),
        ));
    }
    Ok(())
}
