use crate::agent::compact;
use crate::application::services::ApplicationServices;
use crate::context::{self, LlmView};
use crate::db::{conversations as conv_db, messages as msg_db};
use crate::error::{AppError, AppResult};
use crate::events::CompactionTrigger;

use super::{CompactCommand, CompactOutcome, ensure_not_managed, preparation};

pub(super) async fn compact(
    runs: &ApplicationServices,
    command: CompactCommand,
) -> AppResult<CompactOutcome> {
    let id = command.conversation_id.clone();
    // The session entry doubles as the compaction mutex: an active run OR a
    // concurrent manual compact gets a 409. The occupancy guard releases the
    // entry on drop at every exit path below (a synchronous request, unlike
    // the SSE run endpoints).
    let _guard = runs.occupancy().occupy(&id)?;

    let conversation = conv_db::get_conversation(&runs.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    // Managed subagent sessions compact with their parent's history — the
    // wait for their pending tasks would be lost otherwise.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;
    // A root conversation with crashed subagent tasks must recover first:
    // compacting would summarize past the very calls whose results are still
    // owed.
    preparation::ensure_no_pending_recovery(&runs.pool, &id).await?;

    run_manual_compact(runs, &id, command.provider_spec.as_deref()).await
}

async fn run_manual_compact(
    runs: &ApplicationServices,
    id: &str,
    provider_spec: Option<&str>,
) -> AppResult<CompactOutcome> {
    conv_db::get_conversation(&runs.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let spec = match provider_spec {
        Some(s) => s.to_string(),
        None => match msg_db::get_last_provider_spec(&runs.pool, id).await? {
            Some((provider_id, model_id)) => format!("{provider_id}:{model_id}"),
            None => {
                return Err(AppError::BadRequest(
                    "Conversation has no recorded provider/model; pass provider_spec".into(),
                ));
            }
        },
    };
    let (chat_model, provider_id, model_id) =
        runs.assembly_services().models.resolve_model(&spec).await?;
    let window = runs
        .assembly_services()
        .models
        .resolve_context_window(chat_model.as_ref(), &provider_id, &model_id)
        .await;

    let view = msg_db::load_llm_view(&runs.pool, id).await?;
    let LlmView {
        messages,
        orderings,
        anchor,
    } = view;
    let entries: Vec<context::ContextEntry> = messages
        .into_iter()
        .zip(orderings)
        .map(|(message, ordering)| context::ContextEntry { message, ordering })
        .collect();

    // Display-only figures: the anchor is used when present so the "before"
    // number matches what the provider actually charged, falling back to a
    // message-only estimate (no tool schemas at hand outside a run — the
    // est_after 口径 is documented on `compact::CompactRequest`).
    let est_before = context::estimate_view(&entries, None, &[], anchor.as_ref());

    // The shared compaction transaction (same cut/tail/summary/persist rules
    // as the in-run auto trigger); no hub (no SSE stream here) and no
    // cancellation token — a synchronous request. Tagged with the
    // conversation so the manual path's wire lines land in the right
    // per-conversation file (the run path inherits them from the supervisor).
    // The prompt-env handle flags the frozen instruction block dirty on
    // success — the next run re-reads the files.
    let compaction = oct_llm_provider::wire::with_tags(
        oct_llm_provider::wire::Tags {
            conversation_id: Some(id.to_string()),
            ..Default::default()
        },
        compact::run_compaction(
            chat_model.as_ref(),
            &runs.pool,
            id,
            compact::CompactRequest {
                entries: &entries,
                est_before,
                system_prompt: None,
                tool_specs: &[],
                window,
                trigger: CompactionTrigger::Manual,
                hub: None,
                cancel: None,
                prompt_env_refresh: Some(runs.assembly_services().prompt_env_refresh(id)),
            },
        ),
    );
    let outcome = match compaction.await {
        Ok(outcome) => outcome,
        Err(compact::CompactFailure::Nothing) => {
            return Err(AppError::BadRequest(
                "Conversation is too short to compact".into(),
            ));
        }
        Err(compact::CompactFailure::Summary(e)) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Compaction summary failed: {e}"
            )));
        }
        Err(compact::CompactFailure::Persist(e)) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Compaction failed to persist the marker row: {e}"
            )));
        }
        Err(compact::CompactFailure::Cancelled) => {
            unreachable!("manual compaction passes no cancellation token")
        }
    };

    Ok(CompactOutcome {
        summary_message_id: outcome.summary_message_id,
        compacted_messages: outcome.cut as u32,
        est_tokens_before: outcome.est_before,
        est_tokens_after: outcome.est_after,
        summary: outcome.summary,
    })
}
