use oct::adapter::{parse_openai_generate_body, parse_openai_sse_transcript};
use oct::core::{ContentPart, Message, Role};
use std::fs;

fn load_fixture(path: &str) -> String {
    fs::read_to_string(path).expect(&format!("Failed to read fixture: {}", path))
}

fn extract_request_body(fixture: &str) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_str(fixture).expect("Invalid JSON fixture");
    value["request"]["body"].clone()
}

fn extract_response_body(fixture: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(fixture).expect("Invalid JSON fixture");
    value["response"]["body"]
        .as_str()
        .expect("Missing response body field")
        .to_string()
}

fn parse_messages_from_request(body: &serde_json::Value) -> Vec<Message> {
    let messages = body["messages"]
        .as_array()
        .expect("Messages should be an array");
    messages
        .iter()
        .map(|msg| {
            let role = match msg["role"].as_str().unwrap_or("user") {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => Role::User,
            };
            let mut parts = Vec::new();

            if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    parts.push(ContentPart::Text(content.to_string()));
                }
            }

            if let Some(reasoning) = msg.get("reasoning_content").and_then(|c| c.as_str()) {
                if !reasoning.is_empty() {
                    parts.push(ContentPart::Reasoning(reasoning.to_string()));
                }
            }

            if let Some(tool_calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
                for call in tool_calls {
                    let id = call["id"].as_str().unwrap_or_default().to_string();
                    let name = call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or_default();
                    let arguments = serde_json::from_str(args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.to_string()));
                    parts.push(ContentPart::ToolCall(oct::core::ToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
            }

            if role == Role::Tool {
                let tool_call_id = msg["tool_call_id"].as_str().unwrap_or_default().to_string();
                let content = msg["content"].as_str().unwrap_or_default();
                parts.push(ContentPart::ToolResult(oct::core::ToolResult {
                    call_id: tool_call_id,
                    content: serde_json::from_str(content)
                        .unwrap_or_else(|_| serde_json::Value::String(content.to_string())),
                    is_error: false,
                }));
            }

            Message { role, parts }
        })
        .collect()
}

#[test]
fn snapshot_parse_simple_text_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/simple_text.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_simple_text_response() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/simple_text.json");
    let body = extract_response_body(&fixture);
    let response = parse_openai_generate_body(&body).expect("Failed to parse response");

    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_single_tool_call_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/single_tool_call.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_single_tool_call_response() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/single_tool_call.json");
    let body = extract_response_body(&fixture);
    let response = parse_openai_generate_body(&body).expect("Failed to parse response");

    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_parallel_tool_calls_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/parallel_tool_calls.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_parallel_tool_calls_response() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/parallel_tool_calls.json");
    let body = extract_response_body(&fixture);
    let response = parse_openai_generate_body(&body).expect("Failed to parse response");

    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_tool_result_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/tool_result.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_result_response() {
    let fixture = load_fixture("tests/moonshot/fixtures/generate/tool_result.json");
    let body = extract_response_body(&fixture);
    let response = parse_openai_generate_body(&body).expect("Failed to parse response");

    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_simple_text_stream_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/simple_text.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_simple_text_stream() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/simple_text.json");
    let body = extract_response_body(&fixture);
    let events = parse_openai_sse_transcript(&body).expect("Failed to parse stream");

    insta::assert_yaml_snapshot!(events);
}

#[test]
fn snapshot_parse_tool_call_stream_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/tool_call.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_call_stream() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/tool_call.json");
    let body = extract_response_body(&fixture);
    let events = parse_openai_sse_transcript(&body).expect("Failed to parse stream");

    insta::assert_yaml_snapshot!(events);
}

#[test]
fn snapshot_parse_tool_result_stream_request() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/tool_result.json");
    let body = extract_request_body(&fixture);
    let messages = parse_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_result_stream() {
    let fixture = load_fixture("tests/moonshot/fixtures/stream/tool_result.json");
    let body = extract_response_body(&fixture);
    let events = parse_openai_sse_transcript(&body).expect("Failed to parse stream");

    insta::assert_yaml_snapshot!(events);
}
