use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::truncate::{truncate_middle, MAX_TOOL_OUTPUT_BYTES};
use super::{AgentTool, ToolOutput};

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

/// Spawn a task that drains a child pipe into a shared capped buffer. The buffer
/// is shared so collected bytes survive even if the task has to be aborted.
fn spawn_reader<R>(mut stream: R) -> ReaderHandle
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    let buf = Arc::new(Mutex::new(Vec::new()));
    let task_buf = buf.clone();

    let handle = tokio::spawn(async move {
        // Read raw bytes; decoding happens once at the end because a chunk
        // boundary can split a UTF-8 char.
        let mut chunk = [0u8; READ_CHUNK_SIZE];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let mut buf = task_buf
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    append_capped(&mut buf, &chunk[..n], STREAM_CAP_BYTES);
                }
                // Unrecoverable read error: stop and keep what we have so far.
                Err(_) => break,
            }
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
        handle.abort();
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

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
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

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to spawn command: {e}"))),
        };

        // Capture the pgid before anything else; the child leads its own process
        // group, so pgid == child.id().
        let pgid = child.id();

        // Take down the whole group when this scope ends (covers cancellation
        // and lingering background children; kill_on_drop only covers `sh`).
        let _group_kill = GroupKillOnDrop(pgid);

        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);

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

        let exit_code = match wait_result {
            Ok(Ok(status)) => status.code().unwrap_or(-1),
            Ok(Err(e)) if timed_out => {
                // The post-kill wait failed; the command is dead either way, so
                // keep the timeout context and partial output instead of
                // dropping them.
                tracing::warn!("post-kill wait failed: {e}");
                -1
            }
            Ok(Err(e)) => return Ok(ToolOutput::error(format!("Command failed: {e}"))),
            Err(_) => {
                tracing::warn!("child survived SIGKILL for {KILL_REAP_TIMEOUT:?}; giving up on reaping");
                -1
            }
        };

        // Decode once at the end (raw bytes may split a UTF-8 char mid-chunk).
        // Drained concurrently so the worst case is one drain budget, not two.
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

        let result = truncate_middle(&result, MAX_TOOL_OUTPUT_BYTES);

        if reported_exit_code == 0 {
            Ok(ToolOutput::success(result))
        } else {
            Ok(ToolOutput::error(result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(serde_json::json!({ "command": "echo hello" }))
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
            .execute(serde_json::json!({ "command": "echo oops >&2; exit 3" }))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("[stderr]\noop"));
        assert!(out.content.contains("[exit code: 3]"));
    }

    #[tokio::test]
    async fn zero_timeout_is_floored_to_one_second() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let out = tool
            .execute(serde_json::json!({ "command": "echo hi", "timeout_secs": 0 }))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hi"));
    }

    #[tokio::test]
    async fn timeout_kills_process_group_and_reports_124() {
        let tool = ExecuteCommandTool::new(std::env::temp_dir());
        let started = std::time::Instant::now();
        // The backgrounded grandchild holds the pipe open; only a process-group
        // kill (not just killing sh) makes this return promptly.
        let out = tool
            .execute(serde_json::json!({
                "command": "sleep 30 & sleep 30",
                "timeout_secs": 1
            }))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(out.is_error);
        assert!(out.content.starts_with("Command timed out after 1 seconds"));
        assert!(out.content.contains("[exit code: 124]"));
    }
}
