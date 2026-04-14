use crate::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use crate::core::ModelError;
use crate::model::ChatModel;
use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize)]
pub struct MoonshotAIOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<MoonshotAIThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoonshotAIThinking {
    #[serde(rename = "type")]
    pub kind: MoonshotAIThinkingMode,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum MoonshotAIThinkingMode {
    #[serde(rename = "enabled")]
    Enabled,
    #[serde(rename = "disabled")]
    Disabled,
}

impl MoonshotAIOptions {
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone)]
pub struct MoonshotAIProvider {
    pub base_url: String,
    pub api_key_env: &'static str,
}

impl MoonshotAIProvider {
    pub fn new(base_url: impl Into<String>, api_key_env: &'static str) -> Self {
        Self {
            base_url: base_url.into(),
            api_key_env,
        }
    }

    fn model_info(&self, model: &str) -> ModelInfo {
        ModelInfo::new("moonshotai", model)
            .with_capabilities(ModelCapabilities {
                streaming: true,
                native_tools: true,
                vision: true,
                json_mode: true,
                reasoning: true,
                usage: true,
            })
            .with_limits(ModelLimits {
                max_input_tokens: Some(262_144),
                max_output_tokens: Some(262_144),
                max_total_tokens: Some(262_144),
            })
    }
}

impl Default for MoonshotAIProvider {
    fn default() -> Self {
        Self::new("https://api.moonshot.cn/v1", "MOONSHOTAI_API_KEY")
    }
}

impl Provider for MoonshotAIProvider {
    fn name(&self) -> &'static str {
        "moonshotai"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        Ok(Box::new(OpenAiCompatibleChatModel::new(
            OpenAiCompatibleConfig {
                provider_name: "moonshotai",
                base_url: self.base_url.clone(),
                api_key_env: self.api_key_env,
                default_headers: Default::default(),
                model_info: self.model_info(model),
                use_responses_api: false,
            },
        )))
    }
}
