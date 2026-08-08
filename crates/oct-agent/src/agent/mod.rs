pub mod loop_runner;
pub mod prompt;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::tools::AgentTool;
use oct_llm_provider::model::ChatModel;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    #[serde(rename = "text_delta")]
    TextDelta(String),

    #[serde(rename = "reasoning_delta")]
    ReasoningDelta(String),

    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
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
