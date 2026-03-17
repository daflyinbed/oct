use oct::provider::Provider;
use oct::providers::OpenAiProvider;

#[test]
fn returns_openai_model_with_effective_limits() {
    let provider = OpenAiProvider::default();
    let model = provider.chat_model("gpt-4.1").unwrap();
    assert_eq!(model.info().provider_name, "openai");
    assert_eq!(model.info().limits.max_input_tokens, Some(128_000));
    assert!(model.info().capabilities.native_tools);
}
