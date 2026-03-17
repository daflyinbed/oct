mod common;

use serde_json::json;

use common::StubProvider;
use oct::adapter::map_openai_usage;
use oct::core::ModelError;
use oct::provider::ProviderRegistry;
use oct::providers::{default_registry, map_anthropic_usage, OpenAiProvider};

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
fn resolves_real_openai_provider_models() {
    let mut registry = ProviderRegistry::new();
    registry.register(Box::new(OpenAiProvider::default()));

    let model = registry.resolve_chat_model("openai:gpt-4.1").unwrap();
    assert_eq!(model.info().provider_name, "openai");
    assert_eq!(model.info().model_id, "gpt-4.1");
    assert_eq!(model.info().limits.max_output_tokens, Some(16_384));
}

#[test]
fn usage_mappers_produce_normalized_values() {
    let openai_usage = map_openai_usage(&json!({
        "prompt_tokens": 5,
        "completion_tokens": 2,
        "total_tokens": 7
    }));
    assert_eq!(openai_usage.total_tokens, Some(7));

    let anthropic_usage = map_anthropic_usage(&json!({
        "input_tokens": 9,
        "output_tokens": 4
    }));
    assert_eq!(anthropic_usage.total_tokens, Some(13));
}

#[test]
fn default_registry_registers_supported_providers() {
    let registry = default_registry();
    assert!(registry.provider("openai").is_some());
    assert!(registry.provider("anthropic").is_some());
}
