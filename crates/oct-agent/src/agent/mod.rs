pub mod loop_runner;
pub mod prompt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Events emitted by the agent loop, streamed as SSE to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", content = "data")]
pub enum AgentEvent {
    /// Streamed text content from the assistant.
    #[serde(rename = "text_delta")]
    TextDelta(String),

    /// Streamed reasoning/thinking content.
    #[serde(rename = "reasoning_delta")]
    ReasoningDelta(String),

    /// A tool call has started execution.
    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
    },

    /// Result of a tool execution.
    #[serde(rename = "tool_result")]
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },

    /// Token usage update.
    #[serde(rename = "usage")]
    Usage {
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        reasoning_tokens: Option<u32>,
    },

    /// Agent loop has finished.
    #[serde(rename = "finish")]
    Finish,

    /// An error occurred.
    #[serde(rename = "error")]
    Error(String),
}

/// Configuration for an agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Provider and model spec, e.g. "anthropic:claude-sonnet-4-20250514".
    pub provider_spec: String,
    /// Working directory for file operations.
    pub working_dir: std::path::PathBuf,
}

/// Unique identifier for a conversation.
pub type ConversationId = Uuid;
