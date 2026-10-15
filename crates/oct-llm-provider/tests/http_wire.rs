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

fn openai_config(base_url: String, api_key: Option<&str>) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        provider_name: "mock-openai".to_string(),
        base_url,
        api_key: api_key.map(str::to_string),
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
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
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
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
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
    // No materialized key anywhere: must fail before any HTTP traffic.
    let model =
        OpenAiCompatibleChatModel::new(openai_config("http://127.0.0.1:1".to_string(), None));
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
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
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

// --- User-Agent ------------------------------------------------------------

/// Identity contract: every LLM request carries the explicit `oct/{version}`
/// User-Agent (opencode-style bare `product/version` token); a
/// caller-configured `User-Agent` default header wins over it.
#[tokio::test]
async fn llm_requests_carry_oct_user_agent_unless_explicitly_configured() {
    let openai_body = serde_json::json!({
        "id": "chatcmpl-ua",
        "object": "chat.completion",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    })
    .to_string();

    // OpenAI-compatible default: the adapter's own identity.
    let server = spawn_server(vec![MockResponse::json(200, openai_body.clone())]).await;
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
        reqwest::Client::new(),
    );
    model.generate(chat_request()).await.unwrap();
    assert_eq!(
        server.captured()[0].header("user-agent"),
        // Same package, so the test's CARGO_PKG_VERSION is the library's.
        Some(concat!("oct/", env!("CARGO_PKG_VERSION"))),
    );

    // A caller-configured `User-Agent` default header wins over the
    // automatic identity.
    let server = spawn_server(vec![MockResponse::json(200, openai_body)]).await;
    let mut config = openai_config(server.url(), Some("test-key"));
    config.default_headers.insert(
        "User-Agent".to_string(),
        serde_json::Value::String("custom-agent/9".to_string()),
    );
    let model = OpenAiCompatibleChatModel::new_with_client(config, reqwest::Client::new());
    model.generate(chat_request()).await.unwrap();
    assert_eq!(
        server.captured()[0].header("user-agent"),
        Some("custom-agent/9")
    );

    // The Anthropic path rides the same identity, alongside
    // anthropic-version.
    let anthropic_body = serde_json::json!({
        "id": "msg_ua",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, anthropic_body)]).await;
    anthropic_model(server.url(), "test-key")
        .generate(chat_request())
        .await
        .unwrap();
    let captured = &server.captured()[0];
    assert_eq!(
        captured.header("user-agent"),
        Some(concat!("oct/", env!("CARGO_PKG_VERSION"))),
    );
    assert_eq!(captured.header("anthropic-version"), Some("2023-06-01"));
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
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
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
    key_value: &str,
) -> Box<dyn oct_llm_provider::model::ChatModel> {
    // The key is explicit connection config — no environment involved, so
    // parallel tests cannot race on it.
    AnthropicProvider::new(base_url, Some(key_value.to_string()))
        .chat_model("claude-test")
        .unwrap()
}

