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

pub mod anthropic_llm;
pub mod llm;
pub mod mcp;
pub mod mcp_http;
pub mod oauth_as;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use oct_agent::agent::events::SubscribeStream;
use oct_agent::agent::loop_runner::run_agent_loop;
use oct_agent::agent::{AgentContext, AgentEvent, EventHub, RunHandle};
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
    /// Scripted replies for non-streaming `generate` calls (title/summary
    /// style); each call pops one.
    generations: Mutex<VecDeque<Result<String, ModelError>>>,
    generate_requests: Mutex<Vec<ChatRequest>>,
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

    /// Every non-streaming `generate` request (e.g. the compaction summary
    /// call), in order.
    pub fn generate_requests(&self) -> Vec<ChatRequest> {
        self.state.generate_requests.lock().unwrap().clone()
    }
}

impl ScriptedModel {
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            state: Arc::new(ScriptedState {
                turns: Mutex::new(turns.into()),
                requests: Mutex::new(Vec::new()),
                generations: Mutex::new(VecDeque::new()),
                generate_requests: Mutex::new(Vec::new()),
            }),
            info: test_model_info(),
        }
    }

    pub fn observer(&self) -> ScriptedObserver {
        ScriptedObserver {
            state: self.state.clone(),
        }
    }

    /// Queue one scripted reply for the next non-streaming `generate` call.
    pub fn push_generation(&self, reply: Result<String, ModelError>) {
        self.state.generations.lock().unwrap().push_back(reply);
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

    async fn generate(&self, req: ChatRequest) -> Result<ChatResponse, ModelError> {
        self.state.generate_requests.lock().unwrap().push(req);
        let reply = self
            .state
            .generations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ModelError::unsupported("generate not scripted")));
        let text = reply?;
        Ok(ChatResponse {
            message: oct_llm_provider::core::Message::text(Role::Assistant, text),
            finish_reason: FinishReason::Stop,
            usage: None,
            provider_response_id: None,
            provider_metadata: Default::default(),
        })
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
            additional_dirs: None,
        },
    )
    .await
    .expect("create project");

    let conversation =
        db::conversations::create_conversation(pool, &project.id, Some("test")).await;
    conversation.expect("create conversation").id
}

/// Spawn the real agent loop against `model` and `tools`, returning the run
/// handle plus a subscriber attached *before* the loop starts so no event
/// can be missed.
pub async fn spawn_agent(
    model: ScriptedModel,
    tools: Vec<Box<dyn oct_agent::tools::AgentTool>>,
    pool: sqlx::SqlitePool,
    conv_id: &str,
    user_message: &str,
) -> (RunHandle, SubscribeStream) {
    let view = oct_agent::db::messages::LlmView {
        messages: vec![oct_llm_provider::core::Message::text(
            Role::User,
            user_message,
        )],
        // Synthetic views are not necessarily DB-backed; fabricated orderings
        // keep the parallel vector intact (only compaction reads them).
        orderings: vec![1],
        anchor: None,
    };
    spawn_agent_with(model, tools, pool, conv_id, view, None).await
}

/// [`spawn_agent`] with full control over the loaded view and the context
/// window (auto compaction is driven by the window: `None` disables it).
pub async fn spawn_agent_with(
    model: ScriptedModel,
    tools: Vec<Box<dyn oct_agent::tools::AgentTool>>,
    pool: sqlx::SqlitePool,
    conv_id: &str,
    view: oct_agent::db::messages::LlmView,
    context_window: Option<u32>,
) -> (RunHandle, SubscribeStream) {
    spawn_agent_full(model, tools, pool, conv_id, view, context_window, None).await
}

