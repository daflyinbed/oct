use std::path::PathBuf;
use std::sync::Arc;

use super::{AssemblyServices, log_prompt_env, workspace_roots};
use crate::agent::{
    AgentContext, prompt::worker_system_prompt, repo_instructions::FrozenPromptEnv,
};
use crate::agent_settings::WorkerSnapshot;
use crate::context::LlmView;
use crate::db::projects::Project;
use crate::error::{AppError, AppResult};
use crate::runtime::{execution::AssembledRun, worker_config};
use crate::tools::{
    self,
    shared::{AgentReadState, WorkspaceFileLocks},
    workspace::WorkspaceRoots,
};
use oct_llm_provider::model::ChatModel;

pub(crate) struct WorkerAssembler {
    pub shared: AssemblyServices,
}

pub(crate) struct WorkerAssemblyInput {
    pub run_id: String,
    pub epoch: i64,
    pub hub: Arc<crate::events::EventHub>,
    pub conversation_id: String,
    pub project: Project,
    pub model: Box<dyn ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub view: LlmView,
    pub context_window: Option<u32>,
    pub agent_config: Option<WorkerSnapshot>,
    pub roots: WorkspaceRoots,
    pub locks: Arc<WorkspaceFileLocks>,
    pub inheritance: Option<Arc<crate::application::inheritance::WorkerInheritance>>,
    pub parent_env: Option<Arc<FrozenPromptEnv>>,
}

