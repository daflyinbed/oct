//! Wire-format tests over real HTTP: a local mock server (see `http_mock`)
//! drives the actual reqwest + SSE byte-stream decode path, with response
//! bodies split at deliberately hostile chunk boundaries (mid multi-byte
//! UTF-8, mid SSE event, mid data-line) and with error statuses exercised
//! against the real status-mapping code.

mod http_mock;

use futures_util::StreamExt;
use http_mock::{MockResponse, spawn_server};
use oct_llm_provider::adapter::{OpenAiCompatibleChatModel, OpenAiCompatibleConfig};
use oct_llm_provider::core::{
    ContentPart, FinishReason, GenerateOptions, Message, ModelError, Role, StreamEvent, ToolSpec,
};
use oct_llm_provider::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use oct_llm_provider::provider::{ModelInfo, Provider};
use oct_llm_provider::providers::AnthropicProvider;

fn chat_request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::text(Role::User, "hello")],
        tools: vec![ToolSpec {
            name: "demo".to_string(),
            description: Some("demo tool".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        options: GenerateOptions::default(),
    }
}

/// Drain a stream, keeping items as Results so mid-stream errors can be
/// asserted on.
async fn collect(stream: ChatStream) -> Vec<Result<StreamEvent, ModelError>> {
    stream.collect().await
}

fn text_of(events: &[Result<StreamEvent, ModelError>]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::TextDelta(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

// --- OpenAI-compatible ----------------------------------------------------

fn openai_config(base_url: String, api_key_env: &str) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        provider_name: "mock-openai".to_string(),
        base_url,
        api_key_env: api_key_env.to_string(),
        default_headers: Default::default(),
        model_info: ModelInfo::new("mock-openai", "test-model"),
        use_responses_api: false,
    }
}

#[tokio::test]
async fn openai_stream_reassembles_events_split_at_hostile_chunk_boundaries() {
    let ev1 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"}}]}\n\n";
    let ev2 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好🌍\"}}]}\n\n";
    let ev3 = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let ev4 = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5,\"total_tokens\":8}}\n\n";
    let done = "data: [DONE]\n\n";
    let raw = format!("{ev1}{ev2}{ev3}{ev4}{done}");

    // Cut inside the 4-byte emoji, in the middle of the "\n\n" separator
    // between two events, and right after the "data:" prefix of ev4.
    let emoji_mid = raw.find("🌍").unwrap() + 2;
    let separator_mid = raw.find(ev3).unwrap() - 1;
    let after_data_prefix = raw.find(ev4).unwrap() + "data:".len();

    let server = spawn_server(vec![MockResponse::sse_split(
        &raw,
        &[emoji_mid, separator_mid, after_data_prefix],
    )])
    .await;
    let model = OpenAiCompatibleChatModel::new_with_api_key(
        openai_config(server.url(), "OCT_TEST_OPENAI_KEY_UNUSED"),
        "test-key".to_string(),
        reqwest::Client::new(),
    );

    let events = collect(model.stream(chat_request()).await.unwrap()).await;

    assert_eq!(text_of(&events), "你好🌍");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Finish(FinishReason::Stop))))
    );
    let usage: Vec<&oct_llm_provider::core::Usage> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::Usage(u)) => Some(u),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].input_tokens, Some(3));
    assert_eq!(usage[0].output_tokens, Some(5));

    // Request side: endpoint, auth header, and streaming request shape.
    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].method, "POST");
    assert_eq!(captured[0].path, "/chat/completions");
    assert_eq!(captured[0].header("authorization"), Some("Bearer test-key"));
    let body = captured[0].body_json();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["model"], "test-model");
    assert!(body["messages"][0]["content"].to_string().contains("hello"));
    assert_eq!(body["tools"][0]["function"]["name"], "demo");
}