/// [`spawn_agent_with`] plus the idle duration for microcompact's idle entry.
pub async fn spawn_agent_full(
    model: ScriptedModel,
    tools: Vec<Box<dyn oct_agent::tools::AgentTool>>,
    pool: sqlx::SqlitePool,
    conv_id: &str,
    view: oct_agent::db::messages::LlmView,
    context_window: Option<u32>,
    idle: Option<Duration>,
) -> (RunHandle, SubscribeStream) {
    // The default harness gate is all-allowing: these suites drive the loop
    // directly and (unless a test passes its own gate through
    // [`spawn_agent_with_gate`]) do not exercise the permission layer.
    spawn_agent_with_gate(
        model,
        tools,
        pool,
        conv_id,
        view,
        context_window,
        idle,
        oct_agent::permissions::gate::PermissionGate::permissive(),
        oct_agent::modality::InputModalities::Unknown,
    )
    .await
}

/// [`spawn_agent_full`] with the run's permission gate and the model's input
/// modalities supplied by the test: permission suites park their
/// ScriptedModel turns on the gate's approval flow and answer through the
/// very registry the gate holds; modalities drive the request-time image
/// gate (`Unknown` = allow everything, the behavior without catalog data).
#[allow(clippy::too_many_arguments)]
// Supervisor/spawner entry: each argument is a distinct
// input of the operation; a param struct would only shuffle names.
pub async fn spawn_agent_with_gate(
    model: ScriptedModel,
    tools: Vec<Box<dyn oct_agent::tools::AgentTool>>,
    pool: sqlx::SqlitePool,
    conv_id: &str,
    view: oct_agent::db::messages::LlmView,
    context_window: Option<u32>,
    idle: Option<Duration>,
    permission: std::sync::Arc<oct_agent::permissions::gate::PermissionGate>,
    modalities: oct_agent::modality::InputModalities,
) -> (RunHandle, SubscribeStream) {
    let cancel = CancellationToken::new();
    let handle = RunHandle {
        hub: Arc::new(EventHub::new()),
        cancel,
        start_message_id: None,
    };
    let stream = handle.subscribe();

    let ctx = AgentContext {
        model: Arc::new(model),
        tools: Arc::new(
            tools
                .into_iter()
                .map(std::sync::Arc::from)
                .collect::<Vec<std::sync::Arc<dyn oct_agent::tools::AgentTool>>>(),
        ),
        pool,
        system_prompt: "test system prompt".to_string(),
        context_window,
        idle,
        reasoning: None,
        input_modalities: modalities,
        max_output_tokens: None,
        attachments: None,
        permission,
        prompt_env_refresh: None,
        prompt_generation: 0,
    };

    let conv = conv_id.to_string();
    let run = handle.clone();
    tokio::spawn(async move {
        run_agent_loop(ctx, conv, view, run).await;
    });

    (handle, stream)
}

/// Receive events until `stop` matches (inclusive) or the stream ends. A
/// generous timeout turns a hung loop into a test failure instead of a
/// stuck test binary; no sleeps are used anywhere — every wait is parked on
/// an event.
pub async fn collect_events(
    stream: &mut (impl futures_util::Stream<Item = AgentEvent> + Unpin),
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("timed out waiting for agent event (loop hung?)")
            .expect("agent event stream ended before a terminal event");
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
    /// The very `Arc<McpManager>` the server's AppState holds: lets tests
    /// inject scripted MCP connections (see [`mcp::FakeMcpConnection`])
    /// instead of spawning real server processes.
    pub mcp: Arc<oct_agent::mcp::McpManager>,
    /// The very `Arc<BackgroundTaskManager>` the server's RunService holds:
    /// lets tests inspect the active-task registry and shrink the output
    /// limit for watchdog coverage.
    pub background: Arc<oct_agent::runtime::background::BackgroundTaskManager>,
    /// The server's shared store, including publication/sweep coordination.
    pub attachments: Arc<oct_agent::attachments::AttachmentService>,
    http: reqwest::Client,
}

/// Wire-log test support: the sink and subscriber are process-global
/// (tracing), so both install ONCE per test binary — the first caller's
/// process-level temp dir wins and later TestApps share it. Assertions
/// never rely on file isolation: every line carries its conversation /
/// request ids, so shared files are filtered, not partitioned.
fn wire_test_setup() -> oct_llm_provider::wire::WireLogControl {
    static WIRE: std::sync::OnceLock<oct_llm_provider::wire::WireLogControl> =
        std::sync::OnceLock::new();
    WIRE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("oct-wire-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = oct_llm_provider::wire::init(dir, true);
        use tracing_subscriber::prelude::*;
        let _ = tracing_subscriber::registry()
            .with(oct_llm_provider::wire::WireLogLayer)
            .try_init();
        control
    })
    .clone()
}

