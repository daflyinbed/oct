pub mod compact;
pub mod events;
pub mod loop_runner;
pub mod micro;
pub mod prompt;
pub mod title;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::tools::{AgentTool, OutputStream};
pub use events::EventHub;
use oct_llm_provider::model::ChatModel;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    #[serde(rename = "text_delta")]
    TextDelta(String),

    #[serde(rename = "reasoning_delta")]
    ReasoningDelta(String),

    /// The model is generating a tool call (forwarded verbatim from the
    /// provider stream).
    #[serde(rename = "tool_call_delta")]
    ToolCallDelta {
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
    },

    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
        /// Human-readable title for this call (e.g. the command being run),
        /// derived from the arguments for the UI.
        title: String,
    },

    /// Live tool output while a call is still running. Live view only:
    /// never persisted and never sent to the LLM.
    #[serde(rename = "tool_output_delta")]
    ToolOutputDelta {
        call_id: String,
        stream: OutputStream,
        delta: String,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        /// Images attached to the result (base64 + MIME type), passed
        /// through to the model as multimodal tool-result content. Absent
        /// when the result is text-only; the frontend does not render them
        /// yet.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::tools::ToolImage>,
        /// UI-only structured metadata; never sent to the LLM.
        #[schema(value_type = Object)]
        details: Option<serde_json::Value>,
    },

    #[serde(rename = "usage")]
    Usage {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        reasoning_tokens: Option<u32>,
        /// Context window of the resolved model, when known. The loop knows
        /// it from the model info / models table; the primary send stream has
        /// no run_meta, so this is where a UI learns the denominator for a
        /// context-usage percentage.
        context_window_tokens: Option<u32>,
    },

    /// Auto compaction started at a turn boundary: the loop is summarizing
    /// old history (a blocking, tool-less model call inside the run).
    #[serde(rename = "compacting")]
    Compacting { triggered_by: CompactionTrigger },

    /// Compaction completed: the marker row is persisted and every subsequent
    /// LLM request carries `[marker summary, verbatim tail]` instead of the
    /// compacted history. Carries the summary text so a live UI can render
    /// the divider without a refetch (a later fetch renders the same row).
    #[serde(rename = "compacted")]
    Compacted {
        summary_message_id: String,
        triggered_by: CompactionTrigger,
        /// Number of message rows the marker replaced in the LLM view (they
        /// remain in the DB, just excluded from future requests).
        compacted_messages: u32,
        est_tokens_before: u64,
        est_tokens_after: u64,
        summary: String,
        context_window_tokens: Option<u32>,
    },

    /// Microcompact completed: old successful tool results were replaced by
    /// placeholder copies (copy-on-write; no model call involved). Carries
    /// the replacements so a live UI can swap its historical tool cards to
    /// the cleared state in place; replay-safe (keyed by call_id).
    #[serde(rename = "microcompacted")]
    Microcompacted {
        triggered_by: MicrocompactTrigger,
        replacements: Vec<MicrocompactReplacement>,
        /// Estimated tokens freed by the clear.
        est_tokens_saved: u64,
        /// Estimated context size of the NEXT request after the clear
        /// (anchor-aware; same口径 as the compaction trigger's estimate).
        est_context_tokens: u64,
    },

    #[serde(rename = "finish")]
    Finish,

    /// The background title task (see [`title`]) landed an auto-generated
    /// conversation title. Published on the run's hub — not by the agent loop
    /// — so live subscribers and reconnecting replays both see it; handling
    /// must be idempotent (a replay delivers it again). Conversation
    /// metadata, not message content: consumers update the conversation
    /// list, not the message stream.
    #[serde(rename = "title_updated")]
    TitleUpdated { title: String },

    #[serde(rename = "cancelled")]
    Cancelled,

    #[serde(rename = "error")]
    Error(String),

    /// Run-boundary metadata for a reconnecting subscriber: the id of the
    /// user message row that started this run. NEVER enters the hub's replay
    /// log and is never published by the agent loop — the events endpoint
    /// injects it (per subscriber) in front of the replay so the frontend can
    /// truncate its DB-loaded history exactly at the run boundary.
    #[serde(rename = "run_meta")]
    RunMeta { start_message_id: String },
}

pub struct AgentContext {
    pub model: Arc<dyn ChatModel>,
    pub tools: Arc<Vec<Box<dyn AgentTool>>>,
    pub pool: SqlitePool,
    pub system_prompt: String,
    /// Context window (max input tokens) of the resolved model: built-in
    /// provider info first, then the models table's limit_context. `None`
    /// disables AUTO compaction (manual still works).
    pub context_window: Option<u32>,
    /// How long the conversation sat idle before this run started, captured
    /// in the API layer BEFORE the repair/new-user-message writes (anything
    /// later would always look "just active"). Drives microcompact's idle
    /// entry; `None`/short values mean waterline-only.
    pub idle: Option<std::time::Duration>,
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Auto,
    Manual,
}

/// What started a microcompaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MicrocompactTrigger {
    /// The estimated context crossed the waterline (70% of the window).
    Waterline,
    /// The run started long after the conversation's last activity.
    Idle,
}

/// One copy-on-write replacement from a microcompact batch: the tool result
/// named by `call_id` now shows its cleared copy. The copy row id is what a
/// "view original" affordance follows (its supersedes back-pointer leads to
/// the original content).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MicrocompactReplacement {
    pub call_id: String,
    pub copy_message_id: String,
}

#[derive(Clone)]
pub struct RunHandle {
    /// This run's event hub: replay log + live fan-out. Shared by the agent
    /// loop (publisher), tool contexts (publisher) and every SSE subscriber.
    pub hub: Arc<EventHub>,
    pub cancel: CancellationToken,
    /// The user message row that started this run — the boundary between the
    /// DB history (everything up to and including it) and this run's event
    /// log (everything after it). `None` for resumed runs (no new user
    /// message) and in the brief window before the user message is inserted.
    pub start_message_id: Option<String>,
}

impl RunHandle {
    pub fn subscribe(&self) -> events::SubscribeStream {
        self.hub.subscribe()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Terminal status of an agent run (ADR 0002 §6.1): the in-memory mirror of
/// the persisted `agent_runs.status` terminal values. `lost` is
/// process-crash only and never produced by a live loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Failed,
    Cancelled,
}

/// Token usage aggregated over a run's Usage stream events. `None` (unknown)
/// is deliberately distinct from zero: a run that never reported usage must
/// not be mistaken for a free one (ADR 0002 §6.3).
#[derive(Debug, Clone, Default)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The agent loop's result envelope (ADR 0002 §6.1): events keep serving the
/// UI, but the run's outcome is a value the supervisor can persist — never
/// something a listener must reassemble from SSE text.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub status: RunStatus,
    /// The persisted final assistant message row. Set when at least one
    /// assistant message landed in the DB during the run.
    pub final_message_id: Option<String>,
    /// Its text; `None` when the row exists but its text is empty.
    pub final_text: Option<String>,
    /// Interrupted (failed/cancelled) but some assistant output was
    /// persisted — the persisted tail must not be presented as a settled
    /// success.
    pub partial: bool,
    /// Aggregated usage; `None` = no Usage event ever arrived (unknown).
    pub usage: Option<RunUsage>,
    /// Failure detail for [`RunStatus::Failed`].
    pub error: Option<String>,
}

pub type ConversationId = Uuid;
