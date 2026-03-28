use oct::provider::Provider;
use oct::providers::MoonshotAIProvider;

#[test]
fn returns_moonshotai_model_with_effective_limits() {
    let provider = MoonshotAIProvider::default();
    let model = provider.chat_model("kimi-k2.5").unwrap();
    assert_eq!(model.info().provider_name, "moonshotai");
    assert_eq!(model.info().limits.max_input_tokens, Some(262_144));
    assert!(model.info().capabilities.native_tools);
}
