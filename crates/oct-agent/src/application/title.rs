//! Independent title preparation and persistence. The job owns a model
//! resolver and its own deadline, not the run service or a running toolset.
use std::sync::Weak;
use std::time::Duration;
use tracing::debug;

use crate::agent::title;
use crate::db::conversations as conv_db;
use crate::events::AgentEvent;
use crate::events::EventHub;
use crate::runtime::ModelResolver;

/// Hard timeout for the model call, including configuration preparation;
/// on expiry the fallback title is used.
const MODEL_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the background task owns. The hub is held WEAKLY on purpose:
/// holding an `Arc` would keep the run's hub (and with it every SSE body)
/// alive past the run's end until this call finishes or times out. If the run
/// is already gone when the title lands, publishing is skipped — the DB has
/// the title and the next conversation-list fetch picks it up.
pub struct TitleJob {
    pub resolver: ModelResolver,
    pub project_id: String,
    pub parent_provider_id: String,
    pub parent_model_id: String,
    pub pool: sqlx::SqlitePool,
    pub conversation_id: String,
    pub first_user_content: String,
    pub hub: Weak<EventHub>,
}

/// Fire-and-forget: detach a task that generates the title and applies it.
pub fn spawn(job: TitleJob) {
    tokio::spawn(async move {
        run(job).await;
    });
}

async fn run(job: TitleJob) {
    // Include configuration/model preparation in the independent title deadline.
    let generated = tokio::time::timeout(MODEL_TIMEOUT, call_model(&job))
        .await
        .map_err(|_| anyhow::anyhow!("title model call timed out"))
        .and_then(|result| result);
    let title = match &generated {
        Ok(text) => {
            title::normalize_title(text).or_else(|| title::fallback_title(&job.first_user_content))
        }
        Err(e) => {
            // High-frequency background path: debug only, never error-level.
            debug!("title generation failed, using fallback: {e}");
            title::fallback_title(&job.first_user_content)
        }
    };
    let Some(title) = title else {
        return;
    };

    // Write first, publish second: anyone who sees the event can also see
    // the row. `Ok(false)` means the user renamed in the meantime — their
    // name wins and no event is sent.
    match conv_db::set_auto_title(&job.pool, &job.conversation_id, &title).await {
        Ok(true) => {
            if let Some(hub) = job.hub.upgrade() {
                hub.publish(AgentEvent::TitleUpdated { title });
            }
        }
        Ok(false) => {}
        Err(e) => debug!("failed to persist generated title: {e}"),
    }
}

async fn call_model(job: &TitleJob) -> anyhow::Result<String> {
    let settings = crate::application::settings::SettingsApplication::new(job.pool.clone())
        .read_for_run("title", Some(&job.project_id))
        .await?;
    let (provider_id, model_id) = settings
        .effective
        .model
        .identity(&job.parent_provider_id, &job.parent_model_id);
    let (model, _, _) = job
        .resolver
        .resolve_model(&format!("{provider_id}:{model_id}"))
        .await?;
    title::generate_title(
        model.as_ref(),
        &settings.effective.instructions,
        &job.first_user_content,
        &job.conversation_id,
    )
    .await
}
