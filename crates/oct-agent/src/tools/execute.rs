use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use super::truncate::{truncate_middle, MAX_TOOL_OUTPUT_BYTES};
use super::{AgentTool, OutputStream, ToolContext, ToolOutput};

/// Timeout applied when the caller does not request one.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Upper bound for a requested timeout.
const MAX_TIMEOUT_SECS: u64 = 300;
/// Per-stream (stdout/stderr) cap. Bytes past the cap are still read (to avoid
/// pipe backpressure blocking the child) but discarded.
const STREAM_CAP_BYTES: usize = 1024 * 1024;
/// How long to wait for the stdout/stderr reader tasks to finish once the child
/// is done before aborting them.
const IO_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for the child to disappear after a timeout kill before
/// giving up on reaping (a process in uninterruptible sleep can ignore even
/// SIGKILL until its syscall returns).
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// Size of the chunks read from the child's pipes.
const READ_CHUNK_SIZE: usize = 8192;
/// Conventional exit code reported when a command is killed for exceeding the
/// timeout (same as GNU coreutils `timeout`).
const TIMEOUT_EXIT_CODE: i32 = 124;

pub struct ExecuteCommandTool {
    working_dir: PathBuf,
}

impl ExecuteCommandTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

/// UI-only metadata about one command run. Attached to the ToolOutput as
/// `details`; never shown to the LLM.
#[derive(Debug, Serialize)]
pub struct ExecuteDetails {
    pub title: String,
    pub command: String,
    /// Effective (post-clamp) timeout in seconds.
    pub timeout_secs: u64,
    /// Raw exit code if the child exited normally; `None` when it was
    /// terminated by a signal (including the timeout kill) or unknown.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Wall time from spawn to completion, in milliseconds.
    pub wall_time_ms: u64,
    /// The model-visible content was truncated by `truncate_middle`.
    pub truncated: bool,
    /// Content byte length before the `truncate_middle` cut.
    pub original_bytes: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecuteCommandArgs {
    /// The shell command to execute
    command: String,
    /// Working directory for the command (relative to project root). Defaults to project root.
    working_dir: Option<String>,
    /// Timeout in seconds. Minimum 1, default 120, maximum 300.
    timeout_secs: Option<u64>,
}

/// Reader bookkeeping: the shared capped buffer plus the task draining the pipe.
type ReaderHandle = (Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>);

/// Append `chunk` to `buf` unless the cap is already reached. Bytes past the cap
/// are dropped, but the caller keeps reading to EOF so the child is never blocked
/// on a full pipe (modeled on codex's `append_capped`).
fn append_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if buf.len() >= cap {
        return;
    }
    let keep = chunk.len().min(cap - buf.len());
    buf.extend_from_slice(&chunk[..keep]);
}

/// Incrementally decode `pending` as UTF-8: return the longest decodable
/// prefix as text (replacing invalid bytes with U+FFFD, mirroring
/// `String::from_utf8_lossy`) and keep a trailing incomplete multi-byte char
/// in `pending` for the next chunk. A chunk boundary can split a char, so
/// only the valid prefix may be emitted now.
fn drain_decodable(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                out.push_str(text);
                pending.clear();
                return out;
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if let Ok(text) = std::str::from_utf8(&pending[..valid_up_to]) {
                    out.push_str(text);
                }
                if e.error_len().is_some() {
                    // A truly invalid sequence: emit ONE replacement char per
                    // maximal subpart (matching `from_utf8_lossy`, which the
                    // final content uses) and rescan the remainder.
                    let invalid_len = e.error_len().unwrap_or(1);
                    out.push('\u{FFFD}');
                    pending.drain(..valid_up_to + invalid_len);
                } else {
                    // Incomplete char at the end: keep it buffered until the
                    // next chunk arrives.
                    pending.drain(..valid_up_to);
                    return out;
                }
            }
        }
    }
}

/// Spawn a task that drains a child pipe into a shared capped buffer while
/// streaming every decoded byte (even past the collection cap) to the
/// frontend as live deltas. The buffer is shared so collected bytes survive
/// even if the task has to be aborted.
fn spawn_reader<R>(mut stream: R, stream_tag: OutputStream, ctx: ToolContext) -> ReaderHandle
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    let buf = Arc::new(Mutex::new(Vec::new()));
    let task_buf = buf.clone();

    let handle = tokio::spawn(async move {
        let mut chunk = [0u8; READ_CHUNK_SIZE];
        // Trailing bytes of the previous chunk that may start a multi-byte
        // UTF-8 char split across chunk boundaries.
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    {
                        let mut collected = task_buf
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        append_capped(&mut collected, &chunk[..n], STREAM_CAP_BYTES);
                    }
                    pending.extend_from_slice(&chunk[..n]);
                    let text = drain_decodable(&mut pending);
                    ctx.emit_output_delta(stream_tag, &text);
                }
                // Unrecoverable read error: stop and keep what we have so far.
                Err(_) => break,
            }
        }
        if !pending.is_empty() {
            // A partial char at EOF is invalid UTF-8; surface one replacement
            // char so the live stream matches `from_utf8_lossy` in the final
            // content.
            ctx.emit_output_delta(stream_tag, "\u{FFFD}");
        }
    });

    (buf, handle)
}

