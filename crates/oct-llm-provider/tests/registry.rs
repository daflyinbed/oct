mod common;

use common::StubProvider;
use oct_llm_provider::adapter::{map_openai_usage, ChatCompletionUsage};
use oct_llm_provider::core::ModelError;
use oct_llm_provider::provider::ProviderRegistry;
use oct_llm_provider::providers::{default_registry, map_anthropic_usage, AnthropicUsage, MoonshotAIProvider};

#[test]
fn resolves_registered_model_specs() {
    let mut registry = ProviderRegistry::new();
    registry.register(Box::new(StubProvider));

    let model = registry.resolve_chat_model("stub:test-model").unwrap();
    assert_eq!(model.info().provider_name, "stub");
    assert_eq!(model.info().model_id, "test-model");
    assert_eq!(model.info().limits.max_input_tokens, Some(8192));
}

#[test]
fn rejects_invalid_model_specs() {
    let registry = ProviderRegistry::new();
    let err = match registry.resolve_chat_model("missing-separator") {
        Ok(_) => panic!("expected invalid model spec error"),
        Err(err) => err,
    };
    assert!(matches!(err, ModelError::InvalidModelSpec(_)));
}

#[test]
fn resolves_real_moonshotai_provider_models() {
    let mut registry = ProviderRegistry::new();
    registry.register(Box::new(MoonshotAIProvider::default()));

    let model = registry.resolve_chat_model("moonshotai:kimi-k2.5").unwrap();
    assert_eq!(model.info().provider_name, "moonshotai");
    assert_eq!(model.info().model_id, "kimi-k2.5");
    assert_eq!(model.info().limits.max_output_tokens, Some(262_144));
}

#[test]
fn usage_mappers_produce_normalized_values() {
    let openai_usage = map_openai_usage(&ChatCompletionUsage {
        prompt_tokens: 5,
        completion_tokens: 2,
        total_tokens: 7,
        completion_tokens_details: None,
        prompt_tokens_details: None,
    });
    assert_eq!(openai_usage.total_tokens, Some(7));

    let anthropic_usage = map_anthropic_usage(&AnthropicUsage {
        input_tokens: 9,
        output_tokens: 4,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        cache_creation: None,
        server_tool_use: None,
        inference_geo: None,
        service_tier: None,
    });
    assert_eq!(anthropic_usage.total_tokens, Some(13));
}

#[test]
fn default_registry_registers_supported_providers() {
    let registry = default_registry();
    assert!(registry.provider("moonshotai").is_some());
    assert!(registry.provider("anthropic").is_some());
}
