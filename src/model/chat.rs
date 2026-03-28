use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::pin::Pin;

use crate::core::{FinishReason, GenerateOptions, Message, ModelError, StreamEvent, ToolSpec, Usage};
use crate::provider::ModelInfo;

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ModelError>> + Send>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub options: GenerateOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatResponse {
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
    pub provider_response_id: Option<String>,
    pub provider_metadata: Map<String, Value>,
}

#[async_trait]
pub trait ChatModel: Send + Sync {
    fn info(&self) -> &ModelInfo;

    async fn generate(&self, req: ChatRequest) -> Result<ChatResponse, ModelError>;
    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError>;
}
