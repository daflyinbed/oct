use std::collections::HashMap;

use crate::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use crate::core::ModelError;
use crate::model::ChatModel;
use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits};

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

    pub fn resolve_custom_openai_model(
        &self,
        provider_name: &str,
        model_id: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<Box<dyn ChatModel>, ModelError> {
        let model_info = ModelInfo::new(provider_name, model_id)
            .with_capabilities(ModelCapabilities {
                streaming: true,
                native_tools: true,
                vision: true,
                json_mode: true,
                reasoning: true,
                usage: true,
            })
            .with_limits(ModelLimits {
                max_input_tokens: None,
                max_output_tokens: None,
                max_total_tokens: None,
            });

        let config = OpenAiCompatibleConfig {
            provider_name: provider_name.to_string(),
            base_url: base_url.to_string(),
            api_key_env: String::new(),
            default_headers: serde_json::Map::new(),
            model_info,
            use_responses_api: false,
        };

        let model = OpenAiCompatibleChatModel::new_with_api_key(
            config,
            api_key.to_string(),
            reqwest::Client::new(),
        );

        Ok(Box::new(model))
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