impl TestApp {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        Self::new_with_dir(dir).await
    }

    /// [`TestApp::new`] on an EXISTING temp directory: the server boots
    /// against whatever `test.db` state the directory already carries
    /// (migrations re-run harmlessly on a fresh file). Lets a test simulate
    /// a process restart against the same persisted database — the new
    /// server's `runtime::bootstrap` sweep is part of what is under test.
    pub async fn new_with_dir(dir: tempfile::TempDir) -> Self {
        let db_url = format!("sqlite://{}", dir.path().join("test.db").display());
        let pool = db::init_pool(&db_url).await.expect("init test database");

        // Same startup sequence as run_server: sweep interrupted runs
        // before the server accepts traffic.
        oct_agent::runtime::bootstrap(&pool)
            .await
            .expect("bootstrap");

        // Integration tests run in direct tool mode unless a test opts into
        // code mode explicitly. The app-level default (`codemode::mode`) is
        // `code`, which would otherwise make every test's behavior depend on
        // whether a built oct-codemode-worker happens to sit next to the
        // test binary.
        oct_agent::db::settings::set(
            &pool,
            oct_agent::codemode::SETTING,
            oct_agent::codemode::McpToolMode::Direct.as_str(),
        )
        .await
        .expect("pin direct tool mode");

        let mcp = Arc::new(oct_agent::mcp::McpManager::default());
        let sessions: Arc<dashmap::DashMap<String, oct_agent::agent::RunHandle>> =
            Arc::new(dashmap::DashMap::new());
        // Same two-phase wire as run_server (RunService owns the manager; the
        // manager reaches back through a OnceLock). Background task output
        // files live under the temp dir, mirroring the "next to the
        // database" production placement.
        let subagents = Arc::new(oct_agent::subagent::SubagentManager::default());
        let background = Arc::new(oct_agent::runtime::background::BackgroundTaskManager::new(
            pool.clone(),
            dir.path().join("exec"),
        ));
        let attachments = Arc::new(oct_agent::attachments::AttachmentService::new(
            dir.path().join("attachments"),
            pool.clone(),
        ));
        let runs = Arc::new(oct_agent::runtime::RunService {
            lifecycle: Default::default(),
            pool: pool.clone(),
            registry: Arc::new(default_registry()),
            mcp: mcp.clone(),
            sessions: sessions.clone(),
            file_locks: dashmap::DashMap::new(),
            subagents: subagents.clone(),
            background: background.clone(),
            attachments: attachments.clone(),
            approvals: Arc::new(oct_agent::permissions::registry::ApprovalRegistry::new()),
            gates: Arc::new(dashmap::DashMap::new()),
            prompt_envs: Arc::new(dashmap::DashMap::new()),
            prompt_env_locks: dashmap::DashMap::new(),
        });
        subagents.set_service(runs.clone());
        background.set_service(runs.clone());
        let state = oct_agent::api::AppState {
            runs,
            pool: pool.clone(),
            registry: Arc::new(default_registry()),
            sessions,
            mcp: mcp.clone(),
            oauth_flows: Arc::new(oct_agent::oauth::OAuthFlowStore::default()),
            wire_log: wire_test_setup(),
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
            mcp,
            background,
            attachments,
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

    /// PUT variant of [`Self::patch_json`] (the provider update route).
    pub async fn put_json(&self, path: &str, body: impl serde::Serialize) -> reqwest::Response {
        self.http
            .put(format!("{}{path}", self.base_url))
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

    /// POST multipart/form-data (the attachment upload endpoint).
    pub async fn post_multipart(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> reqwest::Response {
        self.http
            .post(format!("{}{path}", self.base_url))
            .multipart(form)
            .send()
            .await
            .expect("request should not fail at transport level")
    }

    /// GET returning the raw response (non-JSON endpoints, e.g. attachment
    /// bytes with their own content type).
    pub async fn get_raw(&self, path: &str) -> reqwest::Response {
        self.http
            .get(format!("{}{path}", self.base_url))
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

    /// PATCH a conversation's permission mode. Suites that
    /// predate the permission layer pin their conversations to `bypass` so
    /// their model-driven writes/executions keep flowing without approval
    /// interactions; permission suites use this to build the mode under
    /// test.
    pub async fn set_permission_mode(&self, project_id: &str, conv_id: &str, mode: &str) {
        let resp = self
            .patch_json(
                &format!("/api/projects/{project_id}/conversations/{conv_id}"),
                serde_json::json!({"permission_mode": mode}),
            )
            .await;
        assert_eq!(resp.status(), 200, "setting permission mode should succeed");
    }

    /// POST a chat message; returns the response with headers received but
    /// the SSE body NOT yet read (send() completes once headers arrive, so
    /// the caller decides when to drain the stream).
    pub async fn send_message(&self, conv_id: &str, content: &str) -> reqwest::Response {
        self.send_message_with(conv_id, content, "mock-llm:test-model")
            .await
    }

    /// [`Self::send_message`] with an explicit provider_spec (tests that
    /// drive other adapter families, e.g. an 'anthropic' row).
    pub async fn send_message_with(
        &self,
        conv_id: &str,
        content: &str,
        provider_spec: &str,
    ) -> reqwest::Response {
        self.post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({
                "content": content,
                "provider_spec": provider_spec,
            }),
        )
        .await
    }

    /// [`Self::send_message_with`] 加思考档位（reasoning 端到端测试用）。
    pub async fn send_message_reasoning(
        &self,
        conv_id: &str,
        content: &str,
        provider_spec: &str,
        reasoning: serde_json::Value,
    ) -> reqwest::Response {
        self.post_json(
            &format!("/api/conversations/{conv_id}/messages"),
            serde_json::json!({
                "content": content,
                "provider_spec": provider_spec,
                "reasoning": reasoning,
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

    /// POST /api/conversations/{id}/compact — manual compaction.
    pub async fn compact(&self, conv_id: &str) -> reqwest::Response {
        self.http
            .post(format!(
                "{}/api/conversations/{conv_id}/compact",
                self.base_url
            ))
            .send()
            .await
            .expect("compact request should not fail at transport level")
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

    /// GET /api/conversations/{id}/run — the reconnect probe.
    pub async fn run_status(&self, conv_id: &str) -> reqwest::Response {
        self.get_json(&format!("/api/conversations/{conv_id}/run"))
            .await
    }

    /// GET /api/conversations/{id}/events — reconnect to a running agent;
    /// returns the response with headers received but the SSE body NOT yet
    /// read (same contract as [`Self::send_message`]). The server-side
    /// subscription is registered by the time headers arrive, which makes
    /// the subsequent replay content deterministic.
    pub async fn run_events(&self, conv_id: &str) -> reqwest::Response {
        self.get_json(&format!("/api/conversations/{conv_id}/events"))
            .await
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

/// Incrementally read SSE `data:` payloads from an open-ended response body
/// as they arrive (keep-alive comments skipped). Unlike
/// [`TestApp::read_sse`] this does not wait for body EOF, so a test can
/// observe the events a stream has produced SO FAR while the run is still
/// parked (e.g. behind an [`llm::gate`]).
pub fn sse_payloads(
    response: reqwest::Response,
) -> futures_util::stream::BoxStream<'static, serde_json::Value> {
    use futures_util::StreamExt;
    Box::pin(async_stream::stream! {
        let mut body = response.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.expect("sse transport error");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim_end_matches(['\n', '\r']);
                if let Some(payload) = line.strip_prefix("data: ") {
                    if payload == "[DONE]" {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str(payload) {
                        yield value;
                    }
                }
            }
        }
    })
}
