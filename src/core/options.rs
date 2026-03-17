use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::tool::ToolChoice;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerateOptions {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
    pub json_schema: Option<Value>,
    pub tool_choice: Option<ToolChoice>,
    pub provider_options: Map<String, Value>,
}
