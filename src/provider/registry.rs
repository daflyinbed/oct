use std::collections::HashMap;

use crate::core::ModelError;
use crate::model::ChatModel;

use super::Provider;

pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn Provider>) -> Option<Box<dyn Provider>> {
        self.providers.insert(provider.name().to_string(), provider)
    }

    pub fn provider(&self, name: &str) -> Option<&dyn Provider> {
        self.providers.get(name).map(Box::as_ref)
    }

    pub fn resolve_chat_model(&self, spec: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        let (provider_name, model_id) = spec
            .split_once(':')
            .ok_or_else(|| ModelError::invalid_model_spec(spec))?;

        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| ModelError::unknown_provider(provider_name))?;

        provider.chat_model(model_id)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use crate::core::{FinishReason, Message, ModelError, Role};
    use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
    use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};
    use crate::providers::{default_registry, map_anthropic_usage, OpenAiProvider};
    use crate::adapter::map_openai_usage;

    use super::ProviderRegistry;

    struct StubChatModel {
        info: ModelInfo,
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

    struct StubProvider;

    impl Provider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
            Ok(Box::new(StubChatModel {
                info: ModelInfo::new("stub", model).with_capabilities(ModelCapabilities {
                    streaming: false,
                    native_tools: false,
                    vision: false,
                    json_mode: false,
                    reasoning: false,
                    usage: true,
                }).with_limits(ModelLimits {
                    max_input_tokens: Some(8192),
                    max_output_tokens: Some(4096),
                    max_total_tokens: Some(12288),
                }),
            }))
        }
    }

    #[test]
    fn resolves_registered_model_specs() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(StubProvider));

        let model = registry.resolve_chat_model("stub:test-model").unwrap();
        assert_eq!(model.info().provider_name, "stub");
        assert_eq!(model.info().model_id, "test-model");
        assert_eq!(model.info().limits.max_input_tokens, Some(8192));
    }

    #[test]
    fn rejects_invalid_model_specs() {
        let registry = ProviderRegistry::new();
        let err = match registry.resolve_chat_model("missing-separator") {
            Ok(_) => panic!("expected invalid model spec error"),
            Err(err) => err,
        };
        assert!(matches!(err, ModelError::InvalidModelSpec(_)));
    }

    #[test]
    fn resolves_real_openai_provider_models() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(OpenAiProvider::default()));

        let model = registry.resolve_chat_model("openai:gpt-4.1").unwrap();
        assert_eq!(model.info().provider_name, "openai");
        assert_eq!(model.info().model_id, "gpt-4.1");
        assert_eq!(model.info().limits.max_output_tokens, Some(16_384));
    }

    #[test]
    fn usage_mappers_produce_normalized_values() {
        let openai_usage = map_openai_usage(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 2,
            "total_tokens": 7
        }));
        assert_eq!(openai_usage.total_tokens, Some(7));

        let anthropic_usage = map_anthropic_usage(&json!({
            "input_tokens": 9,
            "output_tokens": 4
        }));
        assert_eq!(anthropic_usage.total_tokens, Some(13));
    }

    #[test]
    fn default_registry_registers_supported_providers() {
        let registry = default_registry();
        assert!(registry.provider("openai").is_some());
        assert!(registry.provider("anthropic").is_some());
    }
}
