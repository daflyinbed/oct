//! LLM wire logging: fine-grained, machine-processable records of every
//! LLM HTTP exchange — the exact request body and every framed SSE event —
//! as one JSON object per line (JSONL), one file per conversation.
//!
//! Layout:
//! * **Emission** (this crate): the adapters call the `log_*` helpers at
//!   their four HTTP entry points (`generate`/`stream` × anthropic /
//!   openai-compatible). Every event carries the full correlation set as
//!   plain event fields — no span-stack decoding needed downstream.
//! * **Correlation** (task-local): the host (oct-agent) wraps model calls
//!   with [`with_tags`] so each line names its conversation/run/purpose.
//!   A `try_stream!` body is polled by the consumer, NOT inside the
//!   wrapping scope — so `stream()` captures the tags into a [`WireCtx`]
//!   at call time and the generator passes that ctx to every emission.
//! * **Sink** (this crate, installed by the host): a custom
//!   `tracing` layer (target [`TARGET`], TRACE level) formats events into
//!   flat JSON and hands them to a dedicated writer thread. One append
//!   file per conversation id (`{dir}/{conversation_id}.jsonl`, 0600),
//!   `_orphan.jsonl` when no conversation tag is present. Emission is
//!   gated by a runtime toggle ([`WireLogControl`]) so a settings switch
//!   can flip it live; `OCT_LLM_WIRE_LOG=off` hard-disables it.
//! * **Retention**: 16 MiB per file with one rotated backup; a 256 MiB
//!   directory budget evicts oldest diagnostic records. A per-field byte cap (`OCT_LLM_WIRE_FIELD_MAX`, default 1 MiB, `0`
//!   disables) because serializing/escaping an embedded base64 payload
//!   happens on the emitting task. Auth headers are never logged.
//!
//! Event vocabulary (`kind`, closed set): `llm_request` (exact serialized
//! request body), `llm_sse_event` (`seq` + the complete framed event block
//! in `raw` — byte-faithful, reassembling by `seq` reproduces the
//! response), `llm_response` (non-streaming body), `llm_stream_end`
//! (`outcome` eof|error|dropped), `llm_error` (staged failure details).
//! Consumers must tolerate unknown fields and unknown kinds.

mod retention;

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::Context;

/// The single tracing target all wire events are emitted under. Filtering
/// stderr noise vs. the dedicated file sink both key off this string.
pub const TARGET: &str = "oct_llm_provider::wire";

/// Default per-field cap (bytes) for `body`/`raw`-style payload fields.
const DEFAULT_FIELD_CAP: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Correlation tags (host-installed)
// ---------------------------------------------------------------------------

/// Host-provided correlation fields stamped onto every wire line. The host
/// wraps whole agent runs (conversation/run identity) and each model call
/// (purpose) with [`with_tags`]; nested scopes replace the whole set, so an
/// inner site builds on [`current_tags`] rather than starting from scratch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub conversation_id: Option<String>,
    pub run_id: Option<String>,
    /// `root` | `subagent`.
    pub run_kind: Option<String>,
    /// `chat` | `compact` | `title`.
    pub purpose: Option<String>,
}

tokio::task_local! {
    static TAGS: Tags;
}

/// Run `fut` with these tags visible to wire logging (see [`Tags`]).
pub fn with_tags<F: std::future::Future>(
    tags: Tags,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TAGS.scope(tags, fut)
}

