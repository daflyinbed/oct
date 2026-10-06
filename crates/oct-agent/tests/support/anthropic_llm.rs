//! A minimal Anthropic-shaped LLM mock, the acceptance-test counterpart of
//! [`super::llm`]: the production Anthropic adapter must hit whatever
//! `base_url` the DB provider row carries, authenticated with the row's
//! `api_key` (the `x-api-key` header) — no help from the environment. The
//! mock serves scripted turns on `POST /messages` (the adapter appends that
//! path to the row's base_url) in both wire shapes the agent uses:
//! streaming (`stream: true`, the agent loop) and non-streaming `generate`
//! (manual compaction / title generation).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// One scripted turn: the text the assistant "writes".
pub enum AnthropicTurn {
    Text(String),
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
        let state = Arc::new(MockState {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
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

    let turn = state
        .turns
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(AnthropicTurn::Text("no more scripted turns".into()));
    let AnthropicTurn::Text(text) = turn;

    if body.get("stream") == Some(&json!(true)) {
        // Anthropic SSE framing: message_start (input usage) → one text
        // block streamed as a single delta → message_delta (output usage +
        // stop reason) → message_stop.
        let events: Vec<Result<Event, Infallible>> = vec![
            sse(
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
            ),
            sse(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }),
            ),
            sse(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                }),
            ),
            sse(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": 0,
                }),
            ),
            sse(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 5},
                }),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ];
        return Sse::new(futures_util::stream::iter(events)).into_response();
    }

    // Non-streaming generate (manual compaction / title generation).
    Json(json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 2},
    }))
    .into_response()
}

fn sse(event: &str, payload: Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(event).data(payload.to_string()))
}
