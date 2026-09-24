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

/// CRLF-framed events (plus one mixed `\r\n\n` separator) and a final event
/// that never receives its blank line before EOF — the shared framing must
/// deliver it as the EOF tail frame.
#[tokio::test]
async fn openai_stream_accepts_crlf_framing_and_eof_tail_frame() {
    let raw = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\r\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n",
    );
    // One cut after the "data:" prefix of the second event.
    let cut = raw.find("llo").unwrap() - 2;
    let server = spawn_server(vec![MockResponse::sse_split(raw, &[cut])]).await;
    let model = OpenAiCompatibleChatModel::new_with_api_key(
        openai_config(server.url(), "OCT_TEST_OPENAI_KEY_UNUSED"),
        "test-key".to_string(),
        reqwest::Client::new(),
    );

    let events = collect(model.stream(chat_request()).await.unwrap()).await;

    assert_eq!(text_of(&events), "hello");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Finish(FinishReason::Stop))))
    );
}

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
async fn anthropic_stream_accepts_crlf_framing_and_eof_tail_frame() {
    let raw = concat!(
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"he\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\r\n\n",
        "event: message_stop\r\n",
        "data: {\"type\":\"message_stop\"}\r\n",
    );
    // One cut between the "event:" and "data:" lines of the second event.
    let cut = raw.find("event: message_stop").unwrap() - 1;
    let server = spawn_server(vec![MockResponse::sse_split(raw, &[cut])]).await;
    let model = anthropic_model(
        server.url(),
        "OCT_TEST_ANTHROPIC_KEY_CRLF",
        "sk-test-crlf",
    );

    let events = collect(model.stream(chat_request()).await.unwrap()).await;

    assert_eq!(text_of(&events), "hello");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Finish(FinishReason::Stop))))
    );
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

#[tokio::test]
async fn anthropic_tool_result_images_ride_the_wire_as_base64_blocks() {
    let body = serde_json::json!({
        "id": "msg_images",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "got it"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 9, "output_tokens": 3}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, body)]).await;
    let model = anthropic_model(
        server.url(),
        "OCT_TEST_ANTHROPIC_KEY_IMAGES",
        "sk-test-images",
    );

    let image = |mime: &str, data: &str| oct_llm_provider::core::ToolResultImage {
        mime_type: mime.to_string(),
        data: data.to_string(),
    };
    let tool_result = |call_id: &str, text: &str, is_error: bool, images| {
        Message::new(
            Role::Tool,
            vec![ContentPart::ToolResult(
                oct_llm_provider::core::ToolResult {
                    call_id: call_id.to_string(),
                    content: serde_json::Value::String(text.to_string()),
                    is_error,
                    images,
                },
            )],
        )
    };
    let request = ChatRequest {
        messages: vec![
            Message::new(
                Role::Assistant,
                vec![ContentPart::ToolCall(oct_llm_provider::core::ToolCall {
                    id: "toolu_1".to_string(),
                    name: "chart".to_string(),
                    arguments: "{}".to_string(),
                })],
            ),
            // Text + two images: block array (text first, then base64
            // image blocks in order).
            tool_result(
                "toolu_1",
                "chart rendered",
                false,
                vec![
                    image("image/png", "aVBuZw=="),
                    image("image/jpeg", "aGVpZw=="),
                ],
            ),
            // Error result with an image keeps is_error AND the blocks.
            Message::new(
                Role::Assistant,
                vec![ContentPart::ToolCall(oct_llm_provider::core::ToolCall {
                    id: "toolu_2".to_string(),
                    name: "diff".to_string(),
                    arguments: "{}".to_string(),
                })],
            ),
            tool_result(
                "toolu_2",
                "diverged",
                true,
                vec![image("image/png", "ZGlmZg==")],
            ),
            // Image-only result: array with no text block, plus a data-URL
            // user image mapping to an inline base64 source.
            Message::new(
                Role::Assistant,
                vec![ContentPart::ToolCall(oct_llm_provider::core::ToolCall {
                    id: "toolu_3".to_string(),
                    name: "shot".to_string(),
                    arguments: "{}".to_string(),
                })],
            ),
            tool_result("toolu_3", "", false, vec![image("image/png", "c2hvdA==")]),
            Message::new(
                Role::User,
                vec![ContentPart::ImageUrl {
                    url: "data:image/gif;base64,R0lGODlh".to_string(),
                }],
            ),
            // Text-only result: stays a plain string (pre-images shape).
            Message::new(
                Role::Assistant,
                vec![ContentPart::ToolCall(oct_llm_provider::core::ToolCall {
                    id: "toolu_4".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                })],
            ),
            tool_result("toolu_4", "plain text result", false, vec![]),
        ],
        tools: vec![],
        options: GenerateOptions::default(),
    };
    model.generate(request).await.unwrap();

    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    let body = captured[0].body_json();

    // toolu_1: [text, image, image] block array (messages alternate
    // assistant tool_use / user tool_result, Tool mapping to user).
    let block = &body["messages"][1]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "toolu_1");
    assert_eq!(block["content"][0]["type"], "text");
    assert_eq!(block["content"][0]["text"], "chart rendered");
    assert_eq!(block["content"][1]["type"], "image");
    assert_eq!(block["content"][1]["source"]["type"], "base64");
    assert_eq!(block["content"][1]["source"]["media_type"], "image/png");
    assert_eq!(block["content"][1]["source"]["data"], "aVBuZw==");
    assert_eq!(block["content"][2]["source"]["media_type"], "image/jpeg");

    // toolu_2: is_error flag survives alongside the image block.
    let block = &body["messages"][3]["content"][0];
    assert_eq!(block["is_error"], true);
    assert_eq!(block["content"][0]["text"], "diverged");
    assert_eq!(block["content"][1]["source"]["data"], "ZGlmZg==");

    // toolu_3: image-only — the array starts at the image block, no text.
    let block = &body["messages"][5]["content"][0];
    assert_eq!(block["content"].as_array().unwrap().len(), 1);
    assert_eq!(block["content"][0]["type"], "image");
    assert_eq!(block["content"][0]["source"]["data"], "c2hvdA==");

    // Data-URL user image becomes an inline base64 source (not a URL).
    let image_block = &body["messages"][6]["content"][0];
    assert_eq!(image_block["type"], "image");
    assert_eq!(image_block["source"]["type"], "base64");
    assert_eq!(image_block["source"]["media_type"], "image/gif");
    assert_eq!(image_block["source"]["data"], "R0lGODlh");

    // toolu_4: text-only result stays a plain string, no array.
    let block = &body["messages"][8]["content"][0];
    assert_eq!(block["content"], "plain text result");
}
