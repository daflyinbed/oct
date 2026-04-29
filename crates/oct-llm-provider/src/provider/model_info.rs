use serde::{Deserialize, Serialize};

use crate::provider::{ModelCapabilities, ModelLimits};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider_name: String,
    pub model_id: String,
    pub capabilities: ModelCapabilities,
    pub limits: ModelLimits,
}

impl ModelInfo {
    pub fn new(provider_name: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_name: provider_name.into(),
            model_id: model_id.into(),
            capabilities: ModelCapabilities::default(),
            limits: ModelLimits::default(),
        }
    }

    pub fn with_capabilities(mut self, capabilities: ModelCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_limits(mut self, limits: ModelLimits) -> Self {
        self.limits = limits;
        self
    }
}
