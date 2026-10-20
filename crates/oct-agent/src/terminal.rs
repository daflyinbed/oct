//! User-operated PTYs. Sessions outlive WebSocket attachments so switching
//! views or reconnecting never starts another shell (or loses an SSH login).
mod pty_io;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tokio::sync::{Notify, Semaphore, mpsc};
use utoipa::ToSchema;
use uuid::Uuid;

const REPLAY_BYTES: usize = 1024 * 1024;
const MAX_SESSIONS: usize = 16;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct TerminalInfo {
    pub id: String,
    pub project_id: String,
    pub working_dir: String,
    pub shell: String,
    pub created_at: String,
    pub exited: bool,
    pub exit_code: Option<u32>,
}

pub struct TerminalManager {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    shell: String,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::with_shell(
            std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "powershell.exe"
                    } else {
                        "/bin/sh"
                    }
                    .into()
                }),
        )
    }
}

impl TerminalManager {
    pub fn with_shell(shell: impl Into<String>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            shell: shell.into(),
        }
    }
    pub fn create(&self, project_id: &str, cwd: &Path, size: PtySize) -> Result<TerminalInfo> {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= MAX_SESSIONS {
            bail!("Terminal limit ({MAX_SESSIONS}) reached; close an existing terminal first");
        }
        let session = TerminalSession::spawn(project_id, cwd, size, &self.shell)?;
        let info = session.info();
        sessions.insert(info.id.clone(), Arc::new(session));
        Ok(info)
    }

    pub fn list(&self, project_id: &str) -> Vec<TerminalInfo> {
        let mut items: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|session| session.base.project_id == project_id)
            .map(|session| session.info())
            .collect();
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        items
    }

    pub fn get(&self, project_id: &str, id: &str) -> Option<Arc<TerminalSession>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|session| session.base.project_id == project_id)
            .cloned()
    }

    pub fn remove(&self, project_id: &str, id: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if !sessions
            .get(id)
            .is_some_and(|s| s.base.project_id == project_id)
        {
            return false;
        }
        if let Some(session) = sessions.remove(id) {
            session.close();
        }
        true
    }

    pub fn remove_project(&self, project_id: &str) {
        self.sessions.lock().unwrap().retain(|_, session| {
            if session.base.project_id != project_id {
                return true;
            }
            session.close();
            false
        });
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        for session in self.sessions.get_mut().unwrap().values() {
            session.close();
        }
    }
}

enum Input {
    Data(Vec<u8>),
    Resize(PtySize),
}

#[derive(Default)]
struct Output {
    chunks: VecDeque<(u64, Vec<u8>)>,
    end: u64,
    eof: bool,
    exit_code: Option<u32>,
    error: Option<String>,
    closed: bool,
}

impl Output {
    fn append(&mut self, bytes: &[u8]) {
        self.chunks.push_back((self.end, bytes.to_vec()));
        self.end += bytes.len() as u64;
        while self.chunks.front().is_some_and(|(offset, chunk)| {
            self.end - (*offset + chunk.len() as u64) >= REPLAY_BYTES as u64
        }) {
            self.chunks.pop_front();
        }
    }

    fn start(&self) -> u64 {
        self.chunks.front().map_or(self.end, |(offset, _)| *offset)
    }
}

pub struct OutputSnapshot {
    pub start: u64,
    pub end: u64,
    pub chunks: Vec<(u64, Vec<u8>)>,
    pub finished: bool,
    pub exit_code: Option<u32>,
    pub error: Option<String>,
}

type PtyHandle = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

pub struct TerminalSession {
    base: TerminalInfo,
    master: PtyHandle,
    input: Mutex<Option<mpsc::Sender<Input>>>,
    stop: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    io_closed: Arc<AtomicBool>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    output: Arc<Mutex<Output>>,
    pub changed: Arc<Notify>,
    /// One controller per session. Kept separately from the PTY lifetime;
    /// dropping an attachment releases ownership without killing its shell.
    pub controller: Arc<Semaphore>,
    /// Output already processed by a controller. Fresh views may replay it,
    /// but only queries beyond this watermark still need terminal replies.
    acknowledged_output: AtomicU64,
}

impl TerminalSession {
    fn spawn(project_id: &str, cwd: &Path, size: PtySize, shell: &str) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(size)
            .context("open terminal PTY")?;
        let mut command = CommandBuilder::new(shell);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let io_closed = Arc::new(AtomicBool::new(false));
        let (mut reader, mut writer) = pty_io::open(pair.master.as_ref(), io_closed.clone())?;
        let mut child = pair
            .slave
            .spawn_command(command)
            .context("start interactive shell")?;
        drop(pair.slave);
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let output = Arc::new(Mutex::new(Output::default()));
        let changed = Arc::new(Notify::new());
        let (input_tx, mut input_rx) = mpsc::channel::<Input>(16);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();

