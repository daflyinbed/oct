mod atomic_file;
pub(crate) mod process;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;
use utoipa::ToSchema;

use crate::agent::AgentEvent;
use crate::agent::events::EventHub;
use tokio_util::sync::CancellationToken;

pub mod dispatch;
pub mod edit;
pub mod execute;
pub mod glob;
pub mod grep;
pub mod listdir;
pub mod read;
pub mod search_common;
pub mod shared;
pub mod task_stop;
pub mod truncate;
pub mod workspace;
pub mod write;

/// One image attached to a tool result (e.g. an MCP server's image content
/// block), passed through to the model alongside the text content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ToolImage {
    /// MIME type of the image, e.g. `image/png`.
    pub mime_type: String,
    /// Base64 from a tool, or an immutable oct-attachment:// reference after
    /// ingress normalization. Only base64 crosses the provider boundary.
    pub data: String,
}

impl From<ToolImage> for oct_llm_provider::core::ToolResultImage {
    fn from(image: ToolImage) -> Self {
        Self {
            mime_type: image.mime_type,
            data: image.data,
        }
    }
}

/// Output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Model-visible content (post-truncation). This is the only part that
    /// is sent to the LLM (via parts_json).
    pub content: String,
    pub is_error: bool,
    /// Images attached to this result, delivered to the LLM as multimodal
    /// tool-result content. Not subject to the text truncation budget.
    /// Empty for text-only tools; skipped on the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
    /// UI-only structured metadata. NEVER persisted into parts_json and
    /// never sent to the LLM; it lives in the messages.details_json column
    /// and in ToolResult events for the frontend.
    pub details: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
            details: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
            details: None,
        }
    }

    /// Success with UI-only structured details attached.
    pub fn success_with_details(content: impl Into<String>, details: impl Serialize) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
            details: serde_json::to_value(details).ok(),
        }
    }

    /// Error with UI-only structured details attached.
    pub fn error_with_details(content: impl Into<String>, details: impl Serialize) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
            details: serde_json::to_value(details).ok(),
        }
    }

    /// Attach model-visible images to this output (chainable). Mirrors
    /// `is_error`/`details` semantics for success and error outputs alike.
    pub fn with_images(mut self, images: Vec<ToolImage>) -> Self {
        self.images = images;
        self
    }
}

/// Which of a command's output streams a delta belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum OutputStream {
    #[serde(rename = "stdout")]
    Stdout,
    #[serde(rename = "stderr")]
    Stderr,
}

/// Flush a stream's buffer as one delta event once it reaches this size.
const SMOOTHER_FLUSH_BYTES: usize = 8192;
/// Or once this much time has passed since the stream's last flush. The time
/// condition is evaluated on output and by waiting process-pipe readers.
const SMOOTHER_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// Hard cap on delta events per tool call. Further output is dropped
/// silently; the final ToolResult content is authoritative anyway.
const SMOOTHER_MAX_EVENTS: usize = 2000;

/// Per-stream smoother state: buffered text awaiting emission plus the
/// clock for the time-based flush trigger.
#[derive(Default)]
struct StreamBuffer {
    pending: String,
    last_flush: Option<Instant>,
}

/// Interior state of [`ToolContext`] (shared across `Arc` clones).
#[derive(Default)]
struct SmootherState {
    stdout: StreamBuffer,
    stderr: StreamBuffer,
    /// Delta events emitted so far across both streams.
    events_emitted: usize,
}

impl SmootherState {
    fn buffer_mut(&mut self, stream: OutputStream) -> &mut StreamBuffer {
        match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        }
    }

    /// Emit `stream`'s pending buffer as ONE `ToolOutputDelta` event (when
    /// non-empty and the event cap allows) and reset its flush clock.
    fn flush_stream(&mut self, call_id: &str, hub: &EventHub, stream: OutputStream) {
        if self.events_emitted >= SMOOTHER_MAX_EVENTS {
            // Over the cap: drop the buffered remainder without emitting.
            self.buffer_mut(stream).pending.clear();
            return;
        }
        let buffer = self.buffer_mut(stream);
        if buffer.pending.is_empty() {
            return;
        }
        let delta = std::mem::take(&mut buffer.pending);
        buffer.last_flush = Some(Instant::now());
        self.events_emitted += 1;
        hub.publish(AgentEvent::ToolOutputDelta {
            call_id: call_id.to_string(),
            stream,
            delta,
        });
    }
}

