//! Tool-list-driven system prompts: the prompt is built
//! from the tools the run ACTUALLY carries, not a static list — the curated
//! guideline text stays for the seven built-ins, everything else (MCP tools
//! surfaced as `mcp__{server}__{tool}`) gets a generic one-liner derived
//! from its description, and the `task` tool adds its delegation guidance
//! only for root agents: workers never see it (no nesting in v1).

use std::path::Path;
use std::sync::Arc;

use crate::tools::AgentTool;

/// Curated one-line summaries of the built-in tools, in `default_tools`
/// order. Keyed by tool name so a rebuilt toolset (e.g. a worker's file
/// tools) still renders the same lines.
const CURATED_TOOLS: &[(&str, &str)] = &[
    ("read_file", "Read file contents with line numbers"),
    ("list_dir", "List directory contents in a tree format"),
    (
        "grep",
        "Search file contents with a regular expression (content, files-with-matches, or count mode)",
    ),
    (
        "glob",
        "Find files by glob pattern (e.g. \"**/*.ts\"), sorted by most recently modified",
    ),
    ("write_file", "Write/overwrite file contents"),
    (
        "edit_file",
        "Edit a file by replacing exact text (requires a prior read_file of the file)",
    ),
    ("execute_command", "Run shell commands"),
];

/// The delegation guidance for the `task` tool, shown to ROOT agents only.
const TASK_GUIDANCE: &str = "\
Background commands: completion notifications include status, exit_code and output_file. \
Use read_file or grep on output_file for output, and task_stop to cancel. \
Do not poll or sleep while waiting for completion.
Delegation:
- Use the task tool to delegate self-contained subtasks to a fresh worker agent. \
Give `description` a short UI title and `prompt` the full instructions, including \
the goal, necessary context, the files in scope, and the acceptance criteria.
- Workers run in the same workspace as you and report back conclusions, evidence \
(files read or modified), verification/test results, and open items.
- The first version of task waits for the result; multiple task calls in the same \
round run concurrently, so prefer parallel independent tasks over one huge one.";

/// The worker identity paragraph appended to every subagent system prompt:
/// same base as the root prompt, never the task guidance (depth <= 1).
const WORKER_IDENTITY: &str = "\
Tool actions needing approval will pause until the user decides in the parent or worker panel.
You are a delegated worker subagent running in its own conversation, with no \
access to the parent agent's history. Complete the task exactly as described in \
the delegation prompt in this conversation's first user message. Your final \
report must include: the conclusion, the evidence (files read or modified), \
verification/test results, and any unresolved items. Give your final answer \
directly — do not ask to yield and do not delegate further.";

/// The first sentence of a tool description (up to the first `. ` or
/// sentence-ending punctuation), for the generic MCP-tool lines. Falls back
/// to the whole (trimmed) description.
fn first_sentence(description: &str) -> String {
    let trimmed = description.trim();
    if let Some(pos) = trimmed.find(". ") {
        trimmed[..pos].trim().to_string()
    } else {
        trimmed.trim_end_matches('.').to_string()
    }
}

/// Render the "You have access to the following tools" block for `tools`:
/// curated lines for the known built-ins (in canonical order, then any
/// non-canonical extras in list order), generic `- {name}: {description}`
/// lines for everything else (MCP tools).
fn tool_lines(tools: &[Arc<dyn AgentTool>]) -> Vec<String> {
    let has = |name: &str| tools.iter().any(|t| t.name() == name);
    let mut lines: Vec<String> = CURATED_TOOLS
        .iter()
        .filter(|(name, _)| has(name))
        .map(|(name, line)| format!("- {name}: {line}"))
        .collect();
    for tool in tools {
        let name = tool.name();
        if CURATED_TOOLS.iter().any(|(curated, _)| *curated == name) {
            continue;
        }
        if name == "task" {
            // The task tool gets its curated guidance paragraph below, not a
            // tool-list line.
            continue;
        }
        lines.push(format!("- {name}: {}", first_sentence(tool.description())));
    }
    lines
}

