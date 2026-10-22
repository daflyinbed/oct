//! Conversation-level frozen prompt environment: the repo-instruction block
//! plus the MCP tool identities a conversation's system prompt was built
//! from, cached in the shared assembly services under the ROOT conversation id and reused
//! byte-for-byte while clean.
//!
//! Freezing exists for the provider prefix cache: the block sits at the head
//! of every request's system prompt, so re-reading edited files (or
//! re-rendering a changed MCP tool list) on every assembly would invalidate
//! the cache per turn. Refreshes therefore happen only at explicit points —
//! the first assembly of a (restarted) process, after a
//! compaction/microcompaction rewrote the history (the cache is broken
//! anyway, and both transactions flag the entry dirty through a
//! [`PromptEnvRefresh`] handle), and the manual `/reload` endpoint, which
//! re-parses and REPLACES the entry immediately. Mid-run the prompt is
//! constant by construction: assembly completes before the loop spawns.
//!
//! Subagent runs share their tree's entry (same root id), so the parent and
//! every worker see the same repo block and the same MCP composition.
//! Deliberately NOT frozen: code mode (a deliberate settings switch that
//! takes effect at once), additional directories (write-sandbox correctness,
//! very low frequency) and worker instructions (already sealed per run row).

use std::path::PathBuf;

use super::AssemblyServices;
use crate::agent::repo_instructions::{
    FrozenPromptEnv, PromptEnvRefresh, discover_chain, render_block,
};
use crate::agent_settings::SnapshotTool;
use crate::db::projects as project_db;

impl AssemblyServices {
    /// The per-conversation serialization lock shared by environment
    /// resolution and `/reload`: whichever starts second waits, so a run
    /// starting during a reload always resolves the FRESH entry (the user's
    /// reload-then-ask ordering) instead of racing the old one for its
    /// whole lifetime. Only the resolution section is covered — never the
    /// run itself — so reloading while a run is live stays non-blocking.
    fn env_lock(&self, root_conversation_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.prompt_env_locks
            .entry(root_conversation_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// A dirty-flag handle for one root conversation's frozen environment
    /// (carried by the agent context and the compaction transactions).
    pub fn prompt_env_refresh(&self, root_conversation_id: &str) -> PromptEnvRefresh {
        PromptEnvRefresh::new(self.prompt_envs.clone(), root_conversation_id)
    }

    /// Mark one root conversation's environment dirty: the next run assembly
    /// re-reads the instruction files and re-snapshots the MCP identities.
    /// Used by the assembly itself when a frozen MCP identity has no live
    /// instance anymore (server disabled or deleted).
    pub fn mark_prompt_env_dirty(&self, root_conversation_id: &str) {
        if let Some(mut entry) = self.prompt_envs.get_mut(root_conversation_id) {
            entry.value_mut().mark_dirty();
        }
    }

    /// Remove a conversation's frozen entry (deletion cleanup; forks never
    /// share the key, so nothing else is affected). The serialization lock
    /// goes with it — a concurrent holder keeps its Arc and finishes
    /// harmlessly.
    pub fn drop_prompt_env(&self, root_conversation_id: &str) {
        self.prompt_envs.remove(root_conversation_id);
        self.prompt_env_locks.remove(root_conversation_id);
    }

    /// The frozen environment for a conversation tree: a clean hit reuses
    /// the stored entry byte-for-byte; a missing or dirty entry re-reads the
    /// instruction files, re-snapshots the live MCP identities and replaces
    /// the stored entry. Callers reconcile MCP connections beforehand (the
    /// chat/resume/reload paths all do) — the snapshot reads the live set.
    /// Serialized against `/reload` (see [`Self::env_lock`]).
    ///
    /// Anchor staleness is NOT handled here: each assembly compares its own
    /// view's anchor generation against the entry's (`accepts_anchor`).
    pub(crate) async fn resolve_prompt_env(
        &self,
        root_conversation_id: &str,
        project: &project_db::Project,
    ) -> FrozenPromptEnv {
        let lock = self.env_lock(root_conversation_id);
        let _serialization = lock.lock().await;
        {
            let entry = self.prompt_envs.get(root_conversation_id);
            if let Some(env) = entry
                && !env.value().dirty()
            {
                return env.value().clone();
            }
        }
        let env = self.build_prompt_env(project).await;
        self.prompt_envs
            .insert(root_conversation_id.to_string(), env.clone());
        env
    }

    /// Re-read the instruction files and re-snapshot the live MCP
    /// identities, REPLACING the conversation's entry: the `/reload`
    /// endpoint. Safe while a run is live — the running prompt is already
    /// built; a run STARTING concurrently waits on the same serialization
    /// lock and then resolves this fresh entry.
    pub async fn reload_prompt_env(
        &self,
        root_conversation_id: &str,
        project: &project_db::Project,
    ) -> FrozenPromptEnv {
        let lock = self.env_lock(root_conversation_id);
        let _serialization = lock.lock().await;
        let env = self.build_prompt_env(project).await;
        self.prompt_envs
            .insert(root_conversation_id.to_string(), env.clone());
        env
    }

    /// One fresh environment: repo files discovered from the project layout
    /// (NOT the sandbox roots — those may carry exec/code-mode additions the
    /// user never configured) plus the current in-scope MCP identities.
    async fn build_prompt_env(&self, project: &project_db::Project) -> FrozenPromptEnv {
        let working_dir = PathBuf::from(&project.working_dir);
        let git_root = crate::agent::repo_instructions::detect_git_root(&working_dir).await;
        let additional: Vec<PathBuf> = project
            .additional_dirs
            .0
            .iter()
            .map(PathBuf::from)
            .collect();
        let files = discover_chain(&working_dir, git_root.as_deref(), &additional);
        let repo = render_block(&files);
        let mcp_tools = self
            .mcp
            .tools_for_run(Some(&project.id))
            .into_iter()
            .map(|tool| SnapshotTool {
                selector: tool.selector(),
                exposed_name: tool.name().into(),
                // Freeze the presentation too: the generated description
                // embeds the server name, so a rename must not rewrite the
                // prompt bytes behind the freeze's back.
                exposed_description: tool.description().to_string(),
            })
            .collect();
        FrozenPromptEnv::new(repo, mcp_tools)
    }
}

/// The per-run assembly log line: the repo file list (path/bytes/truncated)
/// and the frozen MCP composition, one line at every root/child assembly.
pub(crate) fn log_prompt_env(root_conversation_id: &str, env: &FrozenPromptEnv) {
    tracing::info!(
        conversation_id = %root_conversation_id,
        repo_files = %env.repo.file_summary(),
        mcp_tools = %env
            .mcp_tools
            .iter()
            .map(|t| t.exposed_name.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "frozen prompt environment for run assembly"
    );
}
