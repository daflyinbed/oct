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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpToolMode {
    // `#[default]` MUST stay Direct: v1 WorkerSnapshot rows lack `tool_mode`
    // and decode through serde's default; flipping this to Code would reject
    // every pre-code-mode snapshot in the compatibility matrix. The runtime
    // default lives in `codemode::mode()`, not in this wire enum.
    #[default]
    Direct,
    Code,
}

impl McpToolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Code => "code",
        }
    }
}

/// Version sealed with code-mode worker tool catalogs.
pub const CODE_CATALOG_VERSION: u32 = 3;
