pub mod anthropic;
pub mod moonshotai;

use crate::provider::ProviderRegistry;

pub use anthropic::{
    anthropic_finish_reason, map_anthropic_response, map_anthropic_usage,
    normalize_anthropic_stream_event, parse_anthropic_generate_body,
    parse_anthropic_sse_event, parse_anthropic_sse_transcript, AnthropicChatModel,
    AnthropicConfig, AnthropicProvider, AnthropicResponse, AnthropicStreamEvent,
};
pub use moonshotai::MoonshotAIProvider;

pub fn register_defaults(registry: &mut ProviderRegistry) {
    registry.register(Box::new(MoonshotAIProvider::default()));
    registry.register(Box::new(AnthropicProvider::default()));
}

pub fn default_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    register_defaults(&mut registry);
    registry
}
