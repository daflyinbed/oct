//! An OpenAI-compatible LLM mock served by axum, used by the API-level
//! e2e tests. The production chat path resolves custom providers from the
//! database with an arbitrary `base_url`, so a provider row pointing here
//! puts the ENTIRE production chain (HTTP API → registry → reqwest → SSE
//! decode → agent loop → tools → persistence → SSE broadcast) under test
//! with zero production-code changes.
//!
//! Unlike the provider crate's raw-TCP mock (which exists to control byte
//! boundaries), this one only needs to serve valid SSE turns: scripted
//! completions, a hanging stream (cancellation/conflict scenarios), or HTTP
//! failures.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;

use async_stream::stream;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::watch;

/// One scripted LLM turn.
pub enum LlmTurn {
    /// Serve these chunk payloads as SSE `data:` events, then `data:
    /// [DONE]` and end the stream.
    Complete(Vec<Value>),
    /// Serve the chunks, then hold the stream open forever (no DONE): the
    /// agent loop must end via cancellation, never via stream end.
    Hang(Vec<Value>),
    /// Respond with this HTTP status instead of a stream.
    Fail(u16),
    /// Deterministic mid-stream pause: serve `pre`, then HOLD the stream
    /// until the gate opens, then serve `post` and end normally. Used to
    /// park a run mid-flight (e.g. reconnect tests assert the run status
    /// and the event replay while the gate is closed, then open it to let
    /// the run finish). No sleeps: the test holds the sending half of
    /// [`gate`] and opens it explicitly.
    Gated {
        pre: Vec<Value>,
        gate: watch::Receiver<bool>,
        post: Vec<Value>,
    },
}

/// Create a gate for [`LlmTurn::Gated`]: the returned sender opens the gate
/// (`send(true)`); a gate opened before the turn starts is already open when
/// the stream reaches it.
pub fn gate() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
}

/// One scripted response for a NON-streaming call (the background title
/// generation task's `generate()` request). Routed by request shape: the
/// agent loop always streams (`stream: true`), the title call never does.
pub enum TitleTurn {
    /// 200 with this string as the assistant message content.
    Text(String),
    /// Respond with this HTTP status instead of a completion.
    Fail(u16),
}

struct LlmMockState {
    /// One turn popped per incoming STREAMING request.
    turns: Mutex<VecDeque<LlmTurn>>,
    /// One turn popped per incoming NON-streaming request.
    title_turns: Mutex<VecDeque<TitleTurn>>,
    /// Captured request bodies, in arrival order.
    requests: Mutex<Vec<Value>>,
}

#[derive(Clone)]
pub struct LlmMock {
    /// Base URL to put into a provider row's `base_url`.
    pub url: String,
    state: Arc<LlmMockState>,
}

impl LlmMock {
    /// Queue scripted responses for non-streaming (title-generation) calls,
    /// consumed in order. A non-streaming request with an empty queue gets a
    /// 500 — tests that never trigger title generation never see it, and
    /// ones that do script the outcome explicitly.
    pub fn push_title_turns(&self, turns: impl IntoIterator<Item = TitleTurn>) {
        self.state.title_turns.lock().unwrap().extend(turns);
    }

    /// Request bodies the agent sent, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }
}

pub async fn spawn_llm_mock(turns: Vec<LlmTurn>) -> LlmMock {
    let state = Arc::new(LlmMockState {
        turns: Mutex::new(turns.into()),
        title_turns: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind llm mock");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("llm mock server");
    });

    LlmMock {
        url: format!("http://127.0.0.1:{port}"),
        state,
    }
}

async fn chat_completions(
    State(state): State<Arc<LlmMockState>>,
    Json(body): Json<Value>,
) -> Response {
    state.requests.lock().unwrap().push(body.clone());

    // Non-streaming requests are the background title generation (the agent
    // loop always streams). Serve them from the title queue as plain
    // chat.completion JSON.
    if body.get("stream") != Some(&json!(true)) {
        return match state.title_turns.lock().unwrap().pop_front() {
            None => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "no scripted title turns",
            )
                .into_response(),
            Some(TitleTurn::Fail(status)) => (
                axum::http::StatusCode::from_u16(status).unwrap(),
                "scripted title failure",
            )
                .into_response(),
            Some(TitleTurn::Text(content)) => Json(json!({
                "id": "chatcmpl-mock-title",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop",
                }],
            }))
            .into_response(),
        };
    }

    let turn = state.turns.lock().unwrap().pop_front();
    match turn {
        None => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "no more scripted turns",
        )
            .into_response(),
        Some(LlmTurn::Fail(status)) => (
            axum::http::StatusCode::from_u16(status).unwrap(),
            "scripted failure",
        )
            .into_response(),
        Some(LlmTurn::Complete(chunks)) => {
            let events: Vec<Result<Event, Infallible>> = chunks
                .into_iter()
                .map(|c| Ok(Event::default().json_data(c).expect("serialize chunk")))
                .chain([Ok(Event::default().data("[DONE]"))])
                .collect();
            Sse::new(futures_util::stream::iter(events)).into_response()
        }
        Some(LlmTurn::Hang(chunks)) => {
            let events: Vec<Result<Event, Infallible>> = chunks
                .into_iter()
                .map(|c| Ok(Event::default().json_data(c).expect("serialize chunk")))
                .collect();
            // Stream the chunks, then never yield again and never end.
            let stream = futures_util::stream::iter(events).chain(futures_util::stream::pending());
            Sse::new(stream).into_response()
        }
        Some(LlmTurn::Gated {
            pre,
            mut gate,
            post,
        }) => {
            let pre_events: Vec<Result<Event, Infallible>> = pre
                .into_iter()
                .map(|c| Ok(Event::default().json_data(c).expect("serialize chunk")))
                .collect();
            let post_events: Vec<Result<Event, Infallible>> = post
                .into_iter()
                .map(|c| Ok(Event::default().json_data(c).expect("serialize chunk")))
                .chain([Ok(Event::default().data("[DONE]"))])
                .collect();
            let stream = stream! {
                for event in pre_events {
                    yield event;
                }
                // Hold the stream until the test opens the gate (or the gate
                // sender is dropped, which also releases the stream).
                while !*gate.borrow() {
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
                for event in post_events {
                    yield event;
                }
            };
            Sse::new(stream).into_response()
        }
    }
}

// --- Chunk payload constructors (OpenAI chat.completion.chunk shape) ------

pub fn text_chunk(content: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": content}}],
    })
}

/// A streamed tool-call delta carrying the full call in one chunk (id,
/// name and complete arguments together).
pub fn tool_call_chunk(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            },
        }],
    })
}

pub fn finish_chunk(reason: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
    })
}
