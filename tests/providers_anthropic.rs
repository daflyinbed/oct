use serde_json::json;

use oct::provider::Provider;
use oct::providers::{
    anthropic_finish_reason, map_anthropic_response, map_anthropic_usage,
    normalize_anthropic_stream_event, parse_anthropic_generate_body, parse_anthropic_sse_event,
    parse_anthropic_sse_transcript, AnthropicProvider, AnthropicResponse, AnthropicStreamEvent,
};

#[test]
fn maps_anthropic_usage_fields() {
    let usage = map_anthropic_usage(&json!({
        "input_tokens": 20,
        "output_tokens": 9
    }));
    assert_eq!(usage.input_tokens, Some(20));
    assert_eq!(usage.output_tokens, Some(9));
    assert_eq!(usage.total_tokens, Some(29));
}

#[test]
fn returns_anthropic_model_with_effective_limits() {
    let provider = AnthropicProvider::default();
    let model = provider.chat_model("claude-sonnet-4").unwrap();
    assert_eq!(model.info().provider_name, "anthropic");
    assert_eq!(model.info().limits.max_input_tokens, Some(200_000));
    assert_eq!(
        anthropic_finish_reason(Some("tool_use")),
        oct::core::FinishReason::ToolCalls
    );
}

#[test]
fn normalizes_anthropic_response() {
    let response = map_anthropic_response(AnthropicResponse {
        id: Some("msg_123".to_string()),
        content: vec![
            json!({ "type": "text", "text": "hello" }),
            json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": { "q": "rust" }
            }),
        ],
        stop_reason: Some("tool_use".to_string()),
        usage: Some(json!({ "input_tokens": 15, "output_tokens": 6 })),
    })
    .unwrap();

    assert_eq!(response.provider_response_id.as_deref(), Some("msg_123"));
    assert_eq!(response.finish_reason, oct::core::FinishReason::ToolCalls);
    assert_eq!(
        response.usage.as_ref().and_then(|u| u.total_tokens),
        Some(21)
    );
    assert!(response.provider_metadata.contains_key("raw_usage"));
    assert_eq!(response.message.parts.len(), 2);
}

#[test]
fn normalizes_anthropic_stream_events() {
    let events = normalize_anthropic_stream_event(AnthropicStreamEvent {
        event_type: "content_block_delta".to_string(),
        delta: Some(json!({
            "type": "text_delta",
            "text": "hello"
        })),
        content_block: None,
        usage: Some(json!({ "input_tokens": 10, "output_tokens": 1 })),
    })
    .unwrap();

    assert!(matches!(&events[0], oct::core::StreamEvent::Usage(_)));
    assert!(matches!(&events[1], oct::core::StreamEvent::TextDelta(text) if text == "hello"));
}

#[test]
fn parses_anthropic_sse_message_stop() {
    let events =
        parse_anthropic_sse_event("event: message_stop\ndata: {\"type\":\"message_stop\"}")
            .unwrap();
    assert_eq!(
        events,
        vec![oct::core::StreamEvent::Finish(
            oct::core::FinishReason::Stop
        )]
    );
}

#[test]
fn emits_final_anthropic_tool_call_on_block_stop() {
    let start = normalize_anthropic_stream_event(AnthropicStreamEvent {
        event_type: "content_block_start".to_string(),
        delta: None,
        content_block: Some(json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "lookup",
            "input": {}
        })),
        usage: None,
    })
    .unwrap();

    assert!(matches!(&start[0], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
}

#[test]
fn parses_anthropic_generate_body_text() {
    let response = parse_anthropic_generate_body(
        r#"{
            "id": "msg_realistic",
            "content": [
                { "type": "text", "text": "hello from claude" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 14,
                "output_tokens": 5
            }
        }"#,
    )
    .unwrap();

    assert_eq!(
        response.provider_response_id.as_deref(),
        Some("msg_realistic")
    );
    assert_eq!(response.finish_reason, oct::core::FinishReason::Stop);
    assert_eq!(
        response.usage.as_ref().and_then(|u| u.total_tokens),
        Some(19)
    );
}

#[test]
fn parses_anthropic_sse_transcript_across_events() {
    let events = parse_anthropic_sse_transcript(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"lookup\",\"input\":{}}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"id\":\"toolu_1\",\"partial_json\":\"{\\\"q\\\":\\\"rust\\\"}\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
    .unwrap();

    assert!(matches!(&events[0], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
    assert!(matches!(&events[1], oct::core::StreamEvent::Usage(_)));
    assert!(matches!(
        &events[2],
        oct::core::StreamEvent::ToolCallDelta { .. }
    ));
    assert!(matches!(&events[3], oct::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
    assert!(matches!(
        &events[4],
        oct::core::StreamEvent::Finish(oct::core::FinishReason::Stop)
    ));
}
