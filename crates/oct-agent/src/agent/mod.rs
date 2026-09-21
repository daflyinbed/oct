pub mod events;
pub mod loop_runner;
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
        /// UI-only structured metadata; never sent to the LLM.
        #[schema(value_type = Object)]
        details: Option<serde_json::Value>,
    },

    #[serde(rename = "usage")]
    Usage {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        reasoning_tokens: Option<u32>,
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
    RunMeta {
        start_message_id: String,
    },
}

pub struct AgentContext {
    pub model: Arc<dyn ChatModel>,
    pub tools: Arc<Vec<Box<dyn AgentTool>>>,
    pub pool: SqlitePool,
    pub system_prompt: String,
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

pub type ConversationId = Uuid;