/// Wait up to `IO_DRAIN_TIMEOUT` for a reader task to finish; abort it if overdue
/// (e.g. a grandchild still holds the pipe open) and return whatever output was
/// collected either way.
async fn drain_reader(reader: Option<ReaderHandle>) -> Vec<u8> {
    let Some((buf, handle)) = reader else {
        return Vec::new();
    };
    tokio::pin!(handle);
    if tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut handle)
        .await
        .is_err()
    {
        // Abort and WAIT: the aborted task may be mid-emit of a live delta;
        // joining here prevents a stray delta from reaching the frontend
        // after the final ToolResult event.
        handle.abort();
        let _ = (&mut handle).await;
    }
    buf.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Kill the whole process tree on timeout. The child was spawned with
/// `process_group(0)`, so it leads its own group and pgid == child pid;
/// `killpg` takes down sh plus any descendants it spawned.
#[cfg(unix)]
fn kill_process_group(pgid: Option<u32>, child: &mut tokio::process::Child) {
    if let Some(pgid) = pgid {
        let ret = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        if ret != 0 {
            tracing::warn!(
                "killpg({pgid}, SIGKILL) failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    // Belt and braces: also kill the child directly in case the process group
    // could not be set up or signaled.
    let _ = child.start_kill();
}

/// Non-Unix fallback: no process groups available, kill the child directly.
#[cfg(not(unix))]
fn kill_process_group(_pgid: Option<u32>, child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Kills the child's process group when this scope ends. `kill_on_drop(true)`
/// only signals the `sh` process itself; backgrounded descendants share the
/// group and would otherwise survive a dropped future (agent cancelled
/// mid-execution) or a parent that exited before them. ESRCH when the group is
/// already gone is harmless.
struct GroupKillOnDrop(Option<u32>);

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            // Best effort; the group is usually already empty here.
            unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        }
        #[cfg(not(unix))]
        let _ = self.0;
    }
}

#[async_trait]
impl AgentTool for ExecuteCommandTool {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. \
         Commands run via `sh -c` on Unix. Use timeout_secs to limit execution time."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ExecuteCommandArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("execute_command")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: ExecuteCommandArgs =
            serde_json::from_value(args).context("Invalid arguments for execute_command")?;

        let cwd = if let Some(ref dir) = args.working_dir {
            match super::read::validate_path_within(&self.working_dir, dir) {
                Ok(p) => p,
                Err(output) => return Ok(output),
            }
        } else {
            self.working_dir.clone()
        };

        let timeout_secs = args
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            // Clamp to 1..=300: a zero duration would race the wait and could
            // kill the command instantly.
            .clamp(1, MAX_TIMEOUT_SECS);
        let timeout = Duration::from_secs(timeout_secs);

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Run the child in its own process group so the whole tree can be killed
        // on timeout (inherent tokio method, available on Unix since tokio 1.24).
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(c) => c,
            // Early error: the command never ran, so there are no details.
            Err(e) => return Ok(ToolOutput::error(format!("Failed to spawn command: {e}"))),
        };

        // Capture the pgid before anything else; the child leads its own process
        // group, so pgid == child.id().
        let pgid = child.id();

        // Take down the whole group when this scope ends (covers cancellation
        // and lingering background children; kill_on_drop only covers `sh`).
        let _group_kill = GroupKillOnDrop(pgid);

        // Live output streaming: each reader carries its stream tag and a
        // clone of the context (both clones share the smoother state).
        let stdout_reader = child
            .stdout
            .take()
            .map(|s| spawn_reader(s, OutputStream::Stdout, _ctx.clone()));
        let stderr_reader = child
            .stderr
            .take()
            .map(|s| spawn_reader(s, OutputStream::Stderr, _ctx.clone()));

        // Wait with timeout while keeping the child handle alive so it can be
        // killed on timeout (unlike the previous wait_with_output approach, which
        // consumed the child and left it running forever after a timeout).
        let mut timed_out = false;
        let wait_result = tokio::select! {
            status = child.wait() => Ok(status),
            _ = tokio::time::sleep(timeout) => {
                timed_out = true;
                kill_process_group(pgid, &mut child);
                // Bound the re-wait: a child stuck in uninterruptible sleep
                // could otherwise hang the tool forever even after SIGKILL.
                tokio::time::timeout(KILL_REAP_TIMEOUT, child.wait()).await
            }
        };

        // Raw exit code for the details struct: None when the child died by a
        // signal (e.g. the timeout kill), matching ExitStatus::code.
        let mut exit_code_detail: Option<i32> = None;
        // Set when the child ran but its status could not be reaped (rare).
        let mut wait_error: Option<String> = None;
        let exit_code = match wait_result {
            Ok(Ok(status)) => {
                exit_code_detail = status.code();
                status.code().unwrap_or(-1)
            }
            Ok(Err(e)) if timed_out => {
                // The post-kill wait failed; the command is dead either way, so
                // keep the timeout context and partial output instead of
                // dropping them.
                tracing::warn!("post-kill wait failed: {e}");
                -1
            }
            Ok(Err(e)) => {
                wait_error = Some(format!("Command failed: {e}"));
                -1
            }
            Err(_) => {
                tracing::warn!("child survived SIGKILL for {KILL_REAP_TIMEOUT:?}; giving up on reaping");
                -1
            }
        };

        // Decode once at the end for the final content (raw bytes may split a
        // UTF-8 char mid-chunk). Drained concurrently so the worst case is one
        // drain budget, not two.
        let (stdout_bytes, stderr_bytes) =
            tokio::join!(drain_reader(stdout_reader), drain_reader(stderr_reader));
        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

        // On timeout the real exit status is the kill signal; report the
        // conventional timeout exit code instead.
        let reported_exit_code = if timed_out {
            TIMEOUT_EXIT_CODE
        } else {
            exit_code
        };

        let mut result = String::new();
        if timed_out {
            result.push_str(&format!("Command timed out after {timeout_secs} seconds"));
        }
        if !stdout.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr]\n");
            result.push_str(&stderr);
        }
        if result.is_empty() {
            result = format!("(no output, exit code: {reported_exit_code})");
        } else {
            result.push_str(&format!("\n[exit code: {reported_exit_code}]"));
        }

        let original_bytes = result.len();
        let truncated = original_bytes > MAX_TOOL_OUTPUT_BYTES;
        let result = truncate_middle(&result, MAX_TOOL_OUTPUT_BYTES);

        // The command actually ran, so the result carries full details.
        let details = ExecuteDetails {
            title: args.command.clone(),
            command: args.command.clone(),
            timeout_secs,
            exit_code: exit_code_detail,
            timed_out,
            wall_time_ms: started.elapsed().as_millis() as u64,
            truncated,
            original_bytes,
        };

        if let Some(err) = wait_error {
            return Ok(ToolOutput::error_with_details(err, details));
        }
        if reported_exit_code == 0 {
            Ok(ToolOutput::success_with_details(result, details))
        } else {
            Ok(ToolOutput::error_with_details(result, details))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use serde_json::Value;

    use crate::agent::events::EventHub;

    /// A ToolContext wired to a throwaway event hub so tests can observe the
    /// emitted live-delta events (publishing is synchronous, so subscribing
    /// after the run reads the full snapshot).
    struct TestCtx {
        ctx: ToolContext,
        hub: Arc<EventHub>,
    }

    impl TestCtx {
        fn new() -> Self {
            let hub = Arc::new(EventHub::new());
            Self {
                ctx: ToolContext::new("test-call", hub.clone()),
                hub,
            }
        }
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(
                json!({ "command": "echo hello" }),
                &TestCtx::new().ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.ends_with("[exit code: 0]"));
    }

    #[tokio::test]
    async fn reports_stderr_and_nonzero_exit() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(
                json!({ "command": "echo oops >&2; exit 3" }),
                &TestCtx::new().ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[stderr]\noop"));
        assert!(out.content.contains("[exit code: 3]"));

        let details = out.details.expect("command ran, details expected");
        assert_eq!(details["exit_code"], json!(3));
        assert_eq!(details["timed_out"], json!(false));
    }

    #[tokio::test]
    async fn zero_timeout_is_floored_to_one_second() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(
                json!({ "command": "echo hi", "timeout_secs": 0 }),
                &TestCtx::new().ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hi"));

        // The details report the effective (post-clamp) timeout.
        assert_eq!(out.details.unwrap()["timeout_secs"], json!(1));
    }

    #[tokio::test]
    async fn timeout_kills_process_group_and_reports_124() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let started = std::time::Instant::now();
        // The backgrounded grandchild holds the pipe open; only a process-group
        // kill (not just killing sh) makes this return promptly.
        let out = tool
            .execute(
                json!({
                    "command": "sleep 30 & sleep 30",
                    "timeout_secs": 1
                }),
                &TestCtx::new().ctx,
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(out.is_error);
        assert!(out.content.starts_with("Command timed out after 1 seconds"));
        assert!(out.content.contains("[exit code: 124]"));

        let details = out.details.unwrap();
        assert_eq!(details["timed_out"], json!(true));
        // Killed by a signal: no raw exit code, only the synthetic 124 in
        // the model-visible content.
        assert_eq!(details["exit_code"], Value::Null);
        assert_eq!(details["timeout_secs"], json!(1));
    }

    #[tokio::test]
    async fn details_describe_a_successful_run() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(json!({ "command": "echo hi" }), &TestCtx::new().ctx)
            .await
            .unwrap();

        let details = out.details.unwrap();
        assert_eq!(details["title"], json!("echo hi"));
        assert_eq!(details["command"], json!("echo hi"));
        assert_eq!(details["exit_code"], json!(0));
        assert_eq!(details["timed_out"], json!(false));
        assert_eq!(details["timeout_secs"], json!(DEFAULT_TIMEOUT_SECS));
        assert_eq!(details["truncated"], json!(false));
        assert_eq!(details["original_bytes"], json!(out.content.len()));
        assert!(
            details["wall_time_ms"].as_u64().unwrap() < 60_000,
            "wall time must be milliseconds, not nanos/second mismatches"
        );
    }

    #[tokio::test]
    async fn details_flag_middle_truncation_of_huge_output() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        // `seq 1 20000` produces ~108KB, well past the 50KB content cap.
        let out = tool
            .execute(json!({ "command": "seq 1 20000" }), &TestCtx::new().ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.starts_with("[output truncated: original size"));

        let details = out.details.unwrap();
        assert_eq!(details["truncated"], json!(true));
        let original: usize = details["original_bytes"].as_u64().unwrap() as usize;
        assert!(original > MAX_TOOL_OUTPUT_BYTES);
        assert_eq!(original.to_string(), {
            let header = out.content.lines().next().unwrap();
            header
                .trim_start_matches("[output truncated: original size ")
                .trim_end_matches(" bytes]")
                .to_string()
        });
    }

    #[tokio::test]
    async fn streams_decoded_output_as_deltas() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let test_ctx = TestCtx::new();

        let out = tool
            .execute(json!({ "command": "echo live-output" }), &test_ctx.ctx)
            .await
            .unwrap();
        assert!(!out.is_error);

        // The smoother may still hold a remainder; flush like the agent loop
        // does before the final ToolResult.
        test_ctx.ctx.flush();

        // Subscribe now (snapshot = everything published so far), then drop
        // both hub owners so the stream reaches EOF and collect() terminates.
        let stream = test_ctx.hub.subscribe();
        let TestCtx { ctx, hub } = test_ctx;
        drop(ctx);
        drop(hub);

        let mut stdout = String::new();
        let mut stderr = String::new();
        for event in stream.collect::<Vec<crate::agent::AgentEvent>>().await {
            if let crate::agent::AgentEvent::ToolOutputDelta { stream, delta, .. } = event {
                match stream {
                    OutputStream::Stdout => stdout.push_str(&delta),
                    OutputStream::Stderr => stderr.push_str(&delta),
                }
            }
        }
        assert_eq!(stdout, "live-output\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn drain_decodable_defers_partial_chars_and_replaces_invalid_bytes() {
        let mut pending: Vec<u8> = Vec::new();
        let emoji = "👍".as_bytes(); // 4-byte char
        pending.extend_from_slice(&emoji[..2]);
        // Incomplete char stays buffered, nothing decodable yet.
        assert_eq!(drain_decodable(&mut pending), "");
        assert_eq!(pending, emoji[..2].to_vec());

        pending.extend_from_slice(&emoji[2..]);
        assert_eq!(drain_decodable(&mut pending), "👍");
        assert!(pending.is_empty());

        // A genuinely invalid byte becomes U+FFFD and decoding continues.
        pending.extend_from_slice(b"ok\xffgood");
        assert_eq!(drain_decodable(&mut pending), "ok\u{FFFD}good");
        assert!(pending.is_empty());
    }
}