#[tokio::test]
async fn anthropic_reasoning_provenance_survives_reload_and_filters_edited_prefixes() {
    let content = serde_json::json!([
        {"type":"thinking", "thinking":"summary", "signature":"signed-prefix"},
        {"type":"redacted_thinking", "data":"redacted-prefix"},
        {"type":"text", "text":"answer"},
    ]);
    let response_body = |content: serde_json::Value| {
        serde_json::json!({
            "id":"msg-bound", "type":"message", "role":"assistant", "model":"claude-opus-5-5",
            "content":content, "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1},
        }).to_string()
    };
    let transcript = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"summary\",\"signature\":\"signed-prefix\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"redacted-prefix\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"answer\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    for streaming in [false, true] {
        let first = if streaming {
            MockResponse::sse_split(transcript, &[])
        } else {
            MockResponse::json(200, response_body(content.clone()))
        };
        let mut responses = vec![first];
        responses.extend((0..4).map(|_| {
            MockResponse::json(
                200,
                response_body(serde_json::json!([{"type":"text", "text":"ok"}])),
            )
        }));
        let server = spawn_server(responses).await;
        let provider = AnthropicProvider::new(server.url(), Some("test-key".into()));
        let model = provider.chat_model("claude-opus-5-5").unwrap();
        let mut request = chat_request();
        request
            .messages
            .insert(0, Message::text(Role::System, "original instructions"));
        let assistant = if streaming {
            let events = collect(model.stream(request.clone()).await.unwrap()).await;
            let parts = events
                .into_iter()
                .filter_map(|event| match event.unwrap() {
                    StreamEvent::ReasoningBlock(block) => Some(ContentPart::ReasoningBlock(block)),
                    StreamEvent::TextDelta(text) => Some(ContentPart::Text(text)),
                    _ => None,
                })
                .collect();
            Message::new(Role::Assistant, parts)
        } else {
            model.generate(request.clone()).await.unwrap().message
        };
        let stored = serde_json::to_string(&assistant).unwrap();
        assert!(stored.contains("prefix_fingerprint"));
        // A new model instance and a deserialized message stand in for a
        // process restart; replay cannot depend on an in-memory adapter cache.
        let assistant = serde_json::from_str(&stored).unwrap();
        let model = provider.chat_model("claude-opus-5-5").unwrap();
        request.messages.push(assistant);
        request.messages.push(Message::text(Role::User, "continue"));
        model.generate(request.clone()).await.unwrap();
        assert_eq!(
            server.captured()[1].body_json()["messages"][1]["content"],
            content
        );

        for edit in 0..3 {
            let mut edited = request.clone();
            match edit {
                0 => edited.messages[0] = Message::text(Role::System, "updated instructions"),
                1 => edited.tools[0].description = Some("updated tool".into()),
                _ => edited.messages[1] = Message::text(Role::User, "compacted history"),
            }
            model.generate(edited).await.unwrap();
            let body = server.captured().last().unwrap().body_json();
            assert_eq!(
                body["messages"][1]["content"],
                serde_json::json!([content[2]])
            );
            assert!(!body.to_string().contains("prefix_fingerprint"));
            // The default thinking setting is preserved, including models
            // whose default is already adaptive; replay adds no beta fields.
            assert!(body.get("thinking").is_none());
        }
    }
}

/// The DB-driven resolution path: a `ConnectionConfig` built from provider
/// row fields must override the family's own defaults outright — requests
/// land at the row's base URL carrying the row's key, never at the default
/// endpoint with the default key.
#[tokio::test]
async fn anthropic_connection_overrides_win_over_family_defaults() {
    let body = serde_json::json!({
        "id": "msg_override",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "override ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 1}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, body)]).await;

    let mut registry = oct_llm_provider::provider::ProviderRegistry::new();
    registry.register(Box::new(AnthropicProvider::new(
        // Defaults nobody wants used: an unreachable endpoint and a decoy key.
        "http://127.0.0.1:1",
        Some("sk-default-key".to_string()),
    )));

    let model = registry
        .resolve_chat_model_with_connection(
            "anthropic",
            "claude-test",
            &oct_llm_provider::provider::ConnectionConfig::from_row_fields(
                Some(server.url().as_str()),
                "sk-row-key",
            ),
        )
        .unwrap();
    let response = model.generate(chat_request()).await.unwrap();
    assert_eq!(
        response.message.parts,
        vec![ContentPart::Text("override ok".to_string())]
    );

    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].path, "/messages");
    assert_eq!(captured[0].header("x-api-key"), Some("sk-row-key"));
}

