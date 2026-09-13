use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub cached_input_tokens: Option<u32>,
    pub provider_details: Map<String, Value>,
}

impl Usage {
    pub fn new(
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        total_tokens: Option<u32>,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens,
            reasoning_tokens: None,
            cached_input_tokens: None,
            provider_details: Map::new(),
        }
    }
}
