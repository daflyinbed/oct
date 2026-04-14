use serde::{Deserialize, Serialize};

use crate::core::tool::{ToolCall, ToolResult};
use crate::core::usage::Usage;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Usage(Usage),
    Finish(FinishReason),
}
