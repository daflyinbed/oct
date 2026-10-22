//! Shared model/tool/prompt assembly resources. Worker assembly has no
//! delegation or background-execution service to call back into.
mod code_recovery;
mod prompt_env;
mod root;
mod worker;

pub(crate) use root::{RootAssembler, RootAssemblyInput};
pub(crate) use worker::{WorkerAssembler, WorkerAssemblyInput};

use crate::agent::{RunHandle, repo_instructions::FrozenPromptEnv};
use crate::attachments::AttachmentService;
use crate::error::{AppError, AppResult};
use crate::mcp::McpManager;
use crate::permissions::{
    evaluator::PermissionMode,
    gate::PermissionGate,
    registry::ApprovalRegistry,
    rules::{Effect, Rule, RuleSet},
};
use crate::runtime::{ModelResolver, workspace_roots};
use crate::tools::{shared::WorkspaceFileLocks, workspace::WorkspaceRoots};
use dashmap::DashMap;
use prompt_env::log_prompt_env;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

#[derive(Clone)]
pub(crate) struct AssemblyServices {
    pub pool: SqlitePool,
    pub models: ModelResolver,
    pub mcp: Arc<McpManager>,
    pub exec_dir: PathBuf,
    pub sessions: Arc<DashMap<String, RunHandle>>,
    pub file_locks: Arc<DashMap<String, Arc<WorkspaceFileLocks>>>,
    pub attachments: Arc<AttachmentService>,
    pub approvals: Arc<ApprovalRegistry>,
    pub gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>,
    pub prompt_envs: Arc<DashMap<String, FrozenPromptEnv>>,
    pub prompt_env_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl AssemblyServices {
    /// Runtime data follows the conversation, not the global storage roots.
    /// Workers also inherit their root conversation's outputs and attachments;
    /// the same assembly applies on recovery when no parent toolset survives.
    pub(crate) async fn runtime_roots(
        &self,
        mut roots: WorkspaceRoots,
        conversation_id: &str,
        root_conversation_id: &str,
    ) -> AppResult<WorkspaceRoots> {
        for id in std::iter::once(root_conversation_id)
            .chain((conversation_id != root_conversation_id).then_some(conversation_id))
        {
            roots = roots
                .with_read_only_root(self.exec_dir.as_path().join(id))
                .with_resource_paths(
                    self.attachments
                        .resource_paths(id)
                        .await
                        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?,
                );
        }
        Ok(roots)
    }

    pub(crate) fn codemode_root(&self) -> PathBuf {
        self.exec_dir
            .parent()
            .unwrap_or(&self.exec_dir)
            .join("codemode")
    }

    /// Shared workspace file locks, created on first use. Project identity
    /// does not partition locks: separate projects may share physical paths.
    pub fn file_locks_for(&self, _project_id: &str) -> Arc<WorkspaceFileLocks> {
        self.file_locks
            .entry("shared-paths".to_string())
            .or_insert_with(|| Arc::new(WorkspaceFileLocks::new()))
            .value()
            .clone()
    }

    // --- Permission gate assembly ------------------------------------------

    /// Assemble one run's permission gate: the project's rules (project +
    /// global rows) loaded once into memory, the conversation mode, and the
    /// built-in exec-root deny anchored at the SAME directory the
    /// background manager writes to. Subagent runs pass their ROOT
    /// conversation's id as `mode_conversation_id` (child rows carry no
    /// effective mode; approvals also reach the parent) and `subagent = true`.
    ///
    /// Rule/mode storage errors fail assembly so the execution owner can
    /// settle the run. A missing/unknown mode uses the fail-closed default.
    pub(crate) async fn assemble_gate(
        &self,
        conversation_id: &str,
        mode_conversation_id: &str,
        run_id: &str,
        project_id: &str,
        roots: &WorkspaceRoots,
        subagent: bool,
    ) -> Result<Arc<PermissionGate>, AppError> {
        let mut rules = RuleSet::new(self.exec_dir.as_path());
        rules.protect_read_only_roots(roots.read_only_roots());
        // Write protection covers ALL runtime assets, independently of the
        // much narrower read grants of this run.
        rules.protect_read_only_roots(&[self.codemode_root(), self.attachments.root().into()]);
        match crate::db::permissions::list_rules_for_project(&self.pool, project_id).await {
            Ok(rows) => {
                for row in &rows {
                    // CHECK-constrained columns; an unparsable value (a
                    // hand-edited DB) just drops the row.
                    if let Some(effect) = Effect::from_str_opt(&row.effect) {
                        rules.add(Rule {
                            effect,
                            tool: row.tool.clone(),
                            pattern: row.pattern.clone(),
                        });
                    }
                }
            }
            Err(e) => return Err(AppError::Internal(e)),
        }
        let mode = crate::db::conversations::get_conversation(&self.pool, mode_conversation_id)
            .await?
            .and_then(|conv| PermissionMode::from_str_opt(&conv.permission_mode))
            .unwrap_or(PermissionMode::Default);

        let parent = if subagent {
            let handle = self
                .sessions
                .get(mode_conversation_id)
                .ok_or_else(|| AppError::Conflict("Parent run is no longer active".into()))?;
            Some(crate::permissions::gate::ApprovalParent {
                conversation_id: mode_conversation_id.to_string(),
                hub: Arc::downgrade(&handle.hub),
            })
        } else {
            None
        };
        let gate = PermissionGate::new(
            conversation_id.to_string(),
            run_id.to_string(),
            roots.clone(),
            rules,
            mode,
            parent,
            self.approvals.clone(),
        );
        self.register_gate(conversation_id, &gate);
        // Subagent gates also register under their ROOT conversation: a
        // mode PATCH on the root reaches the workers whose mode it is, and
        // a root `allow_always` covers the whole live task tree.
        if subagent && mode_conversation_id != conversation_id {
            self.register_gate(mode_conversation_id, &gate);
        }
        Ok(gate)
    }

    /// Record a gate under a conversation key, pruning references whose
    /// runs have settled (weak by design — the gate dies with its loop).
    fn register_gate(&self, conversation_id: &str, gate: &Arc<PermissionGate>) {
        let mut entry = self.gates.entry(conversation_id.to_string()).or_default();
        entry.retain(|weak| weak.strong_count() > 0);
        entry.push(Arc::downgrade(gate));
    }

    pub async fn resolve_input_modalities(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> crate::modality::InputModalities {
        self.models
            .resolve_input_modalities(provider_id, model_id)
            .await
    }
}
