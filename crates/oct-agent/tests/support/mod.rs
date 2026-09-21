//! Shared test support for oct-agent integration tests.
//!
//! Two harnesses live here:
//!
//! - [`ScriptedModel`] + [`spawn_agent`]: drive the agent loop directly with
//!   a scripted ChatModel (records every `ChatRequest` it receives) while
//!   tools, SQLite and event broadcast run for real.
//! - [`TestApp`] (+ `llm::spawn_llm_mock`): a real oct-agent HTTP server on a
//!   random port with its own migrated SQLite file — full black-box e2e over
//!   HTTP, with the LLM behind a provider row pointing at the mock server.
// Each test target compiles this module independently, so any given target
// only uses part of it.
#![allow(dead_code)]

pub mod llm;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use oct_agent::agent::loop_runner::run_agent_loop;
use oct_agent::agent::{AgentContext, AgentEvent, RunHandle};
use oct_agent::db;
use oct_llm_provider::core::{FinishReason, ModelError, Role, StreamEvent, ToolCall, Usage};
use oct_llm_provider::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use oct_llm_provider::provider::{ModelCapabilities, ModelInfo, ModelLimits};
use oct_llm_provider::providers::default_registry;

/// One scripted model turn.
pub enum Turn {
    /// Stream these items, then end the turn's stream normally.
    Stream(Vec<Result<StreamEvent, ModelError>>),
    /// Stream these items, then never produce another item nor end the
    /// stream — the agent loop must observe cancellation while pending on
    /// the next event.
    StreamThenHang(Vec<Result<StreamEvent, ModelError>>),
    /// Fail to start the stream at all (transport/auth level failure).
    Fail(ModelError),
}

struct ScriptedState {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<ChatRequest>>,
}

/// A ChatModel that plays scripted turns and records received requests.
pub struct ScriptedModel {
    state: Arc<ScriptedState>,
    info: ModelInfo,
}

/// Cloneable view onto a [`ScriptedModel`] for assertions after the model
/// itself has been moved into the agent context.
#[derive(Clone)]
pub struct ScriptedObserver {
    state: Arc<ScriptedState>,
}

impl ScriptedObserver {
    /// Every `ChatRequest` the agent loop sent, in order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.state.requests.lock().unwrap().clone()
    }
}

impl ScriptedModel {
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            state: Arc::new(ScriptedState {
                turns: Mutex::new(turns.into()),
                requests: Mutex::new(Vec::new()),
            }),
            info: test_model_info(),
        }
    }

    pub fn observer(&self) -> ScriptedObserver {
        ScriptedObserver {
            state: self.state.clone(),
        }
    }
}

fn test_model_info() -> ModelInfo {
    ModelInfo::new("scripted", "test-model")
        .with_capabilities(ModelCapabilities {
            streaming: true,
            native_tools: true,
            vision: false,
            json_mode: false,
            reasoning: true,
            usage: true,
        })
        .with_limits(ModelLimits {
            max_input_tokens: Some(8192),
            max_output_tokens: Some(4096),
            max_total_tokens: Some(12288),
        })
}

#[async_trait]
impl ChatModel for ScriptedModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }

    async fn generate(&self, _req: ChatRequest) -> Result<ChatResponse, ModelError> {
        Err(ModelError::unsupported("generate not scripted"))
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        self.state.requests.lock().unwrap().push(req);

        let turn = self
            .state
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::provider("scripted model ran out of turns"))?;

        match turn {
            Turn::Fail(err) => Err(err),
            Turn::Stream(items) => Ok(Box::pin(futures_util::stream::iter(items))),
            Turn::StreamThenHang(items) => Ok(Box::pin(
                futures_util::stream::iter(items).chain(futures_util::stream::pending()),
            )),
        }
    }
}

// --- Script item constructors -------------------------------------------

pub fn text(delta: &str) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::TextDelta(delta.to_string()))
}

pub fn reasoning(delta: &str) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::ReasoningDelta(delta.to_string()))
}

pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
    }))
}

pub fn usage(input: u32, output: u32) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::Usage(Usage::new(
        Some(input),
        Some(output),
        Some(input + output),
    )))
}

