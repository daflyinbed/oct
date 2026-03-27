use oct::adapter::{
    map_openai_finish_reason, map_openai_generate_response, map_openai_usage,
    normalize_openai_stream_chunk, parse_openai_generate_body, parse_openai_sse_event,
    parse_openai_sse_transcript, ChatCompletionChoice, ChatCompletionChunk,
    ChatCompletionChunkChoice, ChatCompletionChunkDelta, ChatCompletionChunkDeltaToolCall,
    ChatCompletionChunkDeltaToolCallFunction, ChatCompletionMessageToolCall,
    ChatCompletionMessageToolCallFunction, ChatCompletionResponseMessage, ChatCompletionUsage,
    ChatCompletionUsageCompletionDetails, ChatCompletionUsagePromptDetails,
    OpenAiCompatibleChatModel, OpenAiCompatibleConfig,
};
use oct::core::{ContentPart, FinishReason, GenerateOptions, Message, Role, ToolChoice, ToolSpec};
use oct::model::ChatRequest;
use oct::provider::{ModelCapabilities, ModelInfo, ModelLimits};
use serde_json::json;

#[test]
fn maps_openai_usage_fields() {
    let usage = map_openai_usage(&ChatCompletionUsage {
        prompt_tokens: 11,
        completion_tokens: 7,
        total_tokens: 18,
        completion_tokens_details: Some(ChatCompletionUsageCompletionDetails {
            reasoning_tokens: Some(2),
            audio_tokens: None,
            accepted_prediction_tokens: None,
            rejected_prediction_tokens: None,
        }),
        prompt_tokens_details: Some(ChatCompletionUsagePromptDetails {
            cached_tokens: Some(3),
            audio_tokens: None,
        }),
    });

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
    use oct::adapter::ChatCompletionResponse;

    let response = map_openai_generate_response(ChatCompletionResponse {
        id: Some("resp_123".to_string()),
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionResponseMessage {
                role: "assistant".to_string(),
                content: Some("hello".to_string()),
                reasoning_content: None,
                reasoning: None,
                refusal: None,
                tool_calls: Some(vec![ChatCompletionMessageToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: ChatCompletionMessageToolCallFunction {
                        name: "lookup".to_string(),
                        arguments: r#"{"q": "rust"}"#.to_string(),
                    },
                }]),
                function_call: None,
            },
            finish_reason: Some("tool_calls".to_string()),
            logprobs: None,
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            completion_tokens_details: None,
            prompt_tokens_details: None,
        }),
        system_fingerprint: None,
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
    let events = normalize_openai_stream_chunk(ChatCompletionChunk {
        id: None,
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: Some("hel".to_string()),
                refusal: None,
                tool_calls: Some(vec![ChatCompletionChunkDeltaToolCall {
                    index: 0,
                    id: Some("call_1".to_string()),
                    kind: Some("function".to_string()),
                    function: Some(ChatCompletionChunkDeltaToolCallFunction {
                        name: Some("lookup".to_string()),
                        arguments: Some("{\"q\":\"test\"}".to_string()),
                    }),
                }]),
                function_call: None,
                reasoning_content: None,
                reasoning: None,
            },
            finish_reason: Some("tool_calls".to_string()),
            logprobs: None,
        }],
        usage: Some(ChatCompletionUsage {
            prompt_tokens: 5,
            completion_tokens: 2,
            total_tokens: 7,
            completion_tokens_details: None,
            prompt_tokens_details: None,
        }),
        system_fingerprint: None,
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
    let events = parse_openai_sse_event("data: [DONE]").unwrap();
    assert!(events.is_empty());
}

#[test]
fn emits_final_openai_tool_call_when_done() {
    let events = normalize_openai_stream_chunk(ChatCompletionChunk {
        id: None,
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: None,
                refusal: None,
                tool_calls: Some(vec![ChatCompletionChunkDeltaToolCall {
                    index: 0,
                    id: Some("call_1".to_string()),
                    kind: Some("function".to_string()),
                    function: Some(ChatCompletionChunkDeltaToolCallFunction {
                        name: Some("lookup".to_string()),
                        arguments: Some("{\"q\":\"rust\"}".to_string()),
                    }),
                }]),
                function_call: None,
                reasoning_content: None,
                reasoning: None,
            },
            finish_reason: Some("tool_calls".to_string()),
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
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
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "hi there"
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
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}],\"usage\":null}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}],\"content\":null},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
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

