use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::tool::ToolChoice;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerateOptions {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
    pub json_schema: Option<Value>,
    pub tool_choice: Option<ToolChoice>,
    pub n: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    #[serde(flatten)]
    pub provider_options: Value,
}
