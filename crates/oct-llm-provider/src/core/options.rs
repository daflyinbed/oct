use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::tool::ToolChoice;

/// 用户选择的思考档位。值域（effort 的合法字符串、budget 的范围）由
/// 调用方按模型元数据校验，core 只负责语义形态；各 adapter 负责翻译
/// 成自家 wire 参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningRequest {
    /// 档位字符串（low/medium/high/...，按 provider 原样传递）。
    Effort { effort: String },
    /// toggle 型模型：开思考，不指定档位。
    Enabled,
    /// 显式思考 token 预算（anthropic budget_tokens / google thinkingBudget 一系）。
    BudgetTokens { budget: u32 },
}

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
    pub reasoning: Option<ReasoningRequest>,
    #[serde(flatten)]
    pub provider_options: Value,
}