#[test]
fn parses_reasoning_content_in_response() {
    use oct::adapter::ChatCompletionResponse;

    let response = map_openai_generate_response(ChatCompletionResponse {
        id: Some("resp_123".to_string()),
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionResponseMessage {
                role: "assistant".to_string(),
                content: Some("The answer is 42.".to_string()),
                reasoning_content: Some("Let me think about this...".to_string()),
                reasoning: None,
                refusal: None,
                tool_calls: None,
                function_call: None,
            },
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
    })
    .unwrap();

    assert_eq!(response.message.parts.len(), 2);
    assert!(
        matches!(&response.message.parts[0], ContentPart::Reasoning(t) if t == "Let me think about this...")
    );
    assert!(matches!(&response.message.parts[1], ContentPart::Text(t) if t == "The answer is 42."));
}

#[test]
fn parses_reasoning_field_as_fallback_in_response() {
    use oct::adapter::ChatCompletionResponse;

    let response = map_openai_generate_response(ChatCompletionResponse {
        id: Some("resp_123".to_string()),
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionResponseMessage {
                role: "assistant".to_string(),
                content: Some("The answer is 42.".to_string()),
                reasoning_content: None,
                reasoning: Some("Using the reasoning field...".to_string()),
                refusal: None,
                tool_calls: None,
                function_call: None,
            },
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
    })
    .unwrap();

    assert_eq!(response.message.parts.len(), 2);
    assert!(
        matches!(&response.message.parts[0], ContentPart::Reasoning(t) if t == "Using the reasoning field...")
    );
    assert!(matches!(&response.message.parts[1], ContentPart::Text(t) if t == "The answer is 42."));
}

#[test]
fn prefers_reasoning_content_over_reasoning() {
    use oct::adapter::ChatCompletionResponse;

    let response = map_openai_generate_response(ChatCompletionResponse {
        id: Some("resp_123".to_string()),
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChoice {
            index: 0,
            message: ChatCompletionResponseMessage {
                role: "assistant".to_string(),
                content: Some("The answer is 42.".to_string()),
                reasoning_content: Some("Primary reasoning".to_string()),
                reasoning: Some("Fallback reasoning".to_string()),
                refusal: None,
                tool_calls: None,
                function_call: None,
            },
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
    })
    .unwrap();

    assert_eq!(response.message.parts.len(), 2);
    assert!(
        matches!(&response.message.parts[0], ContentPart::Reasoning(t) if t == "Primary reasoning")
    );
}

#[test]
fn parses_reasoning_in_stream_delta() {
    let events = normalize_openai_stream_chunk(ChatCompletionChunk {
        id: None,
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: None,
                refusal: None,
                tool_calls: None,
                function_call: None,
                reasoning_content: Some("thinking...".to_string()),
                reasoning: None,
            },
            finish_reason: None,
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
    })
    .unwrap();

    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], oct::core::StreamEvent::ReasoningDelta(t) if t == "thinking..."));
}

#[test]
fn parses_reasoning_field_as_fallback_in_stream_delta() {
    let events = normalize_openai_stream_chunk(ChatCompletionChunk {
        id: None,
        object: None,
        created: None,
        model: None,
        choices: vec![ChatCompletionChunkChoice {
            index: 0,
            delta: ChatCompletionChunkDelta {
                role: None,
                content: None,
                refusal: None,
                tool_calls: None,
                function_call: None,
                reasoning_content: None,
                reasoning: Some("fallback thinking...".to_string()),
            },
            finish_reason: None,
            logprobs: None,
        }],
        usage: None,
        system_fingerprint: None,
    })
    .unwrap();

    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], oct::core::StreamEvent::ReasoningDelta(t) if t == "fallback thinking...")
    );
}

#[test]
fn maps_reasoning_to_reasoning_content_in_request() {
    let model = OpenAiCompatibleChatModel::new(OpenAiCompatibleConfig {
        provider_name: "moonshot",
        base_url: "https://api.moonshot.ai/v1".to_string(),
        api_key_env: "MOONSHOT_API_KEY",
        default_headers: Default::default(),
        model_info: ModelInfo::new("moonshot", "kimi-k2.5")
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
        messages: vec![
            Message::new(
                Role::User,
                vec![ContentPart::Text("What is 2+2?".to_string())],
            ),
            Message::new(
                Role::Assistant,
                vec![
                    ContentPart::Reasoning("I need to add 2 and 2.".to_string()),
                    ContentPart::Text("The answer is 4.".to_string()),
                ],
            ),
        ],
        tools: vec![],
        options: GenerateOptions::default(),
    };

    let wire = model.to_wire_request(&req).unwrap();
    assert_eq!(wire.messages.len(), 2);

    let assistant_msg = &wire.messages[1];
    assert_eq!(assistant_msg.role, "assistant");
    assert_eq!(
        assistant_msg.reasoning_content,
        Some("I need to add 2 and 2.".to_string())
    );
    assert!(
        matches!(&assistant_msg.content, Some(oct::adapter::RequestMessageContentValue::String(s)) if s == "The answer is 4.")
    );
}