impl WorkerAssembler {
    pub async fn assemble(&self, input: WorkerAssemblyInput) -> AppResult<AssembledRun> {
        let WorkerAssemblyInput {
            run_id,
            epoch,
            hub,
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            agent_config,
            roots,
            locks,
            inheritance,
            parent_env,
        } = input;
        // The worker shares its tree's frozen environment (keyed by ROOT
        // conversation id — the parent's own id for depth-1 tasks): same
        // repo block, same MCP identity set, parent and children never
        // drift mid-conversation. A child dispatched from a LIVE parent
        // inherits the parent's own environment verbatim (the cache may
        // have been replaced by a mid-run `/reload` — the parent keeps
        // running on its original block, so the child must too); only the
        // recovery path (no live parent) resolves the cache. Recovery
        // included: this assembly IS the recovery path.
        let root_conversation_id =
            crate::db::conversations::get_conversation(&self.shared.pool, &conversation_id)
                .await?
                .and_then(|conv| conv.root_conversation_id)
                .unwrap_or_else(|| conversation_id.clone());
        let prompt_env = match parent_env {
            Some(env) => (*env).clone(),
            None => {
                self.shared
                    .resolve_prompt_env(&root_conversation_id, &project)
                    .await
            }
        };
        // Same generation rule as the root assembly, against the child's OWN
        // view: an inherited environment still rejects anchors measured
        // under a different prompt, checked per run (no consumed flag to
        // race for between parent and children).
        let mut view = view;
        if let Some(anchor) = &view.anchor
            && !prompt_env.accepts_anchor(anchor.prompt_generation)
        {
            view.anchor = None;
        }
        log_prompt_env(&root_conversation_id, &prompt_env);

        let tool_mode = agent_config
            .as_ref()
            .map(|snapshot| snapshot.tool_mode)
            .unwrap_or_default();
        let mut roots = self
            .shared
            .runtime_roots(roots, &conversation_id, &root_conversation_id)
            .await?;
        if tool_mode == crate::tool_contract::McpToolMode::Code {
            roots = roots.with_read_only_root(
                self.shared
                    .codemode_root()
                    .join(&conversation_id)
                    .join(&run_id),
            );
        }

        // Effective toolset = parent set − task, with file/search/command
        // tools rebuilt over the worker's scoped roots, shared locks and a
        // FRESH read state (the child's read records must be its own; MCP
        // tools are reused by Arc so connections stay shared). Recovery passes
        // `inheritance = None`: the parent's instances died with the
        // crashed process, so the set is rebuilt from the same assembly
        // sources — which yields the same effective set.
        let child_tools: Vec<Arc<dyn crate::tool_contract::AgentTool>> = match inheritance {
            Some(parent) => derive_child_toolset(&parent, roots.clone(), locks.clone()),
            None => {
                let mut rebuilt: Vec<Arc<dyn crate::tool_contract::AgentTool>> =
                    tools::default_tools_with(
                        roots.clone(),
                        locks.clone(),
                        Arc::new(AgentReadState::new()),
                    )
                    .into_iter()
                    .map(Arc::from)
                    .collect();
                // Recovery deliberately does NOT filter by the conversation
                // freeze: the worker's own sealed snapshot (restored strictly
                // below) is the selection source, per the recovery contract —
                // a server that was absent from a `/reload`-era freeze but is
                // live again at resume must restore, not fail the run with
                // "Worker snapshot tool is unavailable". Recovery is already
                // a cache-breaking event, so freeze purity buys nothing here.
                rebuilt.extend(
                    self.shared
                        .mcp
                        .tools_for_run(Some(&project.id))
                        .into_iter()
                        .map(Arc::from),
                );
                rebuilt
            }
        };
        let child_tools = match &agent_config {
            Some(snapshot) => worker_config::restore_tools(child_tools, &snapshot.tools)?,
            None => child_tools,
        };
        // Code-mode workers discover their SDK through the regular read
        // tools; the settings API rejects non-empty allowlists without them,
        // and this guard covers configs sealed before a mode switch to code.
        if tool_mode == crate::tool_contract::McpToolMode::Code && !child_tools.is_empty() {
            let names: std::collections::HashSet<&str> =
                child_tools.iter().map(|tool| tool.name()).collect();
            let missing: Vec<&str> = ["read_file", "grep", "glob"]
                .into_iter()
                .filter(|name| !names.contains(name))
                .collect();
            if !missing.is_empty() {
                let error = anyhow::anyhow!(
                    "Code mode worker requires read_file/grep/glob in its tool set for SDK discovery; missing: {}",
                    missing.join(", ")
                );
                return Err(AppError::Internal(error));
            }
        }
        let child_tools = Arc::new(child_tools);
        let prepared =
            if tool_mode == crate::tool_contract::McpToolMode::Code && !child_tools.is_empty() {
                match crate::codemode::Prepared::for_worker(
                    self.shared.codemode_root(),
                    &conversation_id,
                    &run_id,
                    epoch,
                    &child_tools,
                )
                .await
                {
                    Ok(prepared) => Some(prepared),
                    Err(error) => {
                        // A sealed code-mode worker must not silently change its
                        // exposure mode on recovery when its helper is unavailable.
                        return Err(error.into());
                    }
                }
            } else {
                None
            };

        // The child's permission gate: rules load like
        // a root run's, the MODE inherits the root conversation (child rows
        // carry no effective mode — recovery included, since this assembly
        // is the recovery path too). Worker approvals are also delivered
        // to the parent surface while keeping their own run identity.
        let gate = self
            .shared
            .assemble_gate(
                &conversation_id,
                &root_conversation_id,
                &run_id,
                &project.id,
                &roots,
                true,
            )
            .await?;

        let code_section = prepared.as_ref().map(|p| p.prompt_section());
        let model_tools = match prepared {
            Some(prepared) => prepared.expose(
                child_tools,
                gate.clone(),
                crate::tool_host::CodeHostEvents::new(hub),
            ),
            None => child_tools,
        };
        let (_, additional) = workspace_roots(&project);
        let mut prompt = worker_system_prompt(
            &PathBuf::from(&project.working_dir),
            &additional,
            &model_tools,
            code_section.as_ref(),
        );
        // Repo instructions sit between the worker identity and the user's
        // worker instructions: repository conventions override the built-in
        // prompt, and the user's explicit configuration stays last (the
        // escape hatch that must be able to win).
        prompt.push_str(&prompt_env.repo.block);
        if let Some(snapshot) = &agent_config
            && !snapshot.instructions.is_empty()
        {
            prompt.push_str("\nAdditional worker instructions:\n");
            prompt.push_str(&snapshot.instructions);
        }
        let ctx = AgentContext {
            model: Arc::from(model),
            tools: model_tools,
            pool: self.shared.pool.clone(),
            system_prompt: prompt,
            context_window,
            idle: None,
            reasoning: None,
            input_modalities: self
                .shared
                .resolve_input_modalities(&provider_id, &model_id)
                .await,
            max_output_tokens: None,
            attachments: Some(self.shared.attachments.clone()),
            permission: gate,
            prompt_env_refresh: Some(self.shared.prompt_env_refresh(&root_conversation_id)),
            prompt_generation: prompt_env.generation,
        };

        Ok(AssembledRun { ctx, view })
    }
}

/// Derive a worker's toolset from the parent's live set:
/// parent set − `task`, with file tools rebuilt over the worker's scoped
/// roots, shared locks and a fresh read state. Everything else (MCP tools)
/// is Arc-cloned so connections and configs are reused
/// verbatim — schema AND dispatch stay exactly parent-minus-task.
///
/// `execute_command` is likewise rebuilt (background gate closed, schema
/// identical): subagent workers must not detach commands — the wake-run
/// semantics only hold in the root conversation. The background
/// companion (`task_stop`) are scoped to the root
/// conversation and simply dropped.
pub(crate) fn derive_child_toolset(
    parent: &crate::application::inheritance::WorkerInheritance,
    roots: WorkspaceRoots,
    locks: Arc<WorkspaceFileLocks>,
) -> Vec<Arc<dyn crate::tool_contract::AgentTool>> {
    let mut child: Vec<Arc<dyn crate::tool_contract::AgentTool>> =
        tools::default_tools_with(roots, locks, Arc::new(AgentReadState::new()))
            .into_iter()
            .map(Arc::from)
            .collect();
    child.extend(parent.mcp_tools().iter().cloned());
    child
}
