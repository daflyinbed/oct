use oct::core::{ContentPart, Message, Role};
use oct::providers::{parse_anthropic_generate_body, parse_anthropic_sse_transcript};
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

fn parse_anthropic_messages_from_request(body: &serde_json::Value) -> Vec<Message> {
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

            if let Some(content) = msg.get("content") {
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        parts.push(ContentPart::Text(text.to_string()));
                    }
                } else if let Some(content_array) = content.as_array() {
                    for part in content_array {
                        let part_type = part["type"].as_str().unwrap_or("");
                        match part_type {
                            "text" => {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    parts.push(ContentPart::Text(text.to_string()));
                                }
                            }
                            "tool_use" => {
                                let id = part["id"].as_str().unwrap_or_default().to_string();
                                let name = part["name"].as_str().unwrap_or_default().to_string();
                                let input = part
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                parts.push(ContentPart::ToolCall(oct::core::ToolCall {
                                    id,
                                    name,
                                    arguments: serde_json::to_string(&input).unwrap_or_default(),
                                }));
                            }
                            "tool_result" => {
                                let call_id =
                                    part["tool_use_id"].as_str().unwrap_or_default().to_string();
                                let content = part
                                    .get("content")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                parts.push(ContentPart::ToolResult(oct::core::ToolResult {
                                    call_id,
                                    content,
                                    is_error: false,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
            }

            Message { role, parts }
        })
        .collect()
}

#[test]
fn snapshot_parse_simple_text_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/simple_text.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_simple_text_response() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/simple_text.json");
    let body = extract_response_body(&fixture);
    let response = parse_anthropic_generate_body(&body).expect("Failed to parse response");
    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_tool_use_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/tool_use.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_use_response() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/tool_use.json");
    let body = extract_response_body(&fixture);
    let response = parse_anthropic_generate_body(&body).expect("Failed to parse response");
    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_tool_result_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/tool_result.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_result_response() {
    let fixture = load_fixture("tests/anthropic/fixtures/generate/tool_result.json");
    let body = extract_response_body(&fixture);
    let response = parse_anthropic_generate_body(&body).expect("Failed to parse response");
    insta::assert_yaml_snapshot!(response);
}

#[test]
fn snapshot_parse_simple_text_stream_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/simple_text.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_simple_text_stream() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/simple_text.json");
    let body = extract_response_body(&fixture);
    let events = parse_anthropic_sse_transcript(&body).expect("Failed to parse stream");
    insta::assert_yaml_snapshot!(events);
}

#[test]
fn snapshot_parse_tool_use_stream_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/tool_use.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_use_stream() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/tool_use.json");
    let body = extract_response_body(&fixture);
    let events = parse_anthropic_sse_transcript(&body).expect("Failed to parse stream");
    insta::assert_yaml_snapshot!(events);
}

#[test]
fn snapshot_parse_tool_result_stream_request() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/tool_result.json");
    let body = extract_request_body(&fixture);
    let messages = parse_anthropic_messages_from_request(&body);
    insta::assert_yaml_snapshot!(messages);
}

#[test]
fn snapshot_parse_tool_result_stream() {
    let fixture = load_fixture("tests/anthropic/fixtures/stream/tool_result.json");
    let body = extract_response_body(&fixture);
    let events = parse_anthropic_sse_transcript(&body).expect("Failed to parse stream");
    insta::assert_yaml_snapshot!(events);
}