/// Blank row fields mean "fall back": the family's own (env-materialized or
/// built-in) defaults carry the request instead.
#[tokio::test]
async fn anthropic_blank_connection_fields_fall_back_to_family_defaults() {
    let body = serde_json::json!({
        "id": "msg_fallback",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "fallback ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 1}
    })
    .to_string();
    let server = spawn_server(vec![MockResponse::json(200, body)]).await;

    let mut registry = oct_llm_provider::provider::ProviderRegistry::new();
    registry.register(Box::new(AnthropicProvider::new(
        server.url(),
        Some("sk-default-key".to_string()),
    )));

    let model = registry
        .resolve_chat_model_with_connection(
            "anthropic",
            "claude-test",
            &oct_llm_provider::provider::ConnectionConfig::from_row_fields(None, ""),
        )
        .unwrap();
    model.generate(chat_request()).await.unwrap();

    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].header("x-api-key"), Some("sk-default-key"));
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
    let model = anthropic_model(server.url(), "sk-test-stream");

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
    let model = anthropic_model(server.url(), "sk-test-crlf");

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
    let model = anthropic_model(server.url(), "sk-test-errors");

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
    let model = anthropic_model(server.url(), "sk-test-generate");

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
    let model = anthropic_model(server.url(), "sk-test-images");

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

// --- Wire log ---------------------------------------------------------------
//
// The sink + subscriber are process-global, so they install ONCE per test
// binary: the first wire test's temp dir wins and every later assertion
// filters by its own conversation id (files are shared, lines are not).

fn wire_sink_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("oct-provider-wire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = oct_llm_provider::wire::init(dir.clone(), true);
        use tracing_subscriber::prelude::*;
        let _ = tracing_subscriber::registry()
            .with(oct_llm_provider::wire::WireLogLayer)
            .try_init();
        let _ = control;
        dir
    })
    .clone()
}

/// Bounded poll for the writer thread (a plain std thread — no event to
/// await). The channel handoff is fast; this only guards scheduling.
fn wait_for_lines(
    conversation_id: &str,
    pred: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(raw) =
            std::fs::read_to_string(wire_sink_dir().join(format!("{conversation_id}.jsonl")))
        {
            let lines: Vec<serde_json::Value> = raw
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).expect("valid JSONL"))
                .collect();
            if pred(&lines) {
                return lines;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wire lines never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[tokio::test]
async fn wire_log_records_request_sse_events_and_reambles_byte_faithfully() {
    // Install the subscriber BEFORE any model call: events emitted before
    // it exists are dropped by tracing itself, not by the sink.
    wire_sink_dir();
    let ev1 =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"}}]}";
    let ev2 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好🌍\"}}]}";
    let ev3 = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";
    let ev4 = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5,\"total_tokens\":8}}";
    let done = "data: [DONE]";
    let raw = [ev1, ev2, ev3, ev4, done].join("\n\n") + "\n\n";

    // Same hostile cuts as the reassembly test above.
    let emoji_mid = raw.find("🌍").unwrap() + 2;
    let separator_mid = raw.find(ev3).unwrap() - 1;
    let after_data_prefix = raw.find(ev4).unwrap() + "data:".len();

    let server = spawn_server(vec![MockResponse::sse_split(
        &raw,
        &[emoji_mid, separator_mid, after_data_prefix],
    )])
    .await;
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
        reqwest::Client::new(),
    );

    let conversation_id = "wire-conv-stream".to_string();
    let tags = oct_llm_provider::wire::Tags {
        conversation_id: Some(conversation_id.clone()),
        run_id: Some("run-1".to_string()),
        run_kind: Some("root".to_string()),
        purpose: Some("chat".to_string()),
    };
    let events = collect(
        oct_llm_provider::wire::with_tags(tags, model.stream(chat_request()))
            .await
            .unwrap(),
    )
    .await;
    assert!(!events.is_empty());

    let lines = wait_for_lines(&conversation_id, |ls| {
        ls.iter().any(|l| l["kind"] == "llm_stream_end")
    });

    // Envelope: every line of this request shares the request id and
    // carries the full correlation set from the tags.
    let request_ids: Vec<_> = lines
        .iter()
        .map(|l| l["request_id"].as_str().unwrap())
        .collect();
    assert!(
        request_ids.iter().all(|id| id == &request_ids[0]),
        "one request id across all lines"
    );
    for line in &lines {
        assert_eq!(line["target"], "oct_llm_provider::wire");
        assert_eq!(line["conversation_id"], "wire-conv-stream");
        assert_eq!(line["run_id"], "run-1");
        assert_eq!(line["run_kind"], "root");
        assert_eq!(line["purpose"], "chat");
        assert_eq!(line["provider"], "mock-openai");
        assert_eq!(line["model"], "test-model");
        assert!(
            line["endpoint"]
                .as_str()
                .unwrap()
                .contains("/chat/completions")
        );
        assert!(line["ts"].as_str().unwrap().contains('T'));
        assert!(line["ts_ms"].is_i64());
    }

    // Request: the logged body is the exact serialized request.
    let request = lines.iter().find(|l| l["kind"] == "llm_request").unwrap();
    assert_eq!(request["stream"], true);
    let logged_body: serde_json::Value =
        serde_json::from_str(request["body"].as_str().unwrap()).unwrap();
    let captured_body = server.captured()[0].body_json();
    assert_eq!(logged_body, captured_body);
    assert_eq!(request["truncated"], false);

    // SSE events: seq is dense 1..=N and joining the raw blocks (with the
    // framing separators re-added) reproduces the response body exactly.
    let mut sse: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["kind"] == "llm_sse_event")
        .collect();
    sse.sort_by_key(|l| l["seq"].as_u64().unwrap());
    let seqs: Vec<u64> = sse.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=5).collect::<Vec<_>>());
    let reassembled: String = sse
        .iter()
        .map(|l| l["raw"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n";
    assert_eq!(reassembled, raw, "byte-faithful reassembly by seq");

    // Stream end: one eof line counting the SSE events, no error lines.
    let ends: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["kind"] == "llm_stream_end")
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["outcome"], "eof");
    assert_eq!(ends[0]["events"], 5);
    assert!(!lines.iter().any(|l| l["kind"] == "llm_error"));
}

