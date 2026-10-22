use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{AssemblyServices, log_prompt_env, workspace_roots};
use crate::agent::{AgentContext, RunHandle, prompt::system_prompt};
use crate::context::LlmView;
use crate::db::projects::Project;
use crate::error::AppResult;
use crate::runtime::{background::BackgroundTaskManager, execution::AssembledRun, worker_config};
use crate::subagent::{SubagentManager, TaskSpawnConfig, TaskTool};
use crate::tools::{self, shared::AgentReadState};
use oct_llm_provider::{core::ReasoningRequest, model::ChatModel};

pub(crate) struct RootAssembler {
    pub shared: AssemblyServices,
    pub background: Arc<BackgroundTaskManager>,
    pub subagents: Arc<SubagentManager>,
}

pub(crate) struct RootAssemblyInput {
    pub run_id: String,
    pub conversation_id: String,
    pub project: Project,
    pub model: Box<dyn ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub view: LlmView,
    pub context_window: Option<u32>,
    pub idle: Option<Duration>,
    pub reasoning: Option<ReasoningRequest>,
    pub attach_task_tool: bool,
    pub handle: RunHandle,
    pub max_output_tokens: Option<u32>,
}

impl RootAssembler {
    pub async fn assemble(&self, input: RootAssemblyInput) -> AppResult<AssembledRun> {
        let RootAssemblyInput {
            run_id,
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
            reasoning,
            attach_task_tool,
            handle,
            max_output_tokens,
        } = input;
        let (roots, additional) = workspace_roots(&project);
        // Uploaded chat attachments are readable by the model (text files
        // beyond the inline budget point here explicitly).
        let mut roots = self
            .shared
            .runtime_roots(roots, &conversation_id, &conversation_id)
            .await?;
        // Frozen prompt environment (repo instruction block + MCP tool
        // identities), keyed by the root conversation: the first assembly
        // reads the files and snapshots the live MCP set; clean hits reuse
        // the entry byte-for-byte so the system prompt — and the provider
        // prefix cache with it — stays stable across runs. Dispatch maps the
        // frozen identities onto the LIVE instances: a renamed server keeps
        // its frozen exposed name, a missing one is skipped (and dirties the
        // the entry, a missing one is skipped (and dirties the
        // entry for the next run), and servers enabled after the freeze wait
        // for the next refresh point.
        let prompt_env = self
            .shared
            .resolve_prompt_env(&conversation_id, &project)
            .await;
        // An anchor from a different prompt generation was measured under a
        // different system prompt — drop it for this run: the anchored
        // estimate excludes the prompt, so an enlarged instruction block
        // would otherwise be invisible to the compaction watermarks.
        // Equality (not a timestamp) closes the in-flight window: a request
        // completing after a `/reload` still carries the OLD generation.
        let mut view = view;
        if let Some(anchor) = &view.anchor
            && !prompt_env.accepts_anchor(anchor.prompt_generation)
        {
            view.anchor = None;
        }
        log_prompt_env(&conversation_id, &prompt_env);
        let live_mcp: Vec<Arc<dyn tools::AgentTool>> = self
            .shared
            .mcp
            .tools_for_run(Some(&project.id))
            .into_iter()
            .map(Arc::from)
            .collect();
        let (mcp_tools, missing_frozen) =
            worker_config::restore_tools_lenient(&live_mcp, &prompt_env.mcp_tools);
        if missing_frozen > 0 {
            self.shared.mark_prompt_env_dirty(&conversation_id);
        }
        let use_code = crate::codemode::mode(&self.shared.pool, Some(&project.id))
            .await
            .unwrap_or_default()
            == crate::codemode::McpToolMode::Code;
        let worker_roots = roots.clone();
        if use_code {
            roots = roots.with_read_only_root(
                self.shared
                    .codemode_root()
                    .join(&conversation_id)
                    .join(&run_id),
            );
        }
        let locks = self.shared.file_locks_for(&project.id);
        let reads = Arc::new(AgentReadState::new());

        // Default workspace tools plus every connected MCP server's tools that
        // are in scope for THIS project (global + project-scoped; the LLM sees
        // one flat toolset; MCP names are the mcp__{server}__{tool} forms
        // deduplicated across servers). The file tools share the project's
        // lock set so subagents serialize with this run on the same paths.
        let mut run_tools: Vec<Arc<dyn tools::AgentTool>> =
            tools::default_tools_with(roots.clone(), locks.clone(), reads)
                .into_iter()
                .map(Arc::from)
                .collect();
        run_tools.extend(mcp_tools);

        // Background execution surface: the default set's plain
        // execute tool is swapped in place for a background-enabled instance
        // carrying this run's identity, and the two companion tools join the
        // set. The swap is positional (schema order is otherwise unchanged).
        for tool in run_tools.iter_mut() {
            if tool.name() == "execute_command" {
                *tool = Arc::new(tools::execute::ExecuteCommandTool::with_background(
                    roots.clone(),
                    tools::execute::ExecuteBackground {
                        manager: self.background.clone(),
                        conversation_id: conversation_id.clone(),
                        parent_run_id: run_id.clone(),
                    },
                ));
            }
        }
        run_tools.push(Arc::new(tools::task_stop::TaskStopTool::new(
            self.background.clone(),
            conversation_id.clone(),
        )) as Arc<dyn tools::AgentTool>);

        // Bind the actual per-run native instances as well as MCP tools:
        // read/edit share this agent's state, and root bash keeps its background identity.
        let mut code_warning = None;
        let prepared = if use_code {
            match crate::codemode::Prepared::new(
                self.shared.codemode_root(),
                &conversation_id,
                &run_id,
                &run_tools,
            )
            .await
            {
                Ok(prepared) => Some(prepared),
                Err(error) => {
                    tracing::warn!("Code mode preparation failed; using direct tools: {error:#}");
                    code_warning = Some(format!(
                        "Code mode was unavailable for this run: {error:#}. Tools are exposed directly.\n"
                    ));
                    None
                }
            }
        } else {
            None
        };

        // The delegation tool for root runs of chat conversations: it hands
        // the SubagentManager everything it needs to spawn a worker —
        // including (through the OnceLock filled right below) the very
        // toolset built here, which children re-derive as parent-minus-task.
        // The slot pattern exists because the TaskTool is PART of the set it
        // must reference: the lock is filled only after the full Arc'd set
        // (task included) exists, and every read happens at execute time,
        // long after.
        let parent_toolset: crate::subagent::ToolsetSlot = Arc::new(std::sync::OnceLock::new());
        if attach_task_tool {
            let task: Arc<dyn tools::AgentTool> = Arc::new(TaskTool::new(
                self.subagents.clone(),
                Arc::new(TaskSpawnConfig {
                    parent_conversation_id: conversation_id.clone(),
                    parent_run_id: run_id.clone(),
                    root_conversation_id: conversation_id.clone(),
                    root_run_id: run_id.clone(),
                    project: project.clone(),
                    roots: worker_roots,
                    locks: locks.clone(),
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    context_window,
                    tool_mode: if prepared.is_some() {
                        crate::codemode::McpToolMode::Code
                    } else {
                        crate::codemode::McpToolMode::Direct
                    },
                    parent_cancel: handle.cancel.clone(),
                    parent_hub: handle.hub.clone(),
                    parent_toolset: parent_toolset.clone(),
                    // The parent's OWN frozen environment: children spawned
                    // from this live run inherit it verbatim instead of
                    // re-resolving the mutable cache, so a mid-run `/reload`
                    // (or a compaction dirty flag) cannot make a child drift
                    // from the parent that delegated to it.
                    parent_env: Arc::new(prompt_env.clone()),
                }),
            ));
            run_tools.push(task);
        }

        let toolset = Arc::new(run_tools);
        if attach_task_tool {
            // Fill the slot now that the full set (task included) exists —
            // as a WEAK reference, so the slot never keeps the toolset (and
            // the run's hub, via the task tool's config) alive past the run
            // (see TaskSpawnConfig::parent_toolset). Children exclude `task`
            // by name when deriving their own sets.
            let _ = parent_toolset.set(Arc::downgrade(&toolset));
        }

        // The run's permission gate: project rules + this
        // conversation's mode, loaded once; `allow_always` decisions inject
        // into it live through the service's gate index.
        let gate = self
            .shared
            .assemble_gate(
                &conversation_id,
                &conversation_id,
                &run_id,
                &project.id,
                &roots,
                false,
            )
            .await?;

        // Keep the full set alive through CodeTool; TaskTool's weak parent slot
        // references it, so worker selection sees the original authorized tools.
        let code_section = prepared.as_ref().map(|p| p.prompt_section());
        let model_tools = match prepared {
            Some(prepared) => prepared.expose(toolset.clone(), gate.clone()),
            None => toolset.clone(),
        };
        let mut prompt = system_prompt(
            &PathBuf::from(&project.working_dir),
            &additional,
            &model_tools,
            code_section.as_ref(),
        );
        if let Some(warning) = code_warning {
            prompt.push_str(&warning);
        }
        // Repo instructions close the prompt head (after the code-mode
        // warning): the frozen block is byte-stable across the
        // conversation's runs, so the prefix stays cacheable.
        prompt.push_str(&prompt_env.repo.block);
        let ctx = AgentContext {
            model: Arc::from(model),
            tools: model_tools,
            pool: self.shared.pool.clone(),
            // Built from the ACTUAL tool list (task guidance included for
            // root agents; see the prompt module).
            system_prompt: prompt,
            context_window,
            idle,
            reasoning,
            input_modalities: self
                .shared
                .resolve_input_modalities(&provider_id, &model_id)
                .await,
            max_output_tokens,
            attachments: Some(self.shared.attachments.clone()),
            permission: gate,
            prompt_env_refresh: Some(self.shared.prompt_env_refresh(&conversation_id)),
            prompt_generation: prompt_env.generation,
        };

        Ok(AssembledRun { ctx, view })
    }
}
