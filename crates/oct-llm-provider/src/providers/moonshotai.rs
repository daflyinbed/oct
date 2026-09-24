use crate::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use crate::core::ModelError;
use crate::model::ChatModel;
use crate::provider::{ConnectionConfig, ModelCapabilities, ModelInfo, ModelLimits, Provider};
use serde::Serialize;
use serde_json::Value;

/// Built-in endpoint used when neither the DB provider row nor the
/// environment names another one.
pub const MOONSHOTAI_DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";

/// The only environment variable the Moonshot family reads — once, at
/// provider construction (see [`MoonshotAIProvider::from_env`]).
pub const MOONSHOTAI_API_KEY_ENV: &str = "MOONSHOTAI_API_KEY";

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
    /// API key materialized from [`MOONSHOTAI_API_KEY_ENV`] at construction
    /// — the model-resolution layer may override it per row via
    /// [`Provider::chat_model_with_connection`].
    pub api_key: Option<String>,
    /// Shared HTTP client handed to every model this provider builds.
    client: reqwest::Client,
}

impl MoonshotAIProvider {
    /// Provider with explicit default connection settings.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self::with_client(base_url, api_key, reqwest::Client::new())
    }

    /// [`MoonshotAIProvider::new`] over a caller-supplied (shared) HTTP
    /// client.
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            client,
        }
    }

    /// The Moonshot family's single environment entry point: reads
    /// [`MOONSHOTAI_API_KEY_ENV`] once and materializes it into the default
    /// connection. Adapters built from this provider never read the env.
    pub fn from_env() -> Self {
        Self::from_env_with_client(reqwest::Client::new())
    }

    /// [`MoonshotAIProvider::from_env`] over a shared HTTP client (what
    /// `register_defaults` uses so every default provider shares one pool).
    pub fn from_env_with_client(client: reqwest::Client) -> Self {
        Self::with_client(
            MOONSHOTAI_DEFAULT_BASE_URL,
            crate::provider::connection::env_api_key(MOONSHOTAI_API_KEY_ENV),
            client,
        )
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
        Self::from_env()
    }
}

impl Provider for MoonshotAIProvider {
    fn name(&self) -> &'static str {
        "moonshotai"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        self.chat_model_with_connection(model, &ConnectionConfig::new())
    }

    /// DB-driven construction: every `Some` field of `connection` wins
    /// outright; `None` falls back to the family's own default (built-in
    /// base URL, `MOONSHOTAI_API_KEY` materialized at construction).
    fn chat_model_with_connection(
        &self,
        model: &str,
        connection: &ConnectionConfig,
    ) -> Result<Box<dyn ChatModel>, ModelError> {
        Ok(Box::new(OpenAiCompatibleChatModel::new_with_client(
            OpenAiCompatibleConfig {
                provider_name: "moonshotai".to_string(),
                base_url: connection
                    .base_url
                    .clone()
                    .unwrap_or_else(|| self.base_url.clone()),
                api_key: connection.api_key.clone().or(self.api_key.clone()),
                default_headers: Default::default(),
                model_info: self.model_info(model),
                use_responses_api: false,
            },
            self.client.clone(),
        )))
    }

    fn default_connection(&self) -> ConnectionConfig {
        ConnectionConfig {
            base_url: Some(self.base_url.clone()),
            api_key: self.api_key.clone(),
        }
    }
}