pub fn finish(reason: FinishReason) -> Result<StreamEvent, ModelError> {
    Ok(StreamEvent::Finish(reason))
}

// --- Test environment ----------------------------------------------------

/// A temp directory with a migrated SQLite database inside it. The pool uses
/// a real file (not `:memory:`) because the production pool opens several
/// connections and each `:memory:` connection would be its own database.
pub struct TestEnv {
    pub dir: tempfile::TempDir,
    pub pool: sqlx::SqlitePool,
}

impl TestEnv {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let url = format!("sqlite://{}", dir.path().join("test.db").display());
        let pool = db::init_pool(&url).await.expect("init test database");
        Self { dir, pool }
    }
}

/// Create a project (rooted at `working_dir`) and a conversation inside it;
/// returns the conversation id. Messages reference conversations via a
/// foreign key, so agent-loop persistence needs the rows to exist.
pub async fn new_conversation(pool: &sqlx::SqlitePool, working_dir: &std::path::Path) -> String {
    let project = db::projects::create_project(
        pool,
        &db::projects::CreateProjectRequest {
            name: "test-project".to_string(),
            working_dir: working_dir.to_string_lossy().into_owned(),
        },
    )
    .await
    .expect("create project");

    let conversation =
        db::conversations::create_conversation(pool, &project.id, Some("test")).await;
    conversation.expect("create conversation").id
}

/// Spawn the real agent loop against `model` and `tools`, returning the
/// run handle plus a subscriber attached *before* the loop starts so no
/// event can be missed.
pub async fn spawn_agent(
    model: ScriptedModel,
    tools: Vec<Box<dyn oct_agent::tools::AgentTool>>,
    pool: sqlx::SqlitePool,
    conv_id: &str,
    user_message: &str,
) -> (RunHandle, broadcast::Receiver<AgentEvent>) {
    let (event_tx, _) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let handle = RunHandle { event_tx, cancel };
    let rx = handle.subscribe();

    let ctx = AgentContext {
        model: Arc::new(model),
        tools: Arc::new(tools),
        pool,
        system_prompt: "test system prompt".to_string(),
    };

    let conv = conv_id.to_string();
    let run = handle.clone();
    let user = user_message.to_string();
    tokio::spawn(async move {
        run_agent_loop(
            ctx,
            conv,
            vec![oct_llm_provider::core::Message::text(Role::User, user)],
            run,
        )
        .await;
    });

    (handle, rx)
}

/// Receive events until `stop` matches (inclusive) or the channel closes.
/// A generous timeout turns a hung loop into a test failure instead of a
/// stuck test binary; no sleeps are used anywhere — every wait is parked on
/// an event.
pub async fn collect_events(
    rx: &mut broadcast::Receiver<AgentEvent>,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for agent event (loop hung?)")
            .expect("agent event channel closed before a terminal event");
        let done = stop(&event);
        out.push(event);
        if done {
            return out;
        }
    }
}

/// Terminal events: after one of these the agent loop has returned.
pub fn is_terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Finish | AgentEvent::Cancelled | AgentEvent::Error(_)
    )
}

// --- Full-stack e2e harness ------------------------------------------------

/// A real oct-agent HTTP server on a random port, backed by its own
/// migrated SQLite file inside a temp directory. Tests talk to it purely
/// over HTTP (reqwest); the LLM behind a provider row points at an
/// [`llm::LlmMock`]. Everything between the two is production code.
pub struct TestApp {
    pub dir: tempfile::TempDir,
    pub base_url: String,
    /// Handle onto the server's own database: lets tests seed persisted
    /// state (e.g. an interrupted turn's dangling history) directly.
    pub pool: sqlx::SqlitePool,
    http: reqwest::Client,
}

impl TestApp {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_url = format!("sqlite://{}", dir.path().join("test.db").display());
        let pool = db::init_pool(&db_url).await.expect("init test database");

