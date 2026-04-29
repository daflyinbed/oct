use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

use oct_llm_provider::core::{Message, Role};

use super::AppState;
use super::error::{AppError, ApiResult};
use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::{AgentContext, RunHandle};
use crate::db::{conversations as conv_db, messages as msg_db, providers as provider_db, projects as project_db};
use crate::tools;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub content: String,
    pub provider_spec: String,
}

fn parse_provider_spec(spec: &str) -> Result<(&str, &str), AppError> {
    spec.split_once(':')
        .ok_or_else(|| AppError::BadRequest(format!("Invalid provider_spec format: '{}'. Expected 'provider_id:model_id'", spec)))
}

async fn resolve_model(
    state: &AppState,
    provider_spec: &str,
) -> Result<(Box<dyn oct_llm_provider::model::ChatModel>, String, String), AppError> {
    let spec = provider_spec.to_string();

    let (provider_id, model_id) = parse_provider_spec(&spec)?;

    let provider = provider_db::get_provider(&state.pool, provider_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Provider '{}' not found", provider_id)))?;

    if provider.api_key.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Provider '{}' has no API key configured",
            provider_id
        )));
    }

    let model = match provider.adapter_type.as_str() {
        "anthropic" => state.registry.resolve_chat_model(&spec)?,
        "openai_compatible" => {
            let base_url = provider.base_url.as_deref().unwrap_or("");
            state.registry.resolve_custom_openai_model(
                provider_id,
                model_id,
                base_url,
                &provider.api_key,
            )?
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown adapter_type: '{}'",
                other
            )));
        }
    };

    Ok((model, provider_id.to_string(), model_id.to_string()))
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/messages",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "SSE stream of agent events"),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "Agent is already running for this conversation")
    ),
    tag = "chat"
)]
pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> ApiResult<impl IntoResponse> {
    let placeholder_cancel = CancellationToken::new();
    match state.sessions.entry(id.clone()) {
        dashmap::mapref::entry::Entry::Occupied(_) => {
            return Err(AppError::Conflict(
                "Agent is already running for this conversation".into(),
            ));
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(RunHandle {
                event_tx: broadcast::channel(256).0,
                cancel: placeholder_cancel.clone(),
            });
        }
    }

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Conversation not found".into())
        })?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(|e| {
            state.sessions.remove(&id);
            AppError::from(e)
        })?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Project not found".into())
        })?;

    let (chat_model, provider_id, model_id) =
        match resolve_model(&state, &req.provider_spec).await {
            Ok(m) => m,
            Err(e) => {
                state.sessions.remove(&id);
                return Err(e);
            }
        };

    let user_msg = Message::text(Role::User, &req.content);
    if let Err(e) = msg_db::insert_message(
        &state.pool,
        &id,
        &user_msg,
        Some(&provider_id),
        Some(&model_id),
    )
    .await
    {
        state.sessions.remove(&id);
        return Err(AppError::from(e));
    }

    let messages = match msg_db::load_messages_for_llm(&state.pool, &id).await {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    let (event_tx, _) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let handle = RunHandle {
        event_tx: event_tx.clone(),
        cancel,
    };

    let rx = handle.subscribe();
    if let Some(mut entry) = state.sessions.get_mut(&id) {
        *entry.value_mut() = handle.clone();
    }

    let ctx = AgentContext {
        model: Arc::from(chat_model),
        tools: Arc::new(tools::default_tools(PathBuf::from(&project.working_dir))),
        pool: state.pool.clone(),
        system_prompt: system_prompt(&PathBuf::from(&project.working_dir)),
    };

    let conv_id = id.clone();
    let sessions = state.sessions.clone();

    tokio::spawn(async move {
        run_agent_loop(ctx, conv_id.clone(), messages, handle).await;
        sessions.remove(&conv_id);
    });

    let stream = BroadcastStream::new(rx).map(|e| match e {
        Ok(e) => Ok::<_, Infallible>(
            Event::default()
                .json_data(&e)
                .unwrap_or_else(|_| Event::default()),
        ),
        Err(_) => Ok(Event::default()),
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[utoipa::path(
    get,
    path = "/conversations/{id}/messages",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "List of messages", body = Vec<msg_db::StoredMessage>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "chat"
)]
pub async fn get_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<msg_db::StoredMessage>>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let messages = msg_db::list_messages(&state.pool, &id).await?;

    Ok(Json(messages))
}