struct ToolContextInner {
    call_id: String,
    /// Outer model call owning this operation in the persisted timeline.
    timeline_call_id: String,
    hub: Arc<EventHub>,
    state: Mutex<SmootherState>,
    /// The run's cancellation token: long-running tools (execute, MCP) race
    /// it against their own completion so a cancelled run stops them at once
    /// instead of letting them run to their timeout.
    cancel: CancellationToken,
}

/// Per-call handle given to a tool so it can stream live output to the
/// frontend. Cheaply cloneable (Arc inner); the smoother state uses interior
/// mutability so reader tasks can emit without external locking.
///
/// Deltas are NOT persisted and never reach the LLM; the final ToolResult
/// remains the authoritative record.
#[derive(Clone)]
pub struct ToolContext {
    inner: Arc<ToolContextInner>,
}

impl ToolContext {
    pub fn new(call_id: impl Into<String>, hub: Arc<EventHub>, cancel: CancellationToken) -> Self {
        let call_id = call_id.into();
        Self {
            inner: Arc::new(ToolContextInner {
                timeline_call_id: call_id.clone(),
                call_id,
                hub,
                state: Mutex::new(SmootherState::default()),
                cancel,
            }),
        }
    }

    pub fn call_id(&self) -> &str {
        &self.inner.call_id
    }

    pub fn timeline_call_id(&self) -> &str {
        &self.inner.timeline_call_id
    }

    pub(crate) fn nested(&self, call_id: String) -> Self {
        Self {
            inner: Arc::new(ToolContextInner {
                call_id,
                timeline_call_id: self.timeline_call_id().to_owned(),
                hub: self.hub(),
                state: Mutex::new(SmootherState::default()),
                cancel: self.cancel_token().clone(),
            }),
        }
    }

    pub fn hub(&self) -> Arc<EventHub> {
        self.inner.hub.clone()
    }

    /// The run's cancellation token. Tools with unbounded or long waits
    /// (execute, MCP calls) should race [`CancellationToken::cancelled`]
    /// against their work and return a cancelled ToolOutput on it.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    /// Append live output text and flush lazily: a stream's buffer is sent
    /// as one `ToolOutputDelta` when it reaches [`SMOOTHER_FLUSH_BYTES`] or
    /// when [`SMOOTHER_FLUSH_INTERVAL`] has elapsed since its last flush
    /// (evaluated at emit time). Slow output therefore flushes immediately
    /// while bursty output is coalesced into ~10 events/sec or 8KB chunks.
    pub fn emit_output_delta(&self, stream: OutputStream, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.events_emitted >= SMOOTHER_MAX_EVENTS {
            // Over the event cap: drop the delta silently.
            return;
        }
        let buffer = state.buffer_mut(stream);
        buffer.pending.push_str(text);
        if buffer.last_flush.is_none() {
            // Start the flush clock at the first output, so even a single
            // small burst never emits more than ~10 events/sec.
            buffer.last_flush = Some(Instant::now());
        }
        let due = buffer.pending.len() >= SMOOTHER_FLUSH_BYTES
            || buffer
                .last_flush
                .is_some_and(|t| t.elapsed() >= SMOOTHER_FLUSH_INTERVAL);
        if due {
            state.flush_stream(&self.inner.call_id, &self.inner.hub, stream);
        }
    }

    /// Pipe readers call this while waiting for more bytes, so a short burst
    /// does not remain buffered until another write or process exit.
    fn flush_due(&self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            if state
                .buffer_mut(stream)
                .last_flush
                .is_some_and(|t| t.elapsed() >= SMOOTHER_FLUSH_INTERVAL)
            {
                state.flush_stream(&self.inner.call_id, &self.inner.hub, stream);
            }
        }
    }

    /// Flush any buffered remainder of both streams as events (respecting
    /// the event cap). Idempotent; called by the agent loop before the final
    /// ToolResult so the frontend has seen all output when it arrives.
    pub fn flush(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            state.flush_stream(&self.inner.call_id, &self.inner.hub, stream);
        }
    }
}

