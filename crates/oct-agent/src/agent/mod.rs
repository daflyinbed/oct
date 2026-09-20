pub mod loop_runner;
pub mod prompt;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::tools::{AgentTool, OutputStream};
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

    #[serde(rename = "cancelled")]
    Cancelled,

    #[serde(rename = "error")]
    Error(String),
}

pub struct AgentContext {
    pub model: Arc<dyn ChatModel>,
    pub tools: Arc<Vec<Box<dyn AgentTool>>>,
    pub pool: SqlitePool,
    pub system_prompt: String,
}

#[derive(Clone)]
pub struct RunHandle {
    pub event_tx: broadcast::Sender<AgentEvent>,
    pub cancel: CancellationToken,
}

impl RunHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

pub type ConversationId = Uuid;
