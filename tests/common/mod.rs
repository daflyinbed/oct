use async_trait::async_trait;

use oct::core::{FinishReason, Message, ModelError, Role};
use oct::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use oct::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};

pub struct StubChatModel {
    pub info: ModelInfo,
}

#[async_trait]
impl ChatModel for StubChatModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn generate(&self, _req: ChatRequest) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse {
            message: Message::text(Role::Assistant, "ok"),
            finish_reason: FinishReason::Stop,
            usage: None,
            provider_response_id: None,
            provider_metadata: Default::default(),
        })
    }

    async fn stream(&self, _req: ChatRequest) -> Result<ChatStream, ModelError> {
        Err(ModelError::unsupported("stream not implemented in stub"))
    }
}

pub struct StubProvider;

impl Provider for StubProvider {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        Ok(Box::new(StubChatModel {
            info: ModelInfo::new("stub", model)
                .with_capabilities(ModelCapabilities {
                    streaming: false,
                    native_tools: false,
                    vision: false,
                    json_mode: false,
                    reasoning: false,
                    usage: true,
                })
                .with_limits(ModelLimits {
                    max_input_tokens: Some(8192),
                    max_output_tokens: Some(4096),
                    max_total_tokens: Some(12288),
                }),
        }))
    }
}
