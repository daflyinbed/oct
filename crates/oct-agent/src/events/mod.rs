//! Public run event vocabulary and replay transport.
mod hub;
use crate::tool_contract::OutputStream;
pub use hub::{EventHub, SubscribeStream};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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

    /// UI-only nested execution state. Never creates a model tool message.
    #[serde(rename = "nested_tool_update")]
    NestedToolUpdate {
        parent_call_id: String,
        call: crate::tool_contract::NestedCall,
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
        images: Vec<crate::tool_contract::ToolImage>,
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

    /// The background title task (see `application/title.rs`) landed an auto-generated
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

    /// Ready boundary for one execution instance, recorded once in its hub
    /// after durable preparation and before content events. Subscribers keep
    /// memberships appended at/before this revision, then replay this epoch.
    #[serde(rename = "run_meta")]
    RunMeta {
        run_id: String,
        execution_epoch: i64,
        history_epoch: i64,
        replay_after_revision: i64,
        trigger_message_id: Option<String>,
    },

    /// A subagent task of this run changed state (started / settled /
    /// recovered). Published on the PARENT run's hub only — small by design:
    /// the child's own text/reasoning/tool events stay on
    /// the child hub and are subscribed to separately. Never carries the
    /// child's `finish`/`usage`: forwarding those would wrongly end the
    /// parent stream or corrupt the parent's context ring.
    #[serde(rename = "subagent_updated")]
    SubagentUpdated {
        run_id: String,
        /// The child's conversation id (== agent id in v1).
        agent_id: String,
        /// The parent tool call this task answers.
        parent_call_id: String,
        status: SubagentRunStatus,
        /// Which execution instance produced the event: recovery bumps the
        /// epoch, letting consumers discard stale-instance events.
        execution_epoch: u64,
        description: String,
        /// Best-effort activity summary at publish time (e.g. the tool the
        /// child is currently running); `None` when unknown.
        activity: Option<String>,
    },

    /// A tool call parked on user approval: the run is
    /// suspended at the gate until the decide endpoint answers, the run is
    /// cancelled, or the approval times out. Replays idempotently off the
    /// hub log; after a page refresh the pending-list endpoint is the
    /// truth source for which approvals are still open.
    #[serde(rename = "approval_requested")]
    ApprovalRequested {
        approval_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        /// The tool call id the approval parks.
        call_id: String,
        /// Tool name as the model sees it (e.g. `write_file`).
        tool: String,
        /// Action class label (`read`/`write`/`edit`/`command`/`mcp`).
        kind: String,
        /// One-line summary: the resolved path, the command, or the tool
        /// name.
        title: String,
        /// The RESOLVED path (post-symlink), file actions only.
        path: Option<String>,
        /// The raw command, execute actions only.
        command: Option<String>,
        /// Workspace containment verdict, file actions only.
        outside: Option<bool>,
        /// A conservative "always allow" pattern the user can edit before
        /// submitting (see `permissions::evaluator::rule_suggestion`).
        rule_suggestion: Option<String>,
    },

    /// The resolution of a previously requested approval:
    /// approved/denied by the decide endpoint (which also persisted an
    /// `allow_always` rule, named by `rule_id`), or cancelled/expired by
    /// the gate and the settlement sweep. Every `ApprovalRequested` is
    /// eventually followed by exactly one of these.
    #[serde(rename = "approval_resolved")]
    ApprovalResolved {
        approval_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        outcome: ApprovalOutcome,
        /// The persisted permission rule an `allow_always` created; `None`
        /// for every other outcome (and for plain `allow`).
        rule_id: Option<String>,
    },
}

/// How a pending approval was settled (the `approval_resolved` event's
/// outcome and the approval flow's four terminal states).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// The user allowed the call (`allow` or `allow_always`).
    Approved,
    /// The user refused the call.
    Denied,
    /// The run was cancelled/settled while the approval was open.
    Cancelled,
    /// The approval timed out (fail-closed self-denial).
    Expired,
}

/// Lifecycle status of a subagent task (the `subagent_updated` event's
/// `status` and the subagent list API's status field). Mirrors the persisted
/// `agent_runs.status` vocabulary, with `running` while live and `lost`
/// meaning the executing instance died before settling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl SubagentRunStatus {
    /// Parse the persisted `agent_runs.status` string.
    pub fn from_run_status(status: &str) -> Self {
        match status {
            "completed" => SubagentRunStatus::Completed,
            "failed" => SubagentRunStatus::Failed,
            "cancelled" => SubagentRunStatus::Cancelled,
            "lost" => SubagentRunStatus::Lost,
            _ => SubagentRunStatus::Running,
        }
    }
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
