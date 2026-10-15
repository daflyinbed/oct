pub mod compact;
pub mod events;
pub mod loop_runner;
pub mod micro;
pub mod prompt;
pub mod repo_instructions;
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

    /// UI-only nested execution state. Never creates a model tool message.
    #[serde(rename = "nested_tool_update")]
    NestedToolUpdate {
        parent_call_id: String,
        call: crate::codemode::NestedCall,
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

pub struct AgentContext {
    pub model: Arc<dyn ChatModel>,
    /// The run's toolset as `Arc` clones: cheap per-call clones, and the
    /// `task` tool can hand the very same instances to its subagents (minus
    /// itself) instead of rebuilding stateful tools.
    pub tools: Arc<Vec<Arc<dyn AgentTool>>>,
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
    /// Run 的思考档位（send 请求携带并随 user 消息持久化，resume 恢复）。
    /// 子代理与内部模型调用（title/compact/wake）恒为 `None`：只有用户的
    /// 根会话携带档位，adapter 负责翻译成各家 wire 参数。
    pub reasoning: Option<oct_llm_provider::core::ReasoningRequest>,
    /// Anthropic's total output ceiling from the model catalog, including
    /// thinking tokens. Other adapters retain their existing defaults.
    pub max_output_tokens: Option<u32>,
    /// The run's permission gate: every tool call passes
    /// `check` before `execute`. Assembled by the RunService (root and
    /// subagent runs alike) from the persisted rules, the conversation's
    /// mode and the workspace roots.
    pub permission: std::sync::Arc<crate::permissions::gate::PermissionGate>,
    /// Marks this run's ROOT conversation frozen prompt environment dirty
    /// when compaction/microcompaction rewrites the history (the prefix
    /// cache breaks anyway; the refresh itself lands at the NEXT run's
    /// assembly — a run's prompt is constant for its whole lifetime).
    /// `None` for contexts assembled outside the RunService (test
    /// harnesses), where the flag has nothing to act on.
    pub prompt_env_refresh: Option<repo_instructions::PromptEnvRefresh>,
    /// Identity of the prompt environment this run's system prompt was built
    /// from: `set_anchor` stamps it onto every usage anchor (and onto the
    /// assistant row's details), so later runs can reject anchors measured
    /// under a different prompt. `0` = unknown (test harnesses), which no
    /// environment accepts.
    pub prompt_generation: u64,
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

/// Terminal status of an agent run: the in-memory mirror of
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
/// not be mistaken for a free one.
#[derive(Debug, Clone, Default)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The agent loop's result envelope: events keep serving the
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
