use crate::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use crate::core::ModelError;
use crate::model::ChatModel;
use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key_env: &'static str,
}

impl OpenAiProvider {
    pub fn new(base_url: impl Into<String>, api_key_env: &'static str) -> Self {
        Self {
            base_url: base_url.into(),
            api_key_env,
        }
    }

    fn model_info(&self, model: &str) -> ModelInfo {
        ModelInfo::new("openai", model)
            .with_capabilities(ModelCapabilities {
                streaming: true,
                native_tools: true,
                vision: true,
                json_mode: true,
                reasoning: true,
                usage: true,
            })
            .with_limits(ModelLimits {
                max_input_tokens: Some(128_000),
                max_output_tokens: Some(16_384),
                max_total_tokens: Some(128_000),
            })
    }
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new("https://api.openai.com/v1", "OPENAI_API_KEY")
    }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        Ok(Box::new(OpenAiCompatibleChatModel::new(
            OpenAiCompatibleConfig {
                provider_name: "openai",
                base_url: self.base_url.clone(),
                api_key_env: self.api_key_env,
                default_headers: Default::default(),
                model_info: self.model_info(model),
                use_responses_api: false,
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::Provider;

    use super::OpenAiProvider;

    #[test]
    fn returns_openai_model_with_effective_limits() {
        let provider = OpenAiProvider::default();
        let model = provider.chat_model("gpt-4.1").unwrap();
        assert_eq!(model.info().provider_name, "openai");
        assert_eq!(model.info().limits.max_input_tokens, Some(128_000));
        assert!(model.info().capabilities.native_tools);
    }
}