#[tokio::test]
async fn wire_log_generate_path_and_orphan_routing() {
    // Install the subscriber BEFORE any model call: events emitted before
    // it exists are dropped by tracing itself, not by the sink.
    wire_sink_dir();
    let body = serde_json::json!({
        "id": "msg_wire_gen",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "wire gen ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 1}
    })
    .to_string();
    let server = spawn_server(vec![
        MockResponse::json(200, body.clone()),
        // The untagged call below consumes its own response.
        MockResponse::json(200, body.clone()),
    ])
    .await;

    // Tagged call → its own conversation file.
    let conversation_id = "wire-conv-generate".to_string();
    let tags = oct_llm_provider::wire::Tags {
        conversation_id: Some(conversation_id.clone()),
        purpose: Some("compact".to_string()),
        ..Default::default()
    };
    oct_llm_provider::wire::with_tags(
        tags,
        anthropic_model(server.url(), "sk-wire-gen").generate(chat_request()),
    )
    .await
    .unwrap();

    let lines = wait_for_lines(&conversation_id, |ls| {
        ls.iter().any(|l| l["kind"] == "llm_response")
    });
    let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["llm_request", "llm_response"]);
    let request = &lines[0];
    assert_eq!(request["stream"], false);
    assert_eq!(request["purpose"], "compact");
    // No run tags were set: run_id/run_kind stay absent, not empty strings.
    assert!(request.get("run_id").is_none());
    let response = &lines[1];
    assert_eq!(response["status"], 200);
    assert_eq!(response["body"].as_str().unwrap(), body);
    assert!(response["duration_ms"].is_u64());
    // No SSE events and no stream end on the generate path.
    assert!(!kinds.contains(&"llm_sse_event"));
    assert!(!kinds.contains(&"llm_stream_end"));

    // Untagged call (no with_tags scope) → `_orphan.jsonl`.
    anthropic_model(server.url(), "sk-wire-gen")
        .generate(chat_request())
        .await
        .unwrap();
    let orphan = wait_for_lines("_orphan", |ls| {
        ls.iter().filter(|l| l["kind"] == "llm_response").count() >= 1
            && ls.iter().any(|l| l.get("conversation_id").is_none())
    });
    assert!(
        orphan
            .iter()
            .any(|l| l.get("conversation_id").is_none() && l["kind"] == "llm_request"),
        "untagged events land in _orphan.jsonl without a conversation_id key"
    );
}

