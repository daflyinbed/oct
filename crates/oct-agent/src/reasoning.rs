//! models.dev `reasoning_options` 的类型化解释与用户档位校验。
//!
//! seed 把 models.dev 的档位描述原样透存为 JSON（见 `seed.rs`——新形态
//! 不应炸 seed），这里是唯一的解释点：
//!
//! `reasoning_options` 是控制项数组，一个模型可以同时有 effort 与预算：
//! - `{"type":"effort","values":["low","medium",...]}` → 档位列表
//! - `{"type":"toggle"}`                               → 开/关
//! - `{"type":"budget_tokens","min":n,"max":n}`        → token 预算区间
//!
//! 未来 models.dev 新增形态（或 JSON 损坏）→ [`ReasoningOptions::Unknown`]：
//! 校验放行任何用户选择，与 seed 的透存策略一致——不认识的数据不拦人。

use oct_llm_provider::core::ReasoningRequest;
use serde::Deserialize;

/// `models.reasoning_options` 列的解释结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningOptions {
    Known(Vec<ReasoningControl>),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningControl {
    Effort { values: Vec<String> },
    Toggle,
    BudgetTokens { min: Option<i64>, max: Option<i64> },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawReasoningOptions {
    Effort {
        #[serde(default)]
        values: Vec<Option<String>>,
    },
    Toggle,
    BudgetTokens {
        min: Option<i64>,
        max: Option<i64>,
    },
}

impl ReasoningOptions {
    pub fn parse(raw: &str) -> Self {
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Self::Unknown;
        };
        // 目录的正式格式是数组；兼容此前自定义模型写入的单个控制项。
        let values = match raw {
            serde_json::Value::Array(values) => values,
            value @ serde_json::Value::Object(_) => vec![value],
            _ => return Self::Unknown,
        };
        let empty = values.is_empty();
        let controls: Vec<_> = values
            .into_iter()
            .filter_map(
                |value| match serde_json::from_value::<RawReasoningOptions>(value) {
                    // values 里的 null 档位（models.dev 用它表达 "none"）不是合法的
                    // Effort 字符串选择，直接丢弃。
                    Ok(RawReasoningOptions::Effort { values }) => Some(ReasoningControl::Effort {
                        values: values.into_iter().flatten().collect(),
                    }),
                    Ok(RawReasoningOptions::Toggle) => Some(ReasoningControl::Toggle),
                    Ok(RawReasoningOptions::BudgetTokens { min, max }) => {
                        Some(ReasoningControl::BudgetTokens { min, max })
                    }
                    Err(_) => None,
                },
            )
            .collect();
        if empty || !controls.is_empty() {
            Self::Known(controls)
        } else {
            Self::Unknown
        }
    }

    /// 校验用户档位是否落在本模型的值域内。`Err` 携带可直接返回给
    /// 前端的错误消息（API 层转 400）。
    pub fn validate(&self, request: &ReasoningRequest) -> Result<(), String> {
        let Self::Known(controls) = self else {
            return Ok(());
        };
        controls
            .iter()
            .find_map(|control| match (control, request) {
                (ReasoningControl::Effort { values }, ReasoningRequest::Effort { effort }) => {
                    Some({
                        if values.iter().any(|v| v == effort) {
                            Ok(())
                        } else {
                            Err(format!(
                                "该模型的思考档位只支持 {}，不支持 '{effort}'",
                                values.join("/")
                            ))
                        }
                    })
                }
                (ReasoningControl::Toggle, ReasoningRequest::Enabled) => Some(Ok(())),
                (
                    ReasoningControl::BudgetTokens { min, max },
                    ReasoningRequest::BudgetTokens { budget },
                ) => Some({
                    // 目录的预算下限按模型校验；anthropic 的 1024 硬下限由 adapter 校验。
                    let floor = min.unwrap_or(0).max(0);
                    let budget = i64::from(*budget);
                    let within_min = budget >= floor;
                    let within_max = max.is_none_or(|m| budget <= m);
                    if within_min && within_max {
                        Ok(())
                    } else {
                        let range = match max {
                            Some(max) => format!("{floor}~{max}"),
                            None => format!("≥{floor}"),
                        };
                        Err(format!(
                            "该模型的思考预算需在 {range} tokens 之间，收到 {budget}"
                        ))
                    }
                }),
                _ => None,
            })
            .unwrap_or_else(|| Err("该模型不支持所选的思考控制方式".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_three_shapes() {
        assert_eq!(
            ReasoningOptions::parse(r#"[{"type":"effort","values":["low","high",null]}]"#),
            ReasoningOptions::Known(vec![ReasoningControl::Effort {
                values: vec!["low".into(), "high".into()]
            }])
        );
        assert_eq!(
            ReasoningOptions::parse(r#"[{"type":"toggle"}]"#),
            ReasoningOptions::Known(vec![ReasoningControl::Toggle])
        );
        assert_eq!(
            ReasoningOptions::parse(r#"[{"type":"budget_tokens","min":1024,"max":31999}]"#),
            ReasoningOptions::Known(vec![ReasoningControl::BudgetTokens {
                min: Some(1024),
                max: Some(31999)
            }])
        );
    }

    #[test]
    fn unknown_shapes_and_broken_json_degrade_to_unknown() {
        assert_eq!(
            ReasoningOptions::parse(r#"{"type":"quantum","qubits":8}"#),
            ReasoningOptions::Unknown
        );
        assert_eq!(
            ReasoningOptions::parse("not json"),
            ReasoningOptions::Unknown
        );
    }

    #[test]
    fn effort_accepts_listed_values_only() {
        let opts = ReasoningOptions::parse(r#"{"type":"effort","values":["low","high"]}"#);
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "high".into()
            })
            .is_ok()
        );
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "xhigh".into()
            })
            .is_err()
        );
        assert!(opts.validate(&ReasoningRequest::Enabled).is_err());
    }

    #[test]
    fn toggle_accepts_enabled_only() {
        let opts = ReasoningOptions::parse(r#"{"type":"toggle"}"#);
        assert!(opts.validate(&ReasoningRequest::Enabled).is_ok());
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "low".into()
            })
            .is_err()
        );
    }

    #[test]
    fn budget_checks_catalog_range() {
        let opts = ReasoningOptions::parse(r#"{"type":"budget_tokens","min":2048,"max":31999}"#);
        assert!(
            opts.validate(&ReasoningRequest::BudgetTokens { budget: 8192 })
                .is_ok()
        );
        // 低于目录下限的预算不能送到 provider。
        assert!(
            opts.validate(&ReasoningRequest::BudgetTokens { budget: 512 })
                .is_err()
        );
        assert!(
            opts.validate(&ReasoningRequest::BudgetTokens { budget: 99_999 })
                .is_err()
        );
    }

    #[test]
    fn unknown_passes_everything_through() {
        let opts = ReasoningOptions::Unknown;
        assert!(opts.validate(&ReasoningRequest::Enabled).is_ok());
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "any".into()
            })
            .ok()
            .is_some()
        );
    }

    #[test]
    fn catalog_arrays_support_multiple_controls_without_bypassing_validation() {
        let opts = ReasoningOptions::parse(
            r#"[{"type":"effort","values":["high","max"]},{"type":"budget_tokens","min":1024},{"type":"future"}]"#,
        );
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "max".into()
            })
            .is_ok()
        );
        assert!(
            opts.validate(&ReasoningRequest::BudgetTokens { budget: 8192 })
                .is_ok()
        );
        assert!(
            opts.validate(&ReasoningRequest::Effort {
                effort: "low".into()
            })
            .is_err()
        );
        assert!(opts.validate(&ReasoningRequest::Enabled).is_err());
        assert!(
            ReasoningOptions::parse("[]")
                .validate(&ReasoningRequest::Enabled)
                .is_err()
        );
    }
}