#[tokio::test]
async fn openai_maps_http_error_statuses_to_model_errors() {
    let server = spawn_server(vec![
        MockResponse::json(401, "{\"error\":\"bad key\"}"),
        MockResponse::json(429, "{\"error\":\"slow down\"}"),
        MockResponse::json(500, "{\"error\":\"kaboom\"}"),
    ])
    .await;
    let model = OpenAiCompatibleChatModel::new_with_api_key(
        openai_config(server.url(), "OCT_TEST_OPENAI_KEY_UNUSED"),
        "test-key".to_string(),
        reqwest::Client::new(),
    );

    assert!(matches!(
        model.stream(chat_request()).await.err().unwrap(),
        ModelError::Authentication
    ));
    assert!(matches!(
        model.stream(chat_request()).await.err().unwrap(),
        ModelError::RateLimited
    ));
    match model.stream(chat_request()).await.err().unwrap() {
        ModelError::Provider { message } => {
            assert!(message.contains("500"), "got: {message}");
            assert!(message.contains("kaboom"), "got: {message}");
        }
        other => panic!("expected Provider error, got {other:?}"),
    }
    assert_eq!(server.captured().len(), 3);
}

#[tokio::test]
async fn openai_fails_with_authentication_error_when_no_api_key_available() {
    // No explicit key and an env var nothing ever sets: must fail before
    // any HTTP traffic.
    let model = OpenAiCompatibleChatModel::new(openai_config(
        "http://127.0.0.1:1".to_string(),
        "OCT_TEST_OPENAI_KEY_NEVER_SET",
    ));
    assert!(matches!(
        model.stream(chat_request()).await.err().unwrap(),
        ModelError::Authentication
    ));
}

#[tokio::test]
async fn openai_generate_maps_json_response_and_captures_request_shape() {
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 123,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello there"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, body)]).await;
    let model = OpenAiCompatibleChatModel::new_with_api_key(
        openai_config(server.url(), "OCT_TEST_OPENAI_KEY_UNUSED"),
        "test-key".to_string(),
        reqwest::Client::new(),
    );

    let response: ChatResponse = model.generate(chat_request()).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.message.parts,
        vec![ContentPart::Text("hello there".to_string())]
    );
    let usage = response.usage.expect("usage should be present");
    assert_eq!(usage.input_tokens, Some(2));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(response.provider_response_id.as_deref(), Some("chatcmpl-1"));

    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    let body = captured[0].body_json();
    // `stream: false` is skipped on the wire (Not::not serializer guard).
    assert!(body["stream"].is_null());
    assert_eq!(body["model"], "test-model");
    assert!(body["messages"][0]["content"].to_string().contains("hello"));
}

// --- Anthropic -------------------------------------------------------------

fn anthropic_model(
    base_url: String,
    api_key_env: &'static str,
    key_value: &str,
) -> Box<dyn oct_llm_provider::model::ChatModel> {
    // SAFETY: each test uses an env var name unique to that test (and never
    // read anywhere else), so parallel tests cannot race on the value.
    unsafe { std::env::set_var(api_key_env, key_value) };
    AnthropicProvider::new(base_url, api_key_env)
        .chat_model("claude-test")
        .unwrap()
}

