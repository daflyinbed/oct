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