/// Trait that all agent tools implement.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Stable configuration identity; MCP tools override this with the server
    /// id and original name, independent of the model-visible alias.
    fn selector(&self) -> crate::agent_settings::ToolSelector {
        crate::agent_settings::ToolSelector::Builtin {
            name: self.name().to_string(),
        }
    }

    /// Tool name as used by the LLM.
    fn name(&self) -> &str;

    /// Authorization identity for the permission gate: the CANONICAL name
    /// rules are written against, independent of any model-facing alias.
    /// Aliasing wrappers (frozen MCP exposure names) override this so a
    /// server rename cannot strand a `deny`/`allow` rule written against
    /// the current name.
    fn authz_name(&self) -> &str {
        self.name()
    }

    /// Description shown to the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> serde_json::Value;

    /// Host-side schema validation. Builtins deserialize typed arguments;
    /// MCP validates its immutable schema before either dispatch path.
    fn validate_input(&self, _args: &serde_json::Value) -> Result<()> {
        Ok(())
    }

    /// Diagnostics for a cancelled call after its owned work has been dropped.
    fn interrupted_output(&self, _call_id: &str) -> Option<ToolOutput> {
        None
    }

    /// Optional structured-output contract (MCP outputSchema).
    fn output_schema(&self) -> Option<serde_json::Value> {
        None
    }

    fn mcp_server_name(&self) -> Option<&str> {
        None
    }

    /// Code mode receives the native tool's bounded text and images. UI details
    /// never cross this boundary; MCP overrides it with pre-truncation data.
    async fn execute_for_code(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<CodeToolResult> {
        let out = self.execute(args, ctx).await?;
        Ok(CodeToolResult {
            text: out.content,
            is_error: out.is_error,
            images: out.images,
            structured_content: None,
        })
    }

    /// Short human-readable title for one specific call, shown in the UI
    /// (e.g. the command being run). Best-effort from the raw args; falls
    /// back to the tool name when the interesting field is missing or the
    /// args are unparseable.
    fn title(&self, args: &serde_json::Value) -> String {
        let _ = args;
        self.name().to_string()
    }

    /// Execute the tool with the given JSON arguments. `ctx` allows the tool
    /// to stream live output to the frontend; tools that do not stream
    /// simply ignore it.
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeToolResult {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<serde_json::Value>,
    pub images: Vec<ToolImage>,
    pub is_error: bool,
}

/// Convert an `AgentTool` to the oct-llm-provider `ToolSpec` for LLM requests.
pub fn to_tool_spec(tool: &dyn AgentTool) -> oct_llm_provider::core::ToolSpec {
    oct_llm_provider::core::ToolSpec {
        name: tool.name().to_string(),
        description: Some(tool.description().to_string()),
        input_schema: tool.input_schema(),
    }
}

/// Create all default tools for a workspace's root directories: a fresh
/// lock set and a fresh read state for a single-agent run (no subagents).
///
/// The file-manipulating tools (read/write/edit) share one lock set and one
/// per-agent read state so read-before-edit tracking and per-file locks work
/// across all of them within a single agent run.
pub fn default_tools(roots: workspace::WorkspaceRoots) -> Vec<Box<dyn AgentTool>> {
    default_tools_with(
        roots,
        Arc::new(shared::WorkspaceFileLocks::new()),
        Arc::new(shared::AgentReadState::new()),
    )
}

/// [`default_tools`] over caller-provided shared pieces: the locks are the
/// task-tree-shared per-file mutexes (a run service hands the SAME `Arc` to
/// a project's root run and every subagent), the read state is this agent's
/// own. Tool order matches [`default_tools`].
pub fn default_tools_with(
    roots: workspace::WorkspaceRoots,
    locks: Arc<shared::WorkspaceFileLocks>,
    reads: Arc<shared::AgentReadState>,
) -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(read::ReadFileTool::new(roots.clone(), reads.clone())),
        Box::new(listdir::ListDirTool::new(roots.clone())),
        Box::new(grep::GrepTool::new(roots.clone())),
        Box::new(glob::GlobTool::new(roots.clone())),
        Box::new(write::WriteFileTool::new(
            roots.clone(),
            locks.clone(),
            reads.clone(),
        )),
        Box::new(edit::EditFileTool::new(roots.clone(), locks, reads)),
        Box::new(execute::ExecuteCommandTool::new(roots)),
    ]
}