#[tokio::test]
async fn anthropic_stream_parses_tool_use_across_hostile_chunk_boundaries() {
    let raw = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-test\",\"content\":[],\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好🌍\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"杭州\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    // Cut inside the emoji, between the "event:" and "data:" lines of a
    // later event, and inside the first partial_json payload (mid-JSON).
    let emoji_mid = raw.find("🌍").unwrap() + 2;
    let between_lines =
        raw.find("event: message_delta").unwrap() + "event: message_delta".len() + 1;
    let inside_partial_json = raw.find("city").unwrap() + 1;

    let server = spawn_server(vec![MockResponse::sse_split(
        raw,
        &[emoji_mid, inside_partial_json, between_lines],
    )])
    .await;
    let model = anthropic_model(
        server.url(),
        "OCT_TEST_ANTHROPIC_KEY_STREAM",
        "sk-test-stream",
    );

    let events = collect(model.stream(chat_request()).await.unwrap()).await;

    assert_eq!(text_of(&events), "你好🌍");

    // Streaming tool-call arguments arrive as deltas and are also
    // accumulated into a complete ToolCall at content_block_stop.
    let mut streamed_args = String::new();
    for event in &events {
        if let Ok(StreamEvent::ToolCallDelta {
            call_id,
            arguments_delta,
            ..
        }) = event
        {
            assert_eq!(call_id, "toolu_1");
            streamed_args.push_str(arguments_delta);
        }
    }
    assert_eq!(streamed_args, r#"{"city":"杭州"}"#);

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::ToolCall(tc)) => Some(tc.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "toolu_1");
    assert_eq!(tool_calls[0].name, "get_weather");
    assert_eq!(tool_calls[0].arguments, r#"{"city":"杭州"}"#);

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Finish(FinishReason::ToolCalls))))
    );

    // message_start carries the input-side usage, message_delta the final
    // output-side usage.
    let usages: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::Usage(u)) => Some(u.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 2);
    assert_eq!(usages[0].input_tokens, Some(4));
    assert_eq!(usages[1].output_tokens, Some(12));

    let captured = server.captured();
    assert_eq!(captured[0].path, "/messages");
    assert_eq!(captured[0].header("x-api-key"), Some("sk-test-stream"));
    assert_eq!(captured[0].header("anthropic-version"), Some("2023-06-01"));
    let body = captured[0].body_json();
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["stream"], true);
}

#[tokio::test]
async fn anthropic_maps_http_error_statuses_to_model_errors() {
    let server = spawn_server(vec![
        MockResponse::json(401, "{\"error\":\"bad key\"}"),
        MockResponse::json(429, "{\"error\":\"slow down\"}"),
        MockResponse::json(500, "{\"error\":\"kaboom\"}"),
    ])
    .await;
    let model = anthropic_model(
        server.url(),
        "OCT_TEST_ANTHROPIC_KEY_ERRORS",
        "sk-test-errors",
    );

    assert!(matches!(
        model.stream(chat_request()).await.err().unwrap(),
        ModelError::Authentication
    ));
    assert!(matches!(
        model.stream(chat_request()).await.err().unwrap(),
        ModelError::RateLimited
    ));
    match model.stream(chat_request()).await.err().unwrap() {
        ModelError::Provider { message } => {
            assert!(message.contains("500"), "got: {message}");
            assert!(message.contains("kaboom"), "got: {message}");
        }
        other => panic!("expected Provider error, got {other:?}"),
    }
    assert_eq!(server.captured().len(), 3);
}

#[tokio::test]
async fn anthropic_generate_extracts_system_prompt_and_maps_response() {
    let body = serde_json::json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "hi there"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 6, "output_tokens": 2}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, body)]).await;
    let model = anthropic_model(
        server.url(),
        "OCT_TEST_ANTHROPIC_KEY_GENERATE",
        "sk-test-generate",
    );

    let request = ChatRequest {
        messages: vec![
            Message::text(Role::System, "sys prompt"),
            Message::text(Role::User, "hello"),
        ],
        tools: vec![],
        options: GenerateOptions::default(),
    };
    let response = model.generate(request).await.unwrap();

    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.message.parts,
        vec![ContentPart::Text("hi there".to_string())]
    );
    assert_eq!(response.usage.as_ref().unwrap().input_tokens, Some(6));

    let captured = server.captured();
    assert_eq!(captured[0].header("x-api-key"), Some("sk-test-generate"));
    let body = captured[0].body_json();
    // System messages are hoisted out of `messages` into the top-level
    // `system` field on the Anthropic wire format.
    assert_eq!(body["system"], "sys prompt");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert!(body["stream"].is_null());
}