        // portable-pty exposes blocking handles. Dedicated blocking workers
        // keep writes, reads and waitpid off Tokio's async executor.
        let write_master = master.clone();
        let write_output = output.clone();
        let write_changed = changed.clone();
        let write_closed = io_closed.clone();
        let write_worker = std::thread::spawn(move || {
            while let Some(input) = input_rx.blocking_recv() {
                if write_closed.load(Ordering::Acquire) {
                    break;
                }
                let result = match input {
                    Input::Data(bytes) => writer.write_all(&bytes).map_err(anyhow::Error::from),
                    Input::Resize(size) => {
                        let guard = write_master.lock().unwrap();
                        match guard.as_ref() {
                            Some(pty) => pty.resize(size),
                            None => break,
                        }
                    }
                };
                if let Err(error) = result {
                    if !write_closed.load(Ordering::Acquire) {
                        write_output.lock().unwrap().error =
                            Some(format!("Terminal input: {error}"));
                        write_changed.notify_waiters();
                    }
                    break;
                }
            }
        });

        let read_output = output.clone();
        let read_changed = changed.clone();
        let read_closed = io_closed.clone();
        let read_worker = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                // ConPTY's close drains its output pipe; its reader must
                // continue until EOF while the waiter closes the console.
                if cfg!(unix) && read_closed.load(Ordering::Acquire) {
                    break;
                }
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => read_output.lock().unwrap().append(&buffer[..count]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Linux PTYs report EIO after the slave closes.
                    Err(error) if cfg!(target_os = "linux") && error.raw_os_error() == Some(5) => {
                        break;
                    }
                    Err(error) => {
                        if !read_closed.load(Ordering::Acquire) {
                            read_output.lock().unwrap().error =
                                Some(format!("Terminal output: {error}"));
                        }
                        break;
                    }
                }
                read_changed.notify_waiters();
            }
            read_output.lock().unwrap().eof = true;
            read_changed.notify_waiters();
        });

        let wait_output = output.clone();
        let wait_changed = changed.clone();
        let wait_master = master.clone();
        let wait_worker = std::thread::spawn(move || {
            // The same worker owns reaping and killing. A cloned PID-based
            // killer used after wait() could otherwise signal a reused PID.
            let result = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Err(error) => break Err(error),
                    Ok(None) => {}
                }
                match stop_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    _ => {
                        let master = wait_master.lock().unwrap();
                        #[cfg(unix)]
                        if let Some(fd) = master.as_ref().and_then(|pty| pty.as_raw_fd()) {
                            // Foreground jobs (including ssh) have a different
                            // process group from their interactive shell.
                            let foreground = unsafe { libc::tcgetpgrp(fd) };
                            if foreground > 0 {
                                unsafe {
                                    libc::killpg(foreground, libc::SIGKILL);
                                }
                            }
                        }
                        drop(master);
                        let _ = child.kill();
                        break child.wait();
                    }
                }
            };
            wait_master.lock().unwrap().take();
            let mut output = wait_output.lock().unwrap();
            match result {
                Ok(status) => output.exit_code = Some(status.exit_code()),
                Err(error) => {
                    output.error = Some(format!("Terminal exit: {error}"));
                    output.exit_code = Some(1);
                }
            }
            drop(output);
            wait_changed.notify_waiters();
        });

        Ok(Self {
            base: TerminalInfo {
                id: Uuid::new_v4().to_string(),
                project_id: project_id.into(),
                working_dir: cwd.to_string_lossy().into_owned(),
                shell: shell.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                exited: false,
                exit_code: None,
            },
            master,
            input: Mutex::new(Some(input_tx)),
            stop: Mutex::new(Some(stop_tx)),
            io_closed,
            workers: Mutex::new(vec![write_worker, read_worker, wait_worker]),
            output,
            changed,
            controller: Arc::new(Semaphore::new(1)),
            acknowledged_output: AtomicU64::new(0),
        })
    }

    pub fn info(&self) -> TerminalInfo {
        let output = self.output.lock().unwrap();
        TerminalInfo {
            exited: output.exit_code.is_some() || output.closed,
            exit_code: output.exit_code,
            ..self.base.clone()
        }
    }

    pub fn snapshot(&self, offset: u64) -> OutputSnapshot {
        let output = self.output.lock().unwrap();
        OutputSnapshot {
            start: output.start(),
            end: output.end,
            chunks: output
                .chunks
                .iter()
                .filter_map(|(start, bytes)| {
                    let skip = offset.saturating_sub(*start) as usize;
                    (skip < bytes.len()).then(|| (*start + skip as u64, bytes[skip..].to_vec()))
                })
                .collect(),
            finished: output.closed || (output.eof && output.exit_code.is_some()),
            exit_code: output.exit_code,
            error: output.error.clone(),
        }
    }

    pub fn size(&self) -> Result<PtySize> {
        self.master
            .lock()
            .unwrap()
            .as_ref()
            .context("Terminal closed")?
            .get_size()
    }

    pub async fn write(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > MAX_INPUT_BYTES {
            bail!("Terminal input is too large");
        }
        self.send(Input::Data(bytes)).await
    }

    pub fn acknowledged_output(&self) -> u64 {
        self.acknowledged_output.load(Ordering::Acquire)
    }

    pub fn acknowledge_output(&self, offset: u64) {
        self.acknowledged_output.fetch_max(offset, Ordering::AcqRel);
    }

    pub async fn resize(&self, size: PtySize) -> Result<()> {
        validate_size(size.cols, size.rows)?;
        self.send(Input::Resize(size)).await
    }

    async fn send(&self, input: Input) -> Result<()> {
        let sender = self
            .input
            .lock()
            .unwrap()
            .as_ref()
            .context("Terminal closed")?
            .clone();
        tokio::time::timeout(std::time::Duration::from_secs(10), sender.send(input))
            .await
            .context("Terminal input stalled")?
            .context("Terminal input closed")
    }

    pub fn close(&self) {
        let mut output = self.output.lock().unwrap();
        output.closed = true;
        drop(output);
        self.io_closed.store(true, Ordering::Release);
        self.input.lock().unwrap().take();
        if let Some(stop) = self.stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
        self.changed.notify_waiters();
        // REST deletion runs on a blocking thread. Do not release the session
        // slot while a slave-holding background job can keep IO workers alive.
        for worker in self.workers.lock().unwrap().drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn validate_size(cols: u16, rows: u16) -> Result<()> {
    if !(2..=500).contains(&cols) || !(1..=300).contains(&rows) {
        bail!("Terminal size must be 2–500 columns and 1–300 rows");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_retains_bounded_bytes_and_clips_at_cursor() {
        let mut output = Output::default();
        for _ in 0..300 {
            output.append(&[b'x'; 8192]);
        }
        assert!(output.end - output.start() <= REPLAY_BYTES as u64 + 8192);
        assert!(output.start() > 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn closing_releases_workers_while_a_background_holder_keeps_the_slave_open() {
        let directory = tempfile::tempdir().unwrap();
        let manager = TerminalManager::with_shell("/bin/sh");
        let info = manager
            .create("project", directory.path(), PtySize::default())
            .unwrap();
        let session = manager.get("project", &info.id).unwrap();
        let tty = session
            .master
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .tty_name()
            .unwrap();
        // This fd has the same effect as an idle background job retaining
        // its terminal: killing the shell cannot produce EOF for the reader.
        let _slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tty)
            .unwrap();
        let output = Arc::downgrade(&session.output);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                assert!(manager.remove("project", &info.id));
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(session.snapshot(0).error.is_none());
        assert!(session.snapshot(0).finished);
        drop(session);
        assert!(
            output.upgrade().is_none(),
            "a PTY worker still owns the output buffer"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn manager_drop_terminates_foreground_job_even_with_a_view_attached() {
        let directory = tempfile::tempdir().unwrap();
        let manager = TerminalManager::with_shell("/bin/sh");
        let info = manager
            .create("project", directory.path(), PtySize::default())
            .unwrap();
        let session = manager.get("project", &info.id).unwrap();
        session.write(b"stty -echo; sh -c 'echo $$ > foreground.pid; printf \"\\n__WAITING__\\n\"; exec cat'\r".to_vec()).await.unwrap();
        let wait_for = |exited: bool| {
            let session = session.clone();
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let changed = session.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        let snapshot = session.snapshot(0);
                        let text: Vec<_> = snapshot
                            .chunks
                            .into_iter()
                            .flat_map(|(_, bytes)| bytes)
                            .collect();
                        if if exited {
                            snapshot.exit_code.is_some()
                        } else {
                            String::from_utf8_lossy(&text).contains("\r\n__WAITING__\r\n")
                        } {
                            break;
                        }
                        changed.await;
                    }
                })
                .await
                .unwrap();
            }
        };
        wait_for(false).await;
        let pid = std::fs::read_to_string(directory.path().join("foreground.pid")).unwrap();
        drop(manager);
        wait_for(true).await;
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid.trim()));
        assert!(
            status.is_err()
                || status
                    .unwrap()
                    .lines()
                    .any(|line| line.starts_with("State:") && line.contains("Z (zombie)")),
            "foreground job is still running"
        );
    }
}
