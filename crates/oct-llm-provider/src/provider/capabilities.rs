use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub native_tools: bool,
    pub vision: bool,
    pub json_mode: bool,
    pub reasoning: bool,
    pub usage: bool,
}