/// The innermost tags, or defaults when no scope is active (standalone
/// library use, provider crate tests — those events go to `_orphan.jsonl`).
pub fn current_tags() -> Tags {
    TAGS.try_with(|tags| tags.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Request context + emission helpers
// ---------------------------------------------------------------------------

/// Whether any installed subscriber would record a wire event right now.
/// Adapters check this before serializing request bodies, so a disabled
/// sink costs nothing beyond one dispatcher probe per call.
pub(crate) fn enabled() -> bool {
    tracing::enabled!(target: TARGET, Level::TRACE)
}

/// Everything one LLM HTTP attempt needs on its log lines. Cloned into
/// stream generators (see the module docs on why span scope is not enough).
#[derive(Clone)]
pub(crate) struct WireCtx {
    tags: Tags,
    request_id: String,
    provider: String,
    model: String,
    endpoint: String,
    started: Instant,
}

impl WireCtx {
    pub(crate) fn new(provider: &str, model: &str, endpoint: String) -> Self {
        Self {
            tags: current_tags(),
            request_id: uuid::Uuid::new_v4().to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint,
            started: Instant::now(),
        }
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// The shared event preamble: `kind` + the correlation envelope, followed
/// by the call site's own fields. Keeps every `log_*` helper's line shape
/// identical. `Option` values implement `Value` by recording nothing when
/// `None` — absent correlation keys are simply missing from the JSON line.
macro_rules! wire_event {
    ($ctx:expr, kind = $kind:expr, $($field:tt)*) => {
        tracing::trace!(
            target: TARGET,
            kind = $kind,
            conversation_id = $ctx.tags.conversation_id.as_deref(),
            run_id = $ctx.tags.run_id.as_deref(),
            run_kind = $ctx.tags.run_kind.as_deref(),
            purpose = $ctx.tags.purpose.as_deref(),
            request_id = %$ctx.request_id,
            attempt = 1u64,
            provider = %$ctx.provider,
            model = %$ctx.model,
            endpoint = %$ctx.endpoint,
            $($field)*
        )
    };
}

/// Byte-cap a payload field on a UTF-8 character boundary. `0` explicitly
/// means unlimited; an unparseable value keeps the default.
fn cap_field(raw: &str) -> (Cow<'_, str>, bool) {
    static CAP: OnceLock<usize> = OnceLock::new();
    let cap = *CAP.get_or_init(|| {
        match std::env::var("OCT_LLM_WIRE_FIELD_MAX").ok() {
            Some(raw) => match raw.trim().parse::<usize>() {
                // 0 = unlimited: usize::MAX makes the length check below a
                // no-op without a second code path.
                Ok(0) => usize::MAX,
                Ok(v) => v,
                Err(_) => DEFAULT_FIELD_CAP,
            },
            None => DEFAULT_FIELD_CAP,
        }
    });
    if raw.len() <= cap {
        return (Cow::Borrowed(raw), false);
    }
    let mut end = cap;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    (Cow::Borrowed(&raw[..end]), true)
}

pub(crate) fn log_request(ctx: &WireCtx, body: &str, is_stream: bool) {
    if !enabled() {
        return;
    }
    let (body, truncated) = cap_field(body);
    wire_event!(ctx, kind = "llm_request",
        stream = is_stream, body = %body, body_bytes = body.len() as u64, truncated = truncated);
}

pub(crate) fn log_sse(ctx: &WireCtx, seq: u64, raw: &str) {
    if !enabled() {
        return;
    }
    let (raw, truncated) = cap_field(raw);
    wire_event!(ctx, kind = "llm_sse_event",
        seq = seq, raw = %raw, raw_bytes = raw.len() as u64, truncated = truncated);
}

pub(crate) fn log_response(ctx: &WireCtx, status: u16, body: &str, duration_ms: u64) {
    if !enabled() {
        return;
    }
    let (body, truncated) = cap_field(body);
    wire_event!(ctx, kind = "llm_response",
        status = status as u64, body = %body, body_bytes = body.len() as u64,
        truncated = truncated, duration_ms = duration_ms);
}

pub(crate) fn log_stream_end(ctx: &WireCtx, outcome: &'static str, events: u64) {
    if !enabled() {
        return;
    }
    wire_event!(
        ctx,
        kind = "llm_stream_end",
        outcome = outcome,
        events = events,
        duration_ms = ctx.elapsed_ms()
    );
}

pub(crate) fn log_error(
    ctx: &WireCtx,
    stage: &'static str,
    status: Option<u16>,
    message: &str,
    body: Option<&str>,
) {
    if !enabled() {
        return;
    }
    // Both payload-ish inputs go through the field cap: error bodies ride
    // inside `message` on the status stage (check_response_status embeds
    // them), so an uncapped message would bypass the valve entirely.
    let (message, message_truncated) = cap_field(message);
    let (body_field, body_bytes, body_truncated) = match body {
        Some(raw) => {
            let (capped, truncated) = cap_field(raw);
            (Some(capped.into_owned()), raw.len() as u64, truncated)
        }
        None => (None, 0, false),
    };
    wire_event!(ctx, kind = "llm_error",
        stage = stage, status = status, message = %message,
        body = body_field.as_deref(), body_bytes = body_bytes,
        truncated = message_truncated || body_truncated,
        duration_ms = ctx.elapsed_ms());
}

/// Emits `llm_stream_end` exactly once per stream: normal/error paths call
/// [`finish`](Self::finish)/[`mark_error`](Self::mark_error); a stream
/// dropped mid-flight (cancellation) is caught by `Drop` as `dropped`.
/// Lives inside the `try_stream!` generator as a plain local — its counters
/// are `Cell`s so the generator body can bump them across yields.
pub(crate) struct StreamEndGuard {
    ctx: WireCtx,
    seq: Cell<u64>,
    events: Cell<u64>,
    outcome: Cell<&'static str>,
    settled: Cell<bool>,
}

impl StreamEndGuard {
    pub(crate) fn new(ctx: WireCtx) -> Self {
        Self {
            ctx,
            seq: Cell::new(0),
            events: Cell::new(0),
            outcome: Cell::new("dropped"),
            settled: Cell::new(false),
        }
    }

    /// Next 1-based `seq` for an SSE event.
    pub(crate) fn next_seq(&self) -> u64 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }

    /// Count one emitted SSE event (called alongside `next_seq`).
    pub(crate) fn count(&self) {
        self.events.set(self.events.get() + 1);
    }

    /// An explicit error is about to end the stream (its own `llm_error`
    /// is logged by the call site); `Drop` then reports `outcome=error`.
    pub(crate) fn mark_error(&self) {
        self.outcome.set("error");
    }

    /// The stream reached natural EOF.
    pub(crate) fn finish(&self) {
        self.settled.set(true);
        log_stream_end(&self.ctx, "eof", self.events.get());
    }
}

impl Drop for StreamEndGuard {
    fn drop(&mut self) {
        if !self.settled.get() {
            self.settled.set(true);
            log_stream_end(&self.ctx, self.outcome.get(), self.events.get());
        }
    }
}

// ---------------------------------------------------------------------------
// Sink: layer + writer thread + control
// ---------------------------------------------------------------------------

/// Shared sink state: the writer channel plus the runtime toggles. The
/// process-wide singleton is set up once by [`init`]; the layer and the
/// host-side control are cheap handles onto it.
struct Sink {
    tx: SyncSender<(Option<String>, String)>,
    dir: PathBuf,
    enabled: AtomicBool,
    /// `OCT_LLM_WIRE_LOG=off`: stronger than the settings toggle — flips
    /// are rejected until the next process start.
    hard_disabled: AtomicBool,
    dropped: AtomicU64,
}

static SINK: OnceLock<Arc<Sink>> = OnceLock::new();

/// Host-facing runtime switch for the wire sink ([`init`] hands one out;
/// the settings API flips it live).
#[derive(Clone)]
pub struct WireLogControl {
    sink: Arc<Sink>,
}

impl WireLogControl {
    pub fn set_enabled(&self, on: bool) {
        if self.sink.hard_disabled.load(Ordering::Relaxed) {
            return;
        }
        self.sink.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.sink.enabled.load(Ordering::Relaxed)
    }

    /// Pin the sink off for the rest of the process (`OCT_LLM_WIRE_LOG=off`):
    /// later [`set_enabled`](Self::set_enabled) calls are rejected — the env
    /// outranks the settings toggle until restart.
    pub fn hard_disable(&self) {
        self.sink.hard_disabled.store(true, Ordering::Relaxed);
        self.sink.enabled.store(false, Ordering::Relaxed);
    }

    /// Whether `OCT_LLM_WIRE_LOG=off` pinned the sink off for this process.
    pub fn hard_disabled(&self) -> bool {
        self.sink.hard_disabled.load(Ordering::Relaxed)
    }
}

/// Wire the sink up: create the directory, spawn the writer thread, seed
/// the toggle. Only the first call per process installs (later calls —
/// e.g. every test app — just get a control handle onto the existing
/// sink); `enabled` then only matters on that first call.
pub fn init(dir: PathBuf, enabled: bool) -> WireLogControl {
    if let Some(sink) = SINK.get() {
        return WireLogControl { sink: sink.clone() };
    }
    std::fs::create_dir_all(&dir).ok();
    let (tx, rx) = sync_channel::<(Option<String>, String)>(8192);
    let sink = Arc::new(Sink {
        tx,
        dir: dir.clone(),
        enabled: AtomicBool::new(enabled),
        hard_disabled: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
    });
    // If another thread won an init race, the loser's channel halves drop
    // UNUSED (no writer for them — the winner must stay the single writer)
    // and the loser's control binds to the winner's sink via the fallback.
    if SINK.set(sink.clone()).is_err() {
        let winner = SINK.get().expect("winner installed a sink").clone();
        return WireLogControl { sink: winner };
    }
    spawn_writer(sink.clone(), rx);
    WireLogControl { sink }
}

/// The tracing layer that turns wire events into JSONL lines and routes
/// them to the writer thread. Cheap to clone; add once per subscriber.
#[derive(Clone, Copy, Default)]
pub struct WireLogLayer;

impl<S: Subscriber> tracing_subscriber::Layer<S> for WireLogLayer {
    /// The enable state is RUNTIME state (the settings toggle). The trait's
    /// default caches `Interest::never` when `enabled` returns false at
    /// callsite registration — an event first fired while the sink is off
    /// would stay dead for the rest of the process even after re-enabling.
    /// Always re-ask instead; the check is a target compare + atomic load.
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> tracing_core::Interest {
        tracing_core::Interest::sometimes()
    }

    /// `Layer::enabled` is GLOBAL narrowing: a `false` here short-circuits
    /// the callsite for EVERY layer on the stack — including the
    /// human-facing fmt layer — so callsites outside the wire target are
    /// not ours to gate and must always answer `true` (the
    /// target/level/toggle narrowing for this layer's own recording lives
    /// in `on_event`). For wire callsites the sink toggle is the answer,
    /// and it doubles as the `tracing::enabled!` probe adapters use to
    /// skip request serialization while the sink is off.
    fn enabled(&self, metadata: &Metadata<'_>, _cx: Context<'_, S>) -> bool {
        metadata.target() != TARGET || wire_sink_enabled()
    }

    fn on_event(&self, event: &Event<'_>, _cx: Context<'_, S>) {
        let Some(sink) = SINK.get() else { return };
        let metadata = event.metadata();
        if metadata.target() != TARGET || *metadata.level() != Level::TRACE {
            return;
        }
        if !sink.enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut visitor = WireVisitor::default();
        event.record(&mut visitor);
        let conversation_id = visitor.conversation_id.clone();
        let line = visitor.finish();
        if sink.tx.try_send((conversation_id, line)).is_err() {
            sink.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn wire_sink_enabled() -> bool {
    SINK.get()
        .is_some_and(|sink| sink.enabled.load(Ordering::Relaxed))
}

// `body`/`raw` arrive via record_str as pre-serialized strings; serde_json
// then re-escapes them into the line, which is exactly the double-encoding
// the schema promises (`jq ... | fromjson` recovers the object).
#[derive(Default)]
struct WireVisitor {
    fields: Vec<(String, serde_json::Value)>,
    conversation_id: Option<String>,
    saw_kind: bool,
}

impl Visit for WireVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "conversation_id" {
            self.conversation_id = Some(value.to_string());
        }
        self.push(field, serde_json::Value::String(value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, serde_json::Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, serde_json::Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, serde_json::Value::String(format!("{value:?}")));
    }
}

impl WireVisitor {
    fn push(&mut self, field: &Field, value: serde_json::Value) {
        if field.name() == "kind" {
            self.saw_kind = true;
        }
        self.fields.push((field.name().to_string(), value));
    }

    /// Assemble the final JSON line: envelope first (`ts`/`ts_ms`/`level`/
    /// `target`), then the event fields in emission order.
    fn finish(mut self) -> String {
        let now = chrono::Utc::now();
        let mut line = String::with_capacity(256);
        line.push_str("{\"ts\":\"");
        line.push_str(&now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        line.push_str("\",\"ts_ms\":");
        line.push_str(&now.timestamp_millis().to_string());
        line.push_str(",\"level\":\"TRACE\",\"target\":\"");
        line.push_str(TARGET);
        line.push('"');
        if !self.saw_kind {
            line.push_str(",\"kind\":\"unknown\"");
        }
        for (name, value) in std::mem::take(&mut self.fields) {
            line.push(',');
            line.push_str(&serde_json::to_string(&name).expect("field names are valid JSON keys"));
            line.push(':');
            line.push_str(&serde_json::to_string(&value).expect("values serialize"));
        }
        line.push('}');
        line
    }
}

/// Append one file per conversation id. The name is sanitized down to a
/// filename-safe subset; anything without a conversation tag lands in
/// `_orphan.jsonl`, writer notices in `_meta.jsonl`.
fn conversation_file_name(conversation_id: &str) -> String {
    let mut name: String = conversation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.truncate(128);
    if name.is_empty() {
        name = "_orphan".to_string();
    }
    name
}

fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }
}

/// The single writer thread: owns every open file handle (no locking), one
/// syscall per line (O_APPEND keeps interleaving tear-free at this volume).
/// IO failures are counted and reported once per file name — a dead
/// directory or full disk must not turn into one stderr line per event.
/// On channel close it flushes and reports accumulated drops to `_meta.jsonl`.
fn spawn_writer(sink: Arc<Sink>, rx: Receiver<(Option<String>, String)>) {
    use std::io::Write;
    std::thread::Builder::new()
        .name("llm-wire-writer".into())
        .spawn(move || {
            let mut files: std::collections::HashMap<String, std::fs::File> =
                std::collections::HashMap::new();
            let mut reported: std::collections::HashSet<String> = std::collections::HashSet::new();
            let report_once = |reported: &mut std::collections::HashSet<String>,
                               label: &str,
                               err: &std::io::Error| {
                if reported.insert(label.to_string()) {
                    eprintln!("llm wire log: {label}: {err}");
                }
            };
            let mut total_bytes = retention::size(&sink.dir);
            while let Ok((conversation_id, line)) = rx.recv() {
                let next_bytes = line.len() as u64 + 1;
                if next_bytes > retention::FILE {
                    sink.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if retention::over_budget(total_bytes, next_bytes) {
                    files.clear(); // never keep writing through evicted descriptors
                    retention::prune(&sink.dir);
                    total_bytes = retention::size(&sink.dir);
                    if retention::over_budget(total_bytes, next_bytes) {
                        sink.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                let name = conversation_id
                    .as_deref()
                    .map(conversation_file_name)
                    .unwrap_or_else(|| "_orphan".to_string());
                let path = sink.dir.join(format!("{name}.jsonl"));
                if std::fs::metadata(&path)
                    .is_ok_and(|m| m.len().saturating_add(next_bytes) > retention::FILE)
                {
                    files.remove(&name);
                    let previous = sink.dir.join(format!("{name}.jsonl.1"));
                    let _ = std::fs::remove_file(&previous);
                    let _ = std::fs::rename(&path, previous);
                    total_bytes = retention::size(&sink.dir);
                }
                if files.len() >= 64 && !files.contains_key(&name) {
                    files.clear();
                }
                let file = match files.get_mut(&name) {
                    Some(file) => file,
                    None => {
                        let file = match open_append(&sink.dir.join(format!("{name}.jsonl"))) {
                            Ok(file) => file,
                            Err(err) => {
                                sink.dropped.fetch_add(1, Ordering::Relaxed);
                                report_once(
                                    &mut reported,
                                    &format!("cannot open {name}.jsonl"),
                                    &err,
                                );
                                continue;
                            }
                        };
                        files.insert(name.clone(), file);
                        files.get_mut(&name).expect("just inserted")
                    }
                };
                total_bytes = total_bytes.saturating_add(next_bytes);
                if let Err(err) = writeln!(file, "{line}") {
                    sink.dropped.fetch_add(1, Ordering::Relaxed);
                    report_once(&mut reported, &format!("cannot write {name}.jsonl"), &err);
                }
            }
            let dropped = sink.dropped.load(Ordering::Relaxed);
            if dropped > 0
                && let Ok(mut meta) = open_append(&sink.dir.join("_meta.jsonl"))
            {
                let _ = writeln!(meta, "{{\"kind\":\"llm_wire_drop\",\"dropped\":{dropped}}}");
            }
        })
        .expect("spawn llm-wire-writer thread");
}

/// Test/diagnostic accessors for the installed sink (used by integration
/// tests on both crates; not part of the emission API).
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// Directory the installed sink writes to (`None` before [`init`]).
    pub fn sink_dir() -> Option<PathBuf> {
        SINK.get().map(|sink| sink.dir.clone())
    }

    /// Lines dropped because the writer channel was full or a write failed.
    pub fn dropped_count() -> u64 {
        SINK.get()
            .map(|sink| sink.dropped.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_field_respects_char_boundary() {
        // '中' is 3 bytes; a cap landing mid-character must back off to the
        // boundary instead of panicking on slicing.
        let raw = "中".repeat(100);
        let (capped, truncated) = cap_field(&raw);
        assert!(!truncated);
        assert_eq!(capped.len(), raw.len());

        // raw is 300 bytes; ×4000 = 1.2 MiB, over the default cap. The cut
        // must land on a char boundary (a 3-byte-char string can only cut
        // at multiples of 3).
        let huge = raw.repeat(4_000);
        let (capped, truncated) = cap_field(&huge);
        assert!(truncated);
        assert!(capped.len() < huge.len());
        assert!(huge.is_char_boundary(capped.len()));
    }

    #[test]
    fn conversation_file_name_sanitizes() {
        assert_eq!(conversation_file_name("conv-123_abc"), "conv-123_abc");
        // 9 chars; everything outside [A-Za-z0-9_-] (incl. 'é') → '_'.
        assert_eq!(conversation_file_name("../é/../x"), "________x");
        assert_eq!(conversation_file_name(""), "_orphan");
    }
}