/// Build the base system prompt (working directory, tool list, guidelines)
/// shared by root agents and workers.
fn base_prompt(
    working_dir: &Path,
    additional_dirs: &[std::path::PathBuf],
    tools: &[Arc<dyn AgentTool>],
) -> String {
    let lines = tool_lines(tools);
    let mut prompt = format!(
        r#"You are a coding agent working in the directory: {working_dir}

You have access to the following tools:
{tool_list}

Guidelines:
- Explain your reasoning and what you're doing at each step.
- If you encounter an error, try to diagnose and fix it.
"#,
        working_dir = working_dir.display(),
        tool_list = lines.join("\n"),
    );

    // Guidance must describe only capabilities in this run's actual toolset.
    let has = |name: &str| tools.iter().any(|t| t.name() == name);
    for (name, guidance) in [
        (
            "read_file",
            "Always read files before modifying them to understand the current state.",
        ),
        (
            "grep",
            "Use grep to search file contents before reading files.",
        ),
        ("glob", "Use glob to find files by name."),
        (
            "edit_file",
            "Use edit_file for targeted changes; read_file the file before editing it.",
        ),
        (
            "write_file",
            "Use write_file for new files or full rewrites; provide the complete file content.",
        ),
        (
            "list_dir",
            "Use list_dir to explore the project structure before making changes.",
        ),
        (
            "execute_command",
            "Use execute_command for running builds, tests, and other shell operations.",
        ),
    ] {
        if has(name) {
            prompt.push_str(&format!("- {guidance}\n"));
        }
    }

    if tools.iter().any(|t| t.name() == "task") {
        prompt.push('\n');
        prompt.push_str(TASK_GUIDANCE);
        prompt.push('\n');
    }

    if !additional_dirs.is_empty() {
        let list = additional_dirs
            .iter()
            .map(|dir| format!("- {}", dir.display()))
            .collect::<Vec<_>>()
            .join("\n");
        prompt.push_str(&format!(
            "\nAdditional working directories you may also read and write:\n{list}\nAlways refer to these with absolute paths; relative paths are resolved against the main working directory above.\n"
        ));
    }

    prompt
}

/// Build the system prompt for a coding agent run from the tools it actually
/// carries: curated guideline text for the seven built-ins, generic lines for
/// MCP tools, and the task-delegation guidance when a tool named `task` is
/// present.
pub fn system_prompt(
    working_dir: &Path,
    additional_dirs: &[std::path::PathBuf],
    tools: &[Arc<dyn AgentTool>],
) -> String {
    base_prompt(working_dir, additional_dirs, tools)
}

/// The subagent (worker) system prompt: the same base, but
/// NEVER the task guidance — workers cannot nest delegations — plus the
/// worker identity paragraph describing the required report shape.
pub fn worker_system_prompt(
    working_dir: &Path,
    additional_dirs: &[std::path::PathBuf],
    tools: &[Arc<dyn AgentTool>],
) -> String {
    // Defense in depth: even if a task tool slipped into the worker set, the
    // base prompt must not teach the model to use it.
    let filtered: Vec<Arc<dyn AgentTool>> = tools
        .iter()
        .filter(|t| t.name() != "task")
        .cloned()
        .collect();
    let mut prompt = base_prompt(working_dir, additional_dirs, &filtered);
    prompt.push('\n');
    prompt.push_str(WORKER_IDENTITY);
    prompt.push('\n');
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{AgentTool, ToolContext, ToolOutput};

    struct NamedTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::success("noop"))
        }
    }

    fn tool(name: &'static str, description: &'static str) -> Arc<dyn AgentTool> {
        Arc::new(NamedTool { name, description })
    }

    #[test]
    fn root_prompt_lists_curated_and_generic_tools_with_task_guidance() {
        let tools = vec![
            tool("read_file", "curated anyway"),
            tool(
                "mcp__gh__create_issue",
                "Create a new issue. Returns the issue URL.",
            ),
            tool("task", "Delegate a subtask."),
        ];
        let prompt = system_prompt(Path::new("/w"), &[], &tools);
        assert!(prompt.contains("You are a coding agent working in the directory: /w"));
        assert!(prompt.contains("- read_file: Read file contents with line numbers"));
        // Generic line uses the FIRST SENTENCE of the description only.
        assert!(prompt.contains("- mcp__gh__create_issue: Create a new issue"));
        assert!(!prompt.contains("Returns the issue URL"));
        // The task tool has no tool-list line but its guidance paragraph is
        // present for root agents.
        assert!(!prompt.contains("- task:"));
        assert!(prompt.contains("Delegation:"));
    }

    #[test]
    fn worker_prompt_never_mentions_task_and_adds_identity() {
        let tools = vec![
            tool("read_file", "curated"),
            tool("execute_command", "curated"),
            tool("mcp__db__query", "Run a read-only SQL query."),
        ];
        let prompt = worker_system_prompt(Path::new("/w"), &[], &tools);
        assert!(!prompt.contains("Delegation:"));
        assert!(prompt.contains("delegated worker subagent"));
        assert!(prompt.contains("- mcp__db__query: Run a read-only SQL query"));

        // Even a (buggy) task tool in the set must not produce guidance.
        let with_task = vec![tool("task", "Delegate a subtask.")];
        let prompt = worker_system_prompt(Path::new("/w"), &[], &with_task);
        assert!(!prompt.contains("Delegation:"));
        assert!(!prompt.contains("- task:"));
    }

    #[test]
    fn additional_dirs_section_is_kept() {
        let tools = vec![tool("read_file", "curated")];
        let prompt = system_prompt(
            Path::new("/w"),
            &[std::path::PathBuf::from("/extra")],
            &tools,
        );
        assert!(
            prompt
                .contains("Additional working directories you may also read and write:\n- /extra")
        );
    }
}
