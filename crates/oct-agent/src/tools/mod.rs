use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod execute;
pub mod listdir;
pub mod read;
pub mod write;

/// Output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Trait that all agent tools implement.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Tool name as used by the LLM.
    fn name(&self) -> &str;

    /// Description shown to the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given JSON arguments.
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}

/// Convert an `AgentTool` to the oct-llm-provider `ToolSpec` for LLM requests.
pub fn to_tool_spec(tool: &dyn AgentTool) -> oct_llm_provider::core::ToolSpec {
    oct_llm_provider::core::ToolSpec {
        name: tool.name().to_string(),
        description: Some(tool.description().to_string()),
        input_schema: tool.input_schema(),
    }
}

/// Create all default tools for a given working directory.
pub fn default_tools(working_dir: std::path::PathBuf) -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(read::ReadFileTool::new(working_dir.clone())),
        Box::new(listdir::ListDirTool::new(working_dir.clone())),
        Box::new(write::WriteFileTool::new(working_dir.clone())),
        Box::new(execute::ExecuteCommandTool::new(working_dir)),
    ]
}
