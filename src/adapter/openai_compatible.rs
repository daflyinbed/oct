use async_trait::async_trait;
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::core::{
    ContentPart, FinishReason, Message, ModelError, StreamEvent, ToolChoice, ToolSpec, Usage,
};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::ModelInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiCompatibleConfig {
    pub provider_name: &'static str,
    pub base_url: String,
    pub api_key_env: &'static str,
    pub default_headers: Map<String, Value>,
    pub model_info: ModelInfo,
    pub use_responses_api: bool,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleChatModel {
    pub config: OpenAiCompatibleConfig,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Default)]
struct OpenAiToolCallAccumulator {
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiChoice {
    pub message: OpenAiRequestMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiGenerateResponse {
    pub id: Option<String>,
    #[serde(default)]
    pub choices: Vec<OpenAiChoice>,
    #[serde(default)]
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OpenAiStreamChunkChoiceDelta {
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiStreamChunkChoice {
    #[serde(default)]
    pub delta: OpenAiStreamChunkChoiceDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiStreamChunk {
    #[serde(default)]
    pub choices: Vec<OpenAiStreamChunkChoice>,
    #[serde(default)]
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiRequestMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiToolFunction {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiResponseFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiWireRequest {
    pub model: String,
    pub messages: Vec<OpenAiRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<OpenAiToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<OpenAiResponseFormat>,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub provider_options: Map<String, Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
}

impl OpenAiCompatibleChatModel {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if self.config.use_responses_api {
            format!("{base}/responses")
        } else {
            format!("{base}/chat/completions")
        }
    }

    fn auth_header_value(&self) -> Result<HeaderValue, ModelError> {
        let api_key = std::env::var(self.config.api_key_env).map_err(|_| ModelError::Authentication)?;
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|err| ModelError::transport(format!("invalid auth header: {err}")))
    }

    fn build_headers(&self) -> Result<HeaderMap, ModelError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(AUTHORIZATION, self.auth_header_value()?);

        for (key, value) in &self.config.default_headers {
            let value = value
                .as_str()
                .ok_or_else(|| ModelError::provider(format!("default header {key} must be a string")))?;
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|err| ModelError::transport(format!("invalid header name {key}: {err}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|err| ModelError::transport(format!("invalid header value for {key}: {err}")))?;
            headers.insert(name, value);
        }

        Ok(headers)
    }

    pub fn to_wire_request(&self, req: &ChatRequest) -> Result<OpenAiWireRequest, ModelError> {
        let messages = req
            .messages
            .iter()
            .map(map_message)
            .collect::<Result<Vec<_>, _>>()?;

        let tools = req.tools.iter().map(map_tool).collect();
        let tool_choice = req.options.tool_choice.as_ref().map(map_tool_choice);
        let response_format = req.options.json_schema.clone().map(|schema| OpenAiResponseFormat {
            kind: "json_schema".to_string(),
            json_schema: Some(schema),
        });

        Ok(OpenAiWireRequest {
            model: self.config.model_info.model_id.clone(),
            messages,
            temperature: req.options.temperature,
            top_p: req.options.top_p,
            max_tokens: req.options.max_output_tokens,
            stop: req.options.stop_sequences.clone(),
            tools,
            tool_choice,
            response_format,
            provider_options: req.options.provider_options.clone(),
            stream: false,
            stream_options: None,
        })
    }

    fn to_streaming_wire_request(&self, req: &ChatRequest) -> Result<OpenAiWireRequest, ModelError> {
        let mut wire = self.to_wire_request(req)?;
        wire.stream = true;
        wire.stream_options = Some(json!({ "include_usage": true }));
        Ok(wire)
    }
}

#[async_trait]
impl ChatModel for OpenAiCompatibleChatModel {
    fn info(&self) -> &ModelInfo {
        &self.config.model_info
    }

    async fn generate(&self, req: ChatRequest) -> Result<ChatResponse, ModelError> {
        let wire = self.to_wire_request(&req)?;
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| ModelError::transport(format!("request failed: {err}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ModelError::Authentication);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ModelError::RateLimited);
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            return Err(ModelError::provider(format!(
                "{} returned {}: {}",
                self.config.provider_name, status, body
            )));
        }

        let payload: OpenAiGenerateResponse = response
            .json()
            .await
            .map_err(|err| ModelError::transport(format!("invalid response json: {err}")))?;

        map_openai_generate_response(payload)
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        let wire = self.to_streaming_wire_request(&req)?;
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| ModelError::transport(format!("request failed: {err}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ModelError::Authentication);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ModelError::RateLimited);
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            return Err(ModelError::provider(format!(
                "{} returned {}: {}",
                self.config.provider_name, status, body
            )));
        }

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut buffer = String::new();
            let mut tool_state = HashMap::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk
                    .map_err(|err| ModelError::transport(format!("stream read failed: {err}")))?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|err| ModelError::transport(format!("stream chunk was not valid utf-8: {err}")))?;
                buffer.push_str(text);

                while let Some(event) = take_sse_event(&mut buffer) {
                    for normalized in parse_openai_sse_event_with_state(&event, &mut tool_state)? {
                        yield normalized;
                    }
                }
            }

            for remainder in flush_sse_buffer(&mut buffer) {
                for normalized in parse_openai_sse_event_with_state(&remainder, &mut tool_state)? {
                    yield normalized;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

pub fn map_openai_generate_response(payload: OpenAiGenerateResponse) -> Result<ChatResponse, ModelError> {
    let usage = payload.usage.as_ref().map(map_openai_usage);
    let mut provider_metadata = Map::new();
    if let Some(raw_usage) = payload.usage {
        provider_metadata.insert("raw_usage".to_string(), raw_usage);
    }

    let choice = payload
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::provider("openai response contained no choices"))?;

    Ok(ChatResponse {
        message: map_response_message(choice.message)?,
        finish_reason: map_openai_finish_reason(choice.finish_reason.as_deref()),
        usage,
        provider_response_id: payload.id,
        provider_metadata,
    })
}

fn map_message(message: &Message) -> Result<OpenAiRequestMessage, ModelError> {
    let role = match message.role {
        crate::core::Role::System => "system",
        crate::core::Role::User => "user",
        crate::core::Role::Assistant => "assistant",
        crate::core::Role::Tool => "tool",
    }
    .to_string();

    let mut parts = Vec::with_capacity(message.parts.len());
    for part in &message.parts {
        parts.push(match part {
            ContentPart::Text(text) => json!({ "type": "text", "text": text }),
            ContentPart::ImageUrl { url } => {
                json!({ "type": "image_url", "image_url": { "url": url } })
            }
            ContentPart::Reasoning(text) => json!({ "type": "text", "text": text }),
            ContentPart::ToolCall(call) => json!({
                "type": "tool_call",
                "id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }),
            ContentPart::ToolResult(result) => json!({
                "type": "tool_result",
                "tool_call_id": result.call_id,
                "content": result.content,
                "is_error": result.is_error,
            }),
        });
    }

    let content = if parts.len() == 1 {
        parts.into_iter().next().unwrap()
    } else {
        Value::Array(parts)
    };

    Ok(OpenAiRequestMessage { role, content })
}

fn map_response_message(message: OpenAiRequestMessage) -> Result<Message, ModelError> {
    let role = match message.role.as_str() {
        "system" => crate::core::Role::System,
        "user" => crate::core::Role::User,
        "assistant" => crate::core::Role::Assistant,
        "tool" => crate::core::Role::Tool,
        other => {
            return Err(ModelError::provider(format!(
                "unsupported openai response role: {other}"
            )))
        }
    };

    Ok(Message {
        role,
        parts: parse_openai_content(message.content)?,
    })
}

fn parse_openai_content(content: Value) -> Result<Vec<ContentPart>, ModelError> {
    match content {
        Value::String(text) => Ok(vec![ContentPart::Text(text)]),
        Value::Array(parts) => parts.into_iter().map(parse_openai_part).collect(),
        Value::Object(_) => Ok(vec![parse_openai_part(content)?]),
        other => Err(ModelError::provider(format!(
            "unsupported openai content payload: {other}"
        ))),
    }
}

fn parse_openai_part(part: Value) -> Result<ContentPart, ModelError> {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text");

    match kind {
        "text" | "output_text" => Ok(ContentPart::Text(
            part.get("text")
                .or_else(|| part.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        "image_url" => {
            let url = part
                .get("image_url")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::provider("image_url part missing url"))?;
            Ok(ContentPart::ImageUrl {
                url: url.to_string(),
            })
        }
        "tool_call" | "function_call" => Ok(ContentPart::ToolCall(crate::core::ToolCall {
            id: part
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: part
                .get("name")
                .or_else(|| part.get("function").and_then(|v| v.get("name")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: part
                .get("arguments")
                .cloned()
                .or_else(|| part.get("function").and_then(|v| v.get("arguments")).cloned())
                .unwrap_or(Value::Null),
        })),
        "tool_result" => Ok(ContentPart::ToolResult(crate::core::ToolResult {
            call_id: part
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content: part.get("content").cloned().unwrap_or(Value::Null),
            is_error: part
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })),
        _ => Err(ModelError::provider(format!(
            "unsupported openai content part type: {kind}"
        ))),
    }
}

fn map_tool(tool: &ToolSpec) -> OpenAiToolDefinition {
    OpenAiToolDefinition {
        kind: "function".to_string(),
        function: OpenAiToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        },
    }
}

fn map_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
}

pub fn map_openai_usage(raw: &Value) -> Usage {
    let mut provider_details = Map::new();
    provider_details.insert("raw".to_string(), raw.clone());

    Usage {
        input_tokens: raw
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        output_tokens: raw
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        total_tokens: raw
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        reasoning_tokens: raw
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        cached_input_tokens: raw
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        provider_details,
    }
}

pub fn map_openai_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

pub fn aggregate_stream_events(events: &[StreamEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage(usage) => Some(usage.clone()),
        _ => None,
    })
}

fn take_sse_event(buffer: &mut String) -> Option<String> {
    if let Some(index) = buffer.find("\n\n") {
        let event = buffer[..index].to_string();
        buffer.drain(..index + 2);
        Some(event)
    } else if let Some(index) = buffer.find("\r\n\r\n") {
        let event = buffer[..index].to_string();
        buffer.drain(..index + 4);
        Some(event)
    } else {
        None
    }
}

fn flush_sse_buffer(buffer: &mut String) -> Vec<String> {
    if buffer.trim().is_empty() {
        Vec::new()
    } else {
        vec![std::mem::take(buffer)]
    }
}

pub fn parse_openai_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_openai_sse_event_with_state(event, &mut HashMap::new())
}

fn parse_openai_sse_event_with_state(
    event: &str,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut data_lines = Vec::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }

    if data_lines.is_empty() {
        return Ok(Vec::new());
    }

    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(vec![StreamEvent::Finish(FinishReason::Stop)]);
    }

    let chunk: OpenAiStreamChunk = serde_json::from_str(&data)
        .map_err(|err| ModelError::transport(format!("invalid openai stream chunk: {err}")))?;

    normalize_openai_stream_chunk_with_state(chunk, tool_state)
}

pub fn normalize_openai_stream_chunk(chunk: OpenAiStreamChunk) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_openai_stream_chunk_with_state(chunk, &mut HashMap::new())
}

fn normalize_openai_stream_chunk_with_state(
    chunk: OpenAiStreamChunk,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = Vec::new();

    if let Some(usage) = chunk.usage.as_ref() {
        events.push(StreamEvent::Usage(map_openai_usage(usage)));
    }

    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            for part in parse_openai_stream_delta_content(content)? {
                events.push(part);
            }
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                let (delta_event, final_event) = parse_openai_tool_call_delta(tool_call, tool_state)?;
                events.push(delta_event);
                if let Some(final_event) = final_event {
                    events.push(final_event);
                }
            }
        }

        if let Some(reason) = choice.finish_reason {
            if reason == "tool_calls" || reason == "function_call" {
                for (call_id, accumulator) in std::mem::take(tool_state) {
                    let parsed_arguments = parse_json_string_or_raw(&accumulator.arguments);
                    events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                        id: call_id,
                        name: accumulator.name.unwrap_or_default(),
                        arguments: parsed_arguments,
                    }));
                }
            }
            events.push(StreamEvent::Finish(map_openai_finish_reason(Some(&reason))));
        }
    }

    Ok(events)
}

fn parse_openai_stream_delta_content(content: Value) -> Result<Vec<StreamEvent>, ModelError> {
    match content {
        Value::String(text) => Ok(vec![StreamEvent::TextDelta(text)]),
        Value::Array(parts) => {
            let mut events = Vec::new();
            for part in parts {
                events.extend(parse_openai_stream_delta_part(part)?);
            }
            Ok(events)
        }
        Value::Object(_) => parse_openai_stream_delta_part(content),
        other => Err(ModelError::provider(format!(
            "unsupported openai stream content payload: {other}"
        ))),
    }
}

fn parse_openai_stream_delta_part(part: Value) -> Result<Vec<StreamEvent>, ModelError> {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text");

    match kind {
        "text" | "output_text" => Ok(vec![StreamEvent::TextDelta(
            part.get("text")
                .or_else(|| part.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )]),
        "reasoning" => Ok(vec![StreamEvent::ReasoningDelta(
            part.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )]),
        _ => Ok(Vec::new()),
    }
}

fn parse_openai_tool_call_delta(
    part: Value,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<(StreamEvent, Option<StreamEvent>), ModelError> {
    let call_id = part
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let (name, arguments_delta) = if let Some(function) = part.get("function") {
        (
            function.get("name").and_then(Value::as_str).map(str::to_string),
            function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    } else {
        (
            part.get("name").and_then(Value::as_str).map(str::to_string),
            part.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };

    let accumulator = tool_state.entry(call_id.clone()).or_default();
    if let Some(name) = name.clone() {
        accumulator.name = Some(name);
    }
    accumulator.arguments.push_str(&arguments_delta);

    let final_event = if let Some(done) = part.get("done").and_then(Value::as_bool) {
        if done {
            let completed = tool_state.remove(&call_id).unwrap_or_default();
            Some(StreamEvent::ToolCall(crate::core::ToolCall {
                id: call_id.clone(),
                name: completed.name.unwrap_or_default(),
                arguments: parse_json_string_or_raw(&completed.arguments),
            }))
        } else {
            None
        }
    } else {
        None
    };

    Ok((
        StreamEvent::ToolCallDelta {
            call_id,
            name,
            arguments_delta,
        },
        final_event,
    ))
}

fn parse_json_string_or_raw(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}