        let state = oct_agent::api::AppState {
            pool: pool.clone(),
            registry: Arc::new(default_registry()),
            sessions: Arc::new(dashmap::DashMap::new()),
        };
        let app = oct_agent::api::build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test app");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test app server");
        });

        Self {
            dir,
            base_url: format!("http://127.0.0.1:{port}"),
            pool,
            http: reqwest::Client::new(),
        }
    }

    pub async fn post_json(&self, path: &str, body: impl serde::Serialize) -> reqwest::Response {
        self.http
            .post(format!("{}{path}", self.base_url))
            .json(&body)
            .send()
            .await
            .expect("request should not fail at transport level")
    }

    pub async fn get_json(&self, path: &str) -> reqwest::Response {
        self.http
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .expect("request should not fail at transport level")
    }

    pub async fn patch_json(&self, path: &str, body: impl serde::Serialize) -> reqwest::Response {
        self.http
            .patch(format!("{}{path}", self.base_url))
            .json(&body)
            .send()
            .await
            .expect("request should not fail at transport level")
    }

    pub async fn http_delete(&self, path: &str) -> reqwest::Response {
        self.http
            .delete(format!("{}{path}", self.base_url))
            .send()
            .await
            .expect("request should not fail at transport level")
    }

    /// POST /api/projects with the temp dir as working_dir; returns the id.
    pub async fn create_project(&self) -> String {
        let resp = self
            .post_json(
                "/api/projects",
                serde_json::json!({
                    "name": "e2e-project",
                    "working_dir": self.dir.path().to_string_lossy(),
                }),
            )
            .await;
        assert_eq!(resp.status(), 201, "project creation should succeed");
        resp.json::<serde_json::Value>()
            .await
            .expect("project json")["id"]
            .as_str()
            .expect("project id")
            .to_string()
    }

    /// POST /api/providers pointing at `llm`; the chat endpoint resolves
    /// custom providers by `provider_id:model_id`.
    pub async fn register_llm_provider(&self, llm: &llm::LlmMock) -> String {
        let resp = self
            .post_json(
                "/api/providers",
                serde_json::json!({
                    "id": "mock-llm",
                    "name": "Mock LLM",
                    "base_url": llm.url,
                    "api_key": "test-key",
                    "doc_url": null,
                }),
            )
            .await;
        assert_eq!(resp.status(), 201, "provider creation should succeed");
        "mock-llm".to_string()
    }

    /// POST /api/projects/{id}/conversations; returns the conversation id.
    pub async fn create_conversation(&self, project_id: &str) -> String {
        let resp = self
            .post_json(
                &format!("/api/projects/{project_id}/conversations"),
                serde_json::json!({"title": "e2e"}),
            )
            .await;
        assert_eq!(resp.status(), 201, "conversation creation should succeed");
        resp.json::<serde_json::Value>()
            .await
            .expect("conversation json")["id"]
            .as_str()
            .expect("conversation id")
            .to_string()
    }

    /// POST a chat message; returns the response with headers received but
    /// the SSE body NOT yet read (send() completes once headers arrive, so
    /// the caller decides when to drain the stream).
    pub async fn send_message(&self, conv_id: &str, content: &str) -> reqwest::Response {
        self.post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({
                "content": content,
                "provider_spec": "mock-llm:test-model",
            }),
        )
        .await
    }

    pub async fn cancel(&self, conv_id: &str) -> reqwest::Response {
        self.http
            .post(format!(
                "{}/api/conversations/{conv_id}/cancel",
                self.base_url
            ))
            .send()
            .await
            .expect("cancel request should not fail at transport level")
    }

    /// POST /api/conversations/{id}/resume; returns the response with headers
    /// received but the SSE body NOT yet read (same contract as
    /// [`Self::send_message`]).
    pub async fn resume(&self, conv_id: &str) -> reqwest::Response {
        self.http
            .post(format!(
                "{}/api/conversations/{conv_id}/resume",
                self.base_url
            ))
            .send()
            .await
            .expect("resume request should not fail at transport level")
    }

    /// Drain an SSE response body and return the JSON payload of every
    /// `data:` line. Comment lines (keep-alives) are skipped.
    pub async fn read_sse(&self, response: reqwest::Response) -> Vec<serde_json::Value> {
        let text = response.text().await.expect("sse body");
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|payload| {
                serde_json::from_str(payload)
                    .unwrap_or_else(|e| panic!("invalid sse payload {payload:?}: {e}"))
            })
            .collect()
    }
}
