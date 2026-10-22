use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Stable identity independent of model-facing aliases and settings storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolSelector {
    Builtin { name: String },
    Mcp { server_id: String, tool: String },
}

/// Identity of an operation and the outer model call owning its timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallIdentity {
    pub call_id: String,
    pub timeline_call_id: String,
}
