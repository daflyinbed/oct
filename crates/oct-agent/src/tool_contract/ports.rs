//! Scoped capabilities supplied by the application at run assembly.
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

pub struct BackgroundCommand {
    pub call_id: String,
    pub command: String,
    pub cwd: PathBuf,
}

pub struct BackgroundStarted {
    pub task_id: String,
    pub command: String,
    pub working_dir: Option<String>,
    pub output_path: PathBuf,
}

pub enum BackgroundStop {
    Unknown { known_ids: Vec<String> },
    Settling,
    Settled { status: String },
}

/// Bound to one conversation and run by the host. Model input cannot choose
/// either scope; stop checks ownership before touching a cancellation token.
#[async_trait]
pub trait BackgroundCommands: Send + Sync {
    async fn start(&self, command: BackgroundCommand) -> Result<BackgroundStarted>;
    async fn stop(&self, task_id: &str) -> Result<BackgroundStop>;
}

/// Failure of a delegation, retaining known child identities for diagnostics.
pub struct TaskError {
    pub message: String,
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct TaskResult {
    pub content: String,
    pub is_error: bool,
    pub details: serde_json::Value,
}

#[async_trait]
pub trait TaskDelegation: Send + Sync {
    async fn delegate(
        &self,
        call_id: &str,
        description: &str,
        prompt: &str,
    ) -> std::result::Result<TaskResult, TaskError>;
}
