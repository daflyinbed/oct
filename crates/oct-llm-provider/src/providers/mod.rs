pub mod anthropic;
pub mod moonshotai;

use crate::provider::ProviderRegistry;

pub use anthropic::{
    AnthropicCacheCreation, AnthropicChatModel, AnthropicConfig, AnthropicContainer,
    AnthropicContentBlock, AnthropicContentBlockDelta, AnthropicProvider, AnthropicResponse,
    AnthropicServerToolUsage, AnthropicStreamEvent, AnthropicTextBlock, AnthropicToolUseBlock,
    AnthropicUsage, ANTHROPIC_API_KEY_ENV, ANTHROPIC_DEFAULT_BASE_URL,
    aggregate_anthropic_stream, anthropic_finish_reason, map_anthropic_response,
    map_anthropic_usage, normalize_anthropic_stream_event, parse_anthropic_generate_body,
    parse_anthropic_sse_event, parse_anthropic_sse_transcript,
};
pub use moonshotai::{
    MoonshotAIProvider, MOONSHOTAI_API_KEY_ENV, MOONSHOTAI_DEFAULT_BASE_URL,
};

/// Register the built-in provider set. This is the crate's single
/// environment-materialization point: the default providers read their
/// API-key env vars HERE — once, at registry construction — and share the
/// registry's HTTP client, so no adapter ever reads the environment (or
/// opens its own pool) at request time.
pub fn register_defaults(registry: &mut ProviderRegistry) {
    let client = registry.client().clone();
    registry.register(Box::new(MoonshotAIProvider::from_env_with_client(
        client.clone(),
    )));
    registry.register(Box::new(AnthropicProvider::from_env_with_client(client)));
}

pub fn default_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    register_defaults(&mut registry);
    registry
}