/// The three file-manipulating tools only, rebuilt over the shared locks and
/// a FRESH read state — the pieces a subagent's toolset swaps in so it gets
/// its own read records while the per-file locks stay task-tree-shared.
pub fn file_tools_with(
    roots: workspace::WorkspaceRoots,
    locks: Arc<shared::WorkspaceFileLocks>,
    reads: Arc<shared::AgentReadState>,
) -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(read::ReadFileTool::new(roots.clone(), reads.clone())),
        Box::new(write::WriteFileTool::new(
            roots.clone(),
            locks.clone(),
            reads.clone(),
        )),
        Box::new(edit::EditFileTool::new(roots, locks, reads)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// Build a ToolContext over a fresh hub and subscribe to it BEFORE any
    /// output is emitted, so every flushed delta is observed as it happens.
    ///
    /// Assertion model: publishing to the hub is synchronous, so after the
    /// actions under test we drop every hub owner (context + local Arc) — the
    /// subscription then reaches EOF and `collect()` returns exactly the
    /// events that were published, with no timeouts or sleeps.
    fn ctx_with_sub() -> (
        ToolContext,
        Arc<EventHub>,
        crate::agent::events::SubscribeStream,
    ) {
        let hub = Arc::new(EventHub::new());
        let stream = hub.subscribe();
        (
            ToolContext::new("call-1", hub.clone(), CancellationToken::new()),
            hub,
            stream,
        )
    }

    fn deltas(events: Vec<AgentEvent>) -> Vec<(OutputStream, String)> {
        events
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ToolOutputDelta { stream, delta, .. } => Some((stream, delta)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn smoother_emits_large_chunks_immediately() {
        let (ctx, hub, stream) = ctx_with_sub();

        let big = "x".repeat(SMOOTHER_FLUSH_BYTES);
        ctx.emit_output_delta(OutputStream::Stdout, &big);
        // No flush() call: the size threshold alone must have emitted it.
        drop(ctx);
        drop(hub);

        assert_eq!(
            deltas(stream.collect().await),
            vec![(OutputStream::Stdout, big)],
            "an 8KB chunk must flush right away"
        );
    }

    #[tokio::test]
    async fn smoother_coalesces_small_bursts_until_flush() {
        let (ctx, hub, stream) = ctx_with_sub();

        for i in 0..10 {
            ctx.emit_output_delta(OutputStream::Stdout, &format!("chunk-{i};"));
        }
        // Under both thresholds so far: exactly one event after the (idempotent)
        // flush proves the ten bursts coalesced and the second flush no-oped.
        ctx.flush();
        ctx.flush();
        drop(ctx);
        drop(hub);

        assert_eq!(
            deltas(stream.collect().await),
            vec![(
                OutputStream::Stdout,
                "chunk-0;chunk-1;chunk-2;chunk-3;chunk-4;chunk-5;chunk-6;chunk-7;chunk-8;chunk-9;"
                    .to_string()
            )]
        );
    }

    #[tokio::test]
    async fn smoother_flushes_streams_independently() {
        let (ctx, hub, stream) = ctx_with_sub();

        let big_out = "o".repeat(SMOOTHER_FLUSH_BYTES);
        // The 8KB stdout chunk flushes right away; the small stderr burst
        // stays buffered (below both thresholds) until the explicit flush.
        ctx.emit_output_delta(OutputStream::Stdout, &big_out);
        ctx.emit_output_delta(OutputStream::Stderr, "small-err");
        ctx.flush();
        drop(ctx);
        drop(hub);

        assert_eq!(
            deltas(stream.collect().await),
            vec![
                (OutputStream::Stdout, big_out),
                (OutputStream::Stderr, "small-err".to_string()),
            ],
            "stdout and stderr must flush in order, from independent buffers"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn smoother_flushes_slow_output_after_interval() {
        let (ctx, hub, stream) = ctx_with_sub();

        ctx.emit_output_delta(OutputStream::Stdout, "first ");

        // Simulate a slow producer: the next emit arrives after the flush
        // interval, so the time condition holds at emit time.
        tokio::time::advance(SMOOTHER_FLUSH_INTERVAL + Duration::from_millis(20)).await;
        ctx.emit_output_delta(OutputStream::Stdout, "second");
        drop(ctx);
        drop(hub);

        assert_eq!(
            deltas(stream.collect().await),
            vec![(OutputStream::Stdout, "first second".to_string())]
        );
    }

    #[tokio::test]
    async fn smoother_caps_total_delta_events() {
        let (ctx, hub, stream) = ctx_with_sub();

        let big = "x".repeat(SMOOTHER_FLUSH_BYTES);
        for _ in 0..(SMOOTHER_MAX_EVENTS + 5) {
            ctx.emit_output_delta(OutputStream::Stdout, &big);
        }
        // Past the cap even explicit flushes emit nothing.
        ctx.emit_output_delta(OutputStream::Stderr, "tail");
        ctx.flush();
        drop(ctx);
        drop(hub);

        let count = stream
            .collect::<Vec<AgentEvent>>()
            .await
            .into_iter()
            .filter(|e| matches!(e, AgentEvent::ToolOutputDelta { .. }))
            .count();
        assert_eq!(count, SMOOTHER_MAX_EVENTS);
    }
}
