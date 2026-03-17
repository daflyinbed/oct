pub mod anthropic;
pub mod openai;

use crate::provider::ProviderRegistry;

pub use anthropic::{map_anthropic_usage, AnthropicChatModel, AnthropicConfig, AnthropicProvider};
pub use openai::OpenAiProvider;

pub fn register_defaults(registry: &mut ProviderRegistry) {
    registry.register(Box::new(OpenAiProvider::default()));
    registry.register(Box::new(AnthropicProvider::default()));
}

pub fn default_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    register_defaults(&mut registry);
    registry
}
