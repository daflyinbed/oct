//! Native tools over neutral contracts and explicitly injected run capabilities.
mod atomic_file;
pub mod edit;
pub mod execute;
pub mod glob;
pub mod grep;
pub mod listdir;
pub mod read;
pub mod search_common;
pub mod shared;
pub mod task;
pub mod task_stop;
pub mod truncate;
pub mod workspace;
pub mod write;
use crate::tool_contract::AgentTool;
use std::sync::Arc;

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
