use crate::protocol::*;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Host: Send + Sync + 'static {
    async fn call(&self, seq: u64, tool: String, args: Value) -> Result<Value>;
    fn emit(&self, text: &str) -> Result<()>;
    fn image(&self, mime_type: String, data: String) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct RunResult {
    pub output: String,
    pub error: Option<String>,
    pub calls: u64,
}

pub fn worker_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OCT_CODEMODE_WORKER") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = format!("oct-codemode-worker{}", std::env::consts::EXE_SUFFIX);
    [dir.join(&name), dir.parent()?.join(&name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Parent owns the lifetime, limits and routing. JS has no access to the stdio pipes.
pub async fn run(
    worker: &Path,
    init: Init,
    host: Arc<dyn Host>,
    cancel: CancellationToken,
    approvals: Arc<AtomicUsize>,
) -> Result<RunResult> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new(worker);
    cmd.env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe syscalls run between fork and exec.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                let parent = libc::getppid();
                if parent == 1
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0
                    || libc::getppid() != parent
                {
                    return Err(std::io::Error::other("Code mode parent disappeared"));
                }
            }
            let cpu = libc::rlimit {
                rlim_cur: 10,
                rlim_max: 11,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &cpu) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("Cannot start code mode helper")?;
    let mut reader = child
        .stdout
        .take()
        .context("Missing code mode stdout pipe")?;
    let mut writer = child.stdin.take().context("Missing code mode stdin pipe")?;
    let hello = tokio::select! {
        _ = cancel.cancelled() => bail!("Code execution cancelled during helper startup"),
        hello = tokio::time::timeout(Duration::from_secs(3), read_frame::<_, FromWorker>(&mut reader)) =>
            hello.context("Code mode helper handshake timed out; rebuild oct-agent and oct-codemode-worker together")?
                .context("Code mode helper handshake failed; rebuild oct-agent and oct-codemode-worker together")?,
    };
    anyhow::ensure!(
        matches!(
            hello,
            FromWorker::Hello {
                protocol_version: PROTOCOL_VERSION
            }
        ),
        "Code mode helper protocol mismatch; rebuild oct-agent and oct-codemode-worker together"
    );
    let timeout_ms = init.timeout_ms.clamp(1, 300_000);
    write_frame(&mut writer, &ToWorker::Init(init)).await?;
    let mut tasks: tokio::task::JoinSet<Result<(u64, Result<Value, CallError>)>> =
        tokio::task::JoinSet::new();
    let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let payload_permits = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CALL_BYTES));
    let mut out = RunResult::default();
    let mut budget = Duration::from_millis(timeout_ms);
    let birth = tokio::time::Instant::now();
    let mut last = birth;
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Reading a frame must survive select branches; read_exact is not cancel-safe.
    let (messages_tx, mut messages) = tokio::sync::mpsc::channel(16);
    let read_task = tokio::spawn(async move {
        loop {
            let message = async {
                let message = read_frame::<_, FromWorker>(&mut reader).await?;
                // Validate before buffering, not just before spawning: approvals
                // can hold all execution permits while the helper sends more calls.
                let payload_bytes = match &message {
                    FromWorker::Call { tool, args, .. } => call_payload_size(tool, args)?,
                    _ => 0,
                };
                Ok::<_, anyhow::Error>((message, payload_bytes))
            }
            .await;
            let failed = message.is_err();
            if messages_tx.send(message).await.is_err() || failed {
                break;
            }
        }
    });
    struct ReaderGuard(tokio::task::JoinHandle<()>);
    impl Drop for ReaderGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _reader_guard = ReaderGuard(read_task);
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => bail!("Code execution cancelled; remote calls may already have taken effect"),
                _ = tick.tick() => {
                    let now = tokio::time::Instant::now();
                    if approvals.load(Ordering::SeqCst) == 0 { budget = budget.saturating_sub(now - last); }
                    last = now;
                    if budget.is_zero() { bail!("Code execution timed out"); }
                    if now - birth > Duration::from_secs(900) { bail!("Code execution exceeded its maximum lifetime"); }
                    #[cfg(target_os = "linux")]
                    if let Some(pid) = child.id()
                        && let Ok(status) = tokio::fs::read_to_string(format!("/proc/{pid}/status")).await {
                            let rss = status.lines().find_map(|l| l.strip_prefix("VmRSS:").and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok()));
                            if rss.is_some_and(|n| n > 512 * 1024) { bail!("Code execution exceeded memory budget"); }
                    }
                },
                reply = tasks.join_next(), if !tasks.is_empty() => {
                    let (seq, result) = reply.context("Missing code mode call")???;
                    write_frame(&mut writer, &ToWorker::Reply { seq, result }).await?;
                },
                message = messages.recv() => {
                    let (message, payload_bytes) = message.context("Code mode helper closed IPC")??;
                    match message {
                        FromWorker::Hello { .. } => bail!("Unexpected code mode helper handshake"),
                        FromWorker::Call {seq, tool, args} => {
                            anyhow::ensure!(seq == out.calls + 1 && seq <= MAX_CALLS, "Invalid code mode call sequence or call budget exceeded");
                            out.calls = seq;
                            let Ok(payload_permit) = payload_permits.clone().try_acquire_many_owned(payload_bytes as u32) else {
                                write_frame(&mut writer, &ToWorker::Reply {
                                    seq,
                                    result: Err(CallError::new("payload_limit", "Code mode pending call payload budget exceeded; await existing calls before submitting more")),
                                }).await?;
                                continue;
                            };
                            let host = host.clone();
                            let permits = permits.clone();
                            tasks.spawn(async move {
                                let _payload_permit = payload_permit;
                                let _permit = permits.acquire_owned().await?;
                                let result = host.call(seq, tool, args).await.map_err(CallError::from_error);
                                let result = if json_size_within(&result, MAX_RESULT).is_some() {
                                    result
                                } else {
                                    Err(CallError::new("result_too_large", "Tool result exceeds code mode budget; request a smaller page"))
                                };
                                Ok::<_, anyhow::Error>((seq, result))
                            });
                        },
                        FromWorker::Emit {text} => {
                            anyhow::ensure!(out.output.len().saturating_add(text.len()) <= MAX_OUTPUT, "Code mode output exceeds limit");
                            out.output.push_str(&text);
                            host.emit(&text)?;
                        },
                        FromWorker::Image {mime_type, data} => host.image(mime_type, data)?,
                        FromWorker::Done {error} => {
                            out.error = error;
                            if !tasks.is_empty() && out.error.is_none() { out.error = Some("unawaited_tool_calls: await all tool calls; some remote actions may have taken effect".into()); }
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;
    tasks.shutdown().await;
    // Kill even after Done: there must never be detached VM work.
    let _ = child.kill().await;
    let _ = child.wait().await;
    if let Err(error) = outcome {
        out.error = Some(format!("{error:#}"));
    }
    Ok(out)
}
