use oct_llm_provider::core::ReasoningRequest;

use crate::application::services::ApplicationServices;
use crate::db::messages as msg_db;
use crate::error::AppError;

// Send preparation rejects dangling managed delegations before repairing
// ordinary tool calls; the application preparation module owns that rule.
/// 校验请求携带的思考档位与 models 表的记录一致——fail fast（400），
/// 避免档位落到 wire 层才被 provider 拒绝。DB 缺少目录元数据时仍校验
/// adapter 支持范围与预算上限，但不猜测 effort 的合法值。
pub(super) async fn validate_reasoning(
    runs: &ApplicationServices,
    provider_id: &str,
    model_id: &str,
    reasoning: &ReasoningRequest,
) -> Result<(), AppError> {
    if !matches!(reasoning, ReasoningRequest::Effort { .. }) {
        let provider = crate::db::providers::get_provider(&runs.pool, provider_id)
            .await
            .map_err(AppError::from)?;
        if provider.is_none_or(|p| p.adapter_type != "anthropic") {
            return Err(AppError::BadRequest(
                "此 provider 的适配器仅支持 effort 档位，暂不支持思考开关或 token 预算".into(),
            ));
        }
    }
    if let ReasoningRequest::BudgetTokens { budget } = reasoning
        && *budget < 1024
    {
        return Err(AppError::BadRequest(
            "Anthropic 思考预算必须至少为 1024".into(),
        ));
    }
    let row = crate::db::models::get_model(&runs.pool, provider_id, model_id)
        .await
        .map_err(AppError::from)?;
    if let ReasoningRequest::BudgetTokens { budget } = reasoning {
        let limit = row
            .as_ref()
            .and_then(|row| row.limit_output)
            .filter(|limit| *limit > 0)
            .unwrap_or(8192);
        if i64::from(*budget) >= limit {
            return Err(AppError::BadRequest(
                "Anthropic 思考预算必须小于模型的输出上限".into(),
            ));
        }
    }
    let Some(row) = row else {
        return Ok(());
    };
    if !row.reasoning {
        return Err(AppError::BadRequest(format!(
            "模型 {provider_id}:{model_id} 不支持思考（reasoning），请移除思考档位"
        )));
    }
    if let Some(raw) = &row.reasoning_options
        && let Err(message) = crate::reasoning::ReasoningOptions::parse(raw).validate(reasoning)
    {
        return Err(AppError::BadRequest(message));
    }
    Ok(())
}

/// How long the conversation sat idle before this request, per the stored
/// history's last message timestamp.
pub(super) async fn conversation_idle(
    runs: &ApplicationServices,
    id: &str,
) -> Option<std::time::Duration> {
    msg_db::last_activity(&runs.pool, id)
        .await
        .ok()
        .flatten()
        .and_then(|last| (chrono::Utc::now().naive_utc() - last).to_std().ok())
}
