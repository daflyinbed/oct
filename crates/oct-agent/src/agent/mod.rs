pub mod compact;
use crate::events;
pub mod loop_runner;
pub mod micro;
pub mod prompt;
pub mod repo_instructions;
pub mod title;

use sqlx::SqlitePool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::EventHub;
use crate::tool_contract::AgentTool;
use oct_llm_provider::model::ChatModel;

pub struct AgentContext {
    pub model: Arc<dyn ChatModel>,
    /// The run's toolset as `Arc` clones: cheap per-call clones, and the
    /// `task` port captures frozen presentation and reusable MCP instances;
    /// workers rebuild native tools with independent read state.
    pub tools: Arc<Vec<Arc<dyn AgentTool>>>,
    pub pool: SqlitePool,
    pub system_prompt: String,
    /// Context window (max input tokens) of the resolved model: built-in
    /// provider info first, then the models table's limit_context. `None`
    /// disables AUTO compaction (manual still works).
    pub context_window: Option<u32>,
    /// How long the conversation sat idle before this run started, captured
    /// in the application BEFORE the repair/new-user-message writes (anything
    /// later would always look "just active"). Drives microcompact's idle
    /// entry; `None`/short values mean waterline-only.
    pub idle: Option<std::time::Duration>,
    /// Run 的思考档位（send 请求携带并随 user 消息持久化，resume 恢复）。
    /// 子代理与内部模型调用（title/compact/wake）恒为 `None`：只有用户的
    /// 根会话携带档位，adapter 负责翻译成各家 wire 参数。
    pub reasoning: Option<oct_llm_provider::core::ReasoningRequest>,
    /// 模型可接受的输入模态（models 表 `modalities_input` 的解释）。请求
    /// 装配时以此门控工具结果图片——文本模型收到图片会被 provider 直接
    /// 400。`Unknown`（列缺失/损坏）放行一切，保持门控前的行为。
    pub input_modalities: crate::modality::InputModalities,
    /// Anthropic's total output ceiling from the model catalog, including
    /// thinking tokens. Other adapters retain their existing defaults.
    pub max_output_tokens: Option<u32>,
    /// 附件存储：请求装配时把 `oct-attachment://` 引用水合成 data URL。
    /// `None` = 测试直接装配，引用原样保留。
    pub attachments: Option<std::sync::Arc<crate::attachments::AttachmentService>>,
    /// The run's permission gate: every tool call passes
    /// `check` before `execute`. Assembled by the application (root and
    /// subagent runs alike) from the persisted rules, the conversation's
    /// mode and the workspace roots.
    pub permission: std::sync::Arc<crate::permissions::gate::PermissionGate>,
    /// Marks this run's ROOT conversation frozen prompt environment dirty
    /// when compaction/microcompaction rewrites the history (the prefix
    /// cache breaks anyway; the refresh itself lands at the NEXT run's
    /// assembly — a run's prompt is constant for its whole lifetime).
    /// `None` for contexts assembled outside application assembly (test
    /// harnesses), where the flag has nothing to act on.
    pub prompt_env_refresh: Option<repo_instructions::PromptEnvRefresh>,
    /// Identity of the prompt environment this run's system prompt was built
    /// from: `set_anchor` stamps it onto every usage anchor (and onto the
    /// assistant row's details), so later runs can reject anchors measured
    /// under a different prompt. `0` = unknown (test harnesses), which no
    /// environment accepts.
    pub prompt_generation: u64,
}

#[derive(Clone)]
pub struct RunHandle {
    /// This run's event hub: replay log + live fan-out. Shared by the agent
    /// loop (publisher), tool contexts (publisher) and every SSE subscriber.
    pub hub: Arc<EventHub>,
    pub cancel: CancellationToken,
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
