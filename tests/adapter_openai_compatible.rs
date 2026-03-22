use serde_json::json;

use oct::adapter::{
    map_openai_finish_reason, map_openai_generate_response, map_openai_usage,
    normalize_openai_stream_chunk, parse_openai_generate_body, parse_openai_sse_event,
    parse_openai_sse_transcript, OpenAiChoice, OpenAiCompatibleChatModel, OpenAiCompatibleConfig,
    OpenAiGenerateResponse, OpenAiRequestMessage, OpenAiStreamChunk, OpenAiStreamChunkChoice,
    OpenAiStreamChunkChoiceDelta,
};
use oct::core::{ContentPart, FinishReason, GenerateOptions, Message, Role, ToolChoice, ToolSpec};
use oct::model::ChatRequest;
use oct::provider::{ModelCapabilities, ModelInfo, ModelLimits};

#[test]
fn maps_openai_usage_fields() {
    let usage = map_openai_usage(&json!({
        "prompt_tokens": 11,
        "completion_tokens": 7,
        "total_tokens": 18,
        "completion_tokens_details": { "reasoning_tokens": 2 },
        "prompt_tokens_details": { "cached_tokens": 3 }
    }));

    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.total_tokens, Some(18));
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert_eq!(usage.cached_input_tokens, Some(3));
}

#[test]
fn maps_chat_request_to_openai_wire_request() {
    let model = OpenAiCompatibleChatModel::new(OpenAiCompatibleConfig {
        provider_name: "openai",
        base_url: "https://api.openai.com/v1".to_string(),
        api_key_env: "OPENAI_API_KEY",
        default_headers: Default::default(),
        model_info: ModelInfo::new("openai", "gpt-4.1")
            .with_capabilities(ModelCapabilities {
                streaming: true,
                native_tools: true,
                vision: true,
                json_mode: true,
                reasoning: true,
                usage: true,
            })
            .with_limits(ModelLimits {
                max_input_tokens: Some(128000),
                max_output_tokens: Some(16384),
                max_total_tokens: Some(128000),
            }),
        use_responses_api: false,
    });

    let req = ChatRequest {
        messages: vec![Message::new(
            Role::User,
            vec![
                ContentPart::Text("hello".to_string()),
                ContentPart::ImageUrl {
                    url: "https://example.com/image.png".to_string(),
                },
            ],
        )],
        tools: vec![ToolSpec {
            name: "lookup".to_string(),
            description: Some("lookup docs".to_string()),
            input_schema: json!({"type": "object"}),
        }],
        options: GenerateOptions {
            temperature: Some(0.3),
            top_p: Some(0.9),
            max_output_tokens: Some(512),
            stop_sequences: vec!["STOP".to_string()],
            json_schema: Some(json!({"name": "answer"})),
            tool_choice: Some(ToolChoice::Named("lookup".to_string())),
            n: None,
            presence_penalty: None,
            frequency_penalty: None,
            provider_options: Default::default(),
        },
    };

    let wire = model.to_wire_request(&req).unwrap();
    assert_eq!(wire.model, "gpt-4.1");
    assert_eq!(wire.tools.len(), 1);
    assert_eq!(wire.stop, vec!["STOP"]);
    assert_eq!(wire.max_tokens, Some(512));
    assert_eq!(
        map_openai_finish_reason(Some("tool_calls")),
        FinishReason::ToolCalls
    );
}

#[test]
fn normalizes_openai_generate_response() {
    let response = map_openai_generate_response(OpenAiGenerateResponse {
        id: Some("resp_123".to_string()),
        choices: vec![OpenAiChoice {
            message: OpenAiRequestMessage {
                role: "assistant".to_string(),
                content: Some(json!([
                    { "type": "text", "text": "hello" }
                ])),
                reasoning_content: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": { "q": "rust" }
                    }
                })]),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: Some(json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14
        })),
    })
    .unwrap();

    assert_eq!(response.provider_response_id.as_deref(), Some("resp_123"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage.as_ref().and_then(|u| u.total_tokens),
        Some(14)
    );
    assert!(response.provider_metadata.contains_key("raw_usage"));
    assert_eq!(response.message.parts.len(), 2);
}

#[test]
fn normalizes_openai_stream_chunks() {
    let events = normalize_openai_stream_chunk(OpenAiStreamChunk {
        choices: vec![OpenAiStreamChunkChoice {
            delta: OpenAiStreamChunkChoiceDelta {
                content: Some(json!({ "type": "text", "text": "hel" })),
                reasoning_content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"q\":"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: Some(json!({ "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 })),
    })
    .unwrap();

    assert!(matches!(&events[0], oct::core::StreamEvent::Usage(_)));
    assert!(matches!(&events[1], oct::core::StreamEvent::TextDelta(text) if text == "hel"));
    assert!(matches!(
        &events[2],
        oct::core::StreamEvent::ToolCallDelta { .. }
    ));
    assert!(matches!(&events[3], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
    assert!(matches!(
        &events[4],
        oct::core::StreamEvent::Finish(FinishReason::ToolCalls)
    ));
}

#[test]
fn parses_openai_sse_done_event() {
    // [DONE] is now a terminator that emits no events
    // The actual Finish event comes from the chunk with finish_reason
    let events = parse_openai_sse_event("data: [DONE]").unwrap();
    assert!(events.is_empty());
}

#[test]
fn emits_final_openai_tool_call_when_done() {
    let events = normalize_openai_stream_chunk(OpenAiStreamChunk {
        choices: vec![OpenAiStreamChunkChoice {
            delta: OpenAiStreamChunkChoiceDelta {
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"q\":\"rust\"}"
                    },
                    "done": true
                })]),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: None,
    })
    .unwrap();

    assert!(matches!(
        &events[0],
        oct::core::StreamEvent::ToolCallDelta { .. }
    ));
    assert!(matches!(&events[1], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
}

#[test]
fn parses_openai_generate_body_text() {
    let response = parse_openai_generate_body(
        r#"{
            "id": "resp_realistic",
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": [
                            { "type": "text", "text": "hi there" }
                        ]
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3,
                "total_tokens": 15
            }
        }"#,
    )
    .unwrap();

    assert_eq!(
        response.provider_response_id.as_deref(),
        Some("resp_realistic")
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage.as_ref().and_then(|u| u.total_tokens),
        Some(15)
    );
}

#[test]
fn parses_openai_sse_transcript_across_events() {
    let events = parse_openai_sse_transcript(
        "data: {\"choices\":[{\"delta\":{\"content\":{\"type\":\"text\",\"text\":\"hel\"}},\"finish_reason\":null}],\"usage\":null}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"},\"done\":true}],\"content\":null},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
data: [DONE]\n\n",
    )
    .unwrap();

    assert!(matches!(&events[0], oct::core::StreamEvent::TextDelta(text) if text == "hel"));
    assert!(matches!(&events[1], oct::core::StreamEvent::Usage(_)));
    assert!(matches!(
        &events[2],
        oct::core::StreamEvent::ToolCallDelta { .. }
    ));
    assert!(matches!(&events[3], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
}
