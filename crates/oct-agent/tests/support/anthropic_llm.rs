//! A minimal Anthropic-shaped LLM mock, the acceptance-test counterpart of
//! [`super::llm`]: the production Anthropic adapter must hit whatever
//! `base_url` the DB provider row carries, authenticated with the row's
//! `api_key` (the `x-api-key` header) — no help from the environment. The
//! mock serves scripted turns on `POST /messages` (the adapter appends that
//! path to the row's base_url) in both wire shapes the agent uses:
//! streaming (`stream: true`, the agent loop) and non-streaming `generate`
//! (manual compaction / title generation).

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// One scripted turn: text or complete provider content blocks.
pub enum AnthropicTurn {
    Text(String),
    Blocks {
        content: Vec<Value>,
        stop_reason: String,
    },
}

/// What the mock observed of one request, for header/wire assertions.
#[derive(Debug, Clone)]
pub struct CapturedAnthropicRequest {
    pub path: String,
    /// The `x-api-key` header — the DB row's api_key must arrive here.
    pub api_key: Option<String>,
    pub anthropic_version: Option<String>,
    pub body: Value,
}

struct MockState {
    /// One turn popped per incoming request (streaming and non-streaming
    /// share the queue: a conversation's turns play strictly in order).
    turns: Mutex<VecDeque<AnthropicTurn>>,
    requests: Mutex<Vec<CapturedAnthropicRequest>>,
    enforce_prefix_binding: bool,
    thinking_prefixes: Mutex<HashMap<(String, String), Value>>,
}

#[derive(Clone)]
pub struct AnthropicMock {
    /// Base URL to put into the provider row's `base_url` (the adapter
    /// appends `/messages`).
    pub url: String,
    state: Arc<MockState>,
}

impl AnthropicMock {
    pub async fn spawn(turns: Vec<AnthropicTurn>) -> Self {
        Self::spawn_inner(turns, false).await
    }

    /// Enforce the newer Claude models' signature binding against the
    /// system prompt, tools and preceding messages.
    pub async fn spawn_with_prefix_binding(turns: Vec<AnthropicTurn>) -> Self {
        Self::spawn_inner(turns, true).await
    }

    async fn spawn_inner(turns: Vec<AnthropicTurn>, enforce_prefix_binding: bool) -> Self {
        let state = Arc::new(MockState {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
            enforce_prefix_binding,
            thinking_prefixes: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .route("/messages", post(messages))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind anthropic llm mock");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("anthropic llm mock server");
        });

        AnthropicMock {
            url: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    /// Requests in arrival order.
    pub fn requests(&self) -> Vec<CapturedAnthropicRequest> {
        self.state.requests.lock().unwrap().clone()
    }
}

async fn messages(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Json(body): Json<Value>,
) -> Response {
    let binding_result = check_prefix_bindings(&state, &body);
    state
        .requests
        .lock()
        .unwrap()
        .push(CapturedAnthropicRequest {
            path: uri.path().to_string(),
            api_key: headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            anthropic_version: headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            body: body.clone(),
        });

    if let Err(message) = binding_result {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"type":"error", "error":{"type":"invalid_request_error", "message":message}})),
        )
            .into_response();
    }

    let turn = state
        .turns
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(AnthropicTurn::Text("no more scripted turns".into()));
    let (content, stop_reason) = match turn {
        AnthropicTurn::Text(text) => (
            vec![json!({"type":"text", "text":text})],
            "end_turn".to_string(),
        ),
        AnthropicTurn::Blocks {
            content,
            stop_reason,
        } => (content, stop_reason),
    };

    if state.enforce_prefix_binding {
        let prefix = thinking_prefix(&body, body["messages"].as_array().unwrap().len());
        let mut prefixes = state.thinking_prefixes.lock().unwrap();
        for block in &content {
            if let Some(key) = thinking_key(block) {
                prefixes.insert(key, prefix.clone());
            }
        }
    }

    if body.get("stream") == Some(&json!(true)) {
        // Anthropic SSE framing: message_start (input usage) → content
        // blocks with deltas → message_delta (output usage + stop reason)
        // → message_stop. Signatures are split to exercise accumulation.
        let mut events: Vec<Result<Event, Infallible>> = vec![sse(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-test",
                    "content": [],
                    "usage": {"input_tokens": 3, "output_tokens": 0},
                },
            }),
        )];
        for (index, block) in content.iter().enumerate() {
            let (start, deltas) = match block["type"].as_str().unwrap() {
                "text" => (
                    json!({"type":"text","text":""}),
                    vec![json!({"type":"text_delta","text":block["text"]})],
                ),
                "thinking" => {
                    let signature = block["signature"].as_str().unwrap();
                    let (first, last) = signature.split_at(signature.len() / 2);
                    (
                        json!({"type":"thinking","thinking":"","signature":""}),
                        vec![
                            json!({"type":"thinking_delta","thinking":block["thinking"]}),
                            json!({"type":"signature_delta","signature":first}),
                            json!({"type":"signature_delta","signature":last}),
                        ],
                    )
                }
                "tool_use" => (
                    json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                    vec![
                        json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
                    ],
                ),
                "redacted_thinking" => (block.clone(), vec![]),
                other => panic!("unsupported mock block {other}"),
            };
            events.push(sse(
                "content_block_start",
                json!({"type":"content_block_start","index":index,"content_block":start}),
            ));
            for delta in deltas {
                events.push(sse(
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":delta}),
                ));
            }
            events.push(sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
        events.extend([
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                    "usage": {"output_tokens": 5},
                }),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ]);
        return Sse::new(futures_util::stream::iter(events)).into_response();
    }

    // Non-streaming generate (manual compaction / title generation).
    Json(json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": content,
        "stop_reason": stop_reason,
        "usage": {"input_tokens": 3, "output_tokens": 2},
    }))
    .into_response()
}

fn thinking_key(block: &Value) -> Option<(String, String)> {
    let kind = block["type"].as_str()?;
    let value = match kind {
        "thinking" => block["signature"].as_str()?,
        "redacted_thinking" => block["data"].as_str()?,
        _ => return None,
    };
    Some((kind.to_string(), value.to_string()))
}

fn thinking_prefix(body: &Value, before: usize) -> Value {
    // Earlier thinking blocks are checked separately, rather than being
    // part of the signed message prefix themselves.
    let messages: Vec<_> = body["messages"].as_array().unwrap()[..before]
        .iter()
        .map(|message| {
            let mut message = message.clone();
            message["content"]
                .as_array_mut()
                .unwrap()
                .retain(|block| thinking_key(block).is_none());
            message
        })
        .collect();
    json!({"system":body["system"], "tools":body["tools"], "messages":messages})
}

fn check_prefix_bindings(state: &MockState, body: &Value) -> Result<(), &'static str> {
    if !state.enforce_prefix_binding {
        return Ok(());
    }
    let prefixes = state.thinking_prefixes.lock().unwrap();
    for (index, message) in body["messages"].as_array().unwrap().iter().enumerate() {
        for block in message["content"].as_array().unwrap() {
            let Some(key) = thinking_key(block) else {
                continue;
            };
            if prefixes.get(&key) != Some(&thinking_prefix(body, index)) {
                return Err("Invalid signature in thinking block: the conversation prefix changed");
            }
        }
    }
    Ok(())
}

fn sse(event: &str, payload: Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(event).data(payload.to_string()))
}