#[tokio::test]
async fn wire_log_records_error_stages() {
    // Install the subscriber BEFORE any model call: events emitted before
    // it exists are dropped by tracing itself, not by the sink.
    wire_sink_dir();
    let server = spawn_server(vec![MockResponse::json(500, "{\"error\":\"kaboom\"}")]).await;
    let conversation_id = "wire-conv-error".to_string();
    let tags = oct_llm_provider::wire::Tags {
        conversation_id: Some(conversation_id.clone()),
        ..Default::default()
    };
    let result = oct_llm_provider::wire::with_tags(
        tags,
        anthropic_model(server.url(), "sk-wire-err").stream(chat_request()),
    )
    .await;
    assert!(result.is_err());

    let lines = wait_for_lines(&conversation_id, |ls| {
        ls.iter().any(|l| l["kind"] == "llm_error")
    });
    let request = lines.iter().find(|l| l["kind"] == "llm_request").unwrap();
    assert_eq!(request["stream"], true);
    let error = lines.iter().find(|l| l["kind"] == "llm_error").unwrap();
    assert_eq!(error["stage"], "status");
    assert!(
        error["message"].as_str().unwrap().contains("kaboom"),
        "error message carries the provider body: {}",
        error["message"]
    );
    // A request that failed at the status stage never produces SSE events.
    assert!(!lines.iter().any(|l| l["kind"] == "llm_sse_event"));
}

/// The reassembly invariant under CRLF framing: the decoder strips the
/// `\r\n\r\n` separators but keeps each block's internal `\r\n` line
/// endings, so re-joining with `\r\n\r\n` must reproduce the body exactly.
#[tokio::test]
async fn wire_log_sse_raw_is_byte_faithful_under_crlf_framing() {
    let ev1 =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}";
    let ev2 = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}";
    let ev3 = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";
    let done = "data: [DONE]";
    let raw = [ev1, ev2, ev3, done].join("\r\n\r\n") + "\r\n\r\n";
    let cut = raw.find("llo").unwrap() - 2;

    wire_sink_dir();
    let server = spawn_server(vec![MockResponse::sse_split(&raw, &[cut])]).await;
    let model = OpenAiCompatibleChatModel::new_with_client(
        openai_config(server.url(), Some("test-key")),
        reqwest::Client::new(),
    );
    let conversation_id = "wire-conv-crlf".to_string();
    let events = collect(
        oct_llm_provider::wire::with_tags(
            oct_llm_provider::wire::Tags {
                conversation_id: Some(conversation_id.clone()),
                ..Default::default()
            },
            model.stream(chat_request()),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(text_of(&events), "hello");

    let lines = wait_for_lines(&conversation_id, |ls| {
        ls.iter().any(|l| l["kind"] == "llm_stream_end")
    });
    let mut sse: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["kind"] == "llm_sse_event")
        .collect();
    sse.sort_by_key(|l| l["seq"].as_u64().unwrap());
    let reassembled: String = sse
        .iter()
        .map(|l| l["raw"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\r\n\r\n")
        + "\r\n\r\n";
    assert_eq!(reassembled, raw, "byte-faithful reassembly under CRLF");
}
