use std::collections::HashMap;

use crate::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use crate::core::ModelError;
use crate::model::ChatModel;
use crate::provider::{ConnectionConfig, ModelCapabilities, ModelInfo, ModelLimits};

use super::Provider;

pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
    /// Shared HTTP client handed to every adapter the registry builds (or,
    /// through `register_defaults`, to the default providers themselves):
    /// reqwest clients clone cheaply and share their connection pool, so
    /// per-run model construction reuses one pool instead of opening a new
    /// one per request loop.
    client: reqwest::Client,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_client(reqwest::Client::new())
    }

    /// Registry over a caller-supplied (shared) HTTP client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            providers: HashMap::new(),
            client,
        }
    }

    /// The shared client, so `register_defaults` can hand the same pool to
    /// every default provider.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
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

    /// Resolve a chat model whose connection comes from a caller-resolved
    /// [`ConnectionConfig`] — the DB-driven construction path. Every `Some`
    /// field overrides the provider family's default; `None` falls back to
    /// it (built-in base URL, startup-materialized env key).
    pub fn resolve_chat_model_with_connection(
        &self,
        provider_name: &str,
        model_id: &str,
        connection: &ConnectionConfig,
    ) -> Result<Box<dyn ChatModel>, ModelError> {
        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| ModelError::unknown_provider(provider_name))?;

        provider.chat_model_with_connection(model_id, connection)
    }

    /// The family's own (default) connection for a registered provider —
    /// built-in base URL plus the env key materialized at construction.
    /// `None` when no provider of that name is registered.
    pub fn provider_default_connection(&self, provider_name: &str) -> Option<ConnectionConfig> {
        self.providers
            .get(provider_name)
            .map(|provider| provider.default_connection())
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
            api_key: Some(api_key.to_string()),
            default_headers: serde_json::Map::new(),
            model_info,
            use_responses_api: false,
        };

        Ok(Box::new(OpenAiCompatibleChatModel::new_with_client(
            config,
            self.client.clone(),
        )))
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
