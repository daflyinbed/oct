//! Terminal REST lifecycle and an exclusive, resumable WebSocket attachment.
//! Client: binary = input bytes; text = {type:"resize",cols,rows} or
//! {type:"ack",offset} after rendering output and sending any query replies.
//! Server: binary = 8-byte BE output offset + raw PTY bytes; text = lifecycle.
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures_util::SinkExt;
use portable_pty::PtySize;
use serde::Deserialize;
use serde_json::json;
use utoipa::{IntoParams, ToSchema};

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::terminal::{MAX_INPUT_BYTES, TerminalInfo, TerminalSession, validate_size};

#[derive(Deserialize, ToSchema)]
pub struct CreateTerminalRequest {
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Deserialize, IntoParams)]
pub struct TerminalStreamQuery {
    /// Next expected output byte. Omit to replay the available history.
    pub after: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { cols: u16, rows: u16 },
    Ack { offset: u64 },
}

/// Browsers attach Origin to WebSocket handshakes and cross-origin fetches.
/// Check it here even though the rest of the API currently permits CORS.
/// Non-browser clients without Origin retain the regular API behavior.
fn check_origin(headers: &HeaderMap) -> ApiResult<()> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let valid = (|| {
        let origin = url::Url::parse(origin.to_str().ok()?).ok()?;
        if !matches!(origin.scheme(), "http" | "https") {
            return None;
        }
        let host = headers.get(header::HOST)?.to_str().ok()?;
        let expected = url::Url::parse(&format!("{}://{host}", origin.scheme())).ok()?;
        Some(origin.origin() == expected.origin())
    })();
    if valid != Some(true) {
        return Err(AppError::BadRequest(
            "Cross-origin terminal access is not allowed".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(
    post, path = "/projects/{project_id}/terminals",
    params(("project_id" = String, Path)), request_body = CreateTerminalRequest,
    responses((status = 201, body = TerminalInfo), (status = 400, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody), (status = 409, body = super::error::ErrorBody)),
    tag = "terminals"
)]
pub async fn create_terminal(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CreateTerminalRequest>,
) -> ApiResult<(StatusCode, Json<TerminalInfo>)> {
    check_origin(&headers)?;
    validate_size(request.cols, request.rows).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let admission = state.runs.lifecycle.admission.clone().read_owned().await;
    state.runs.lifecycle.check("project", &project_id)?;
    let project = crate::db::projects::get_project(&state.pool, &project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;
    let cwd = tokio::fs::canonicalize(&project.working_dir)
        .await
        .map_err(|e| AppError::BadRequest(format!("Invalid working directory: {e}")))?;
    if !cwd.is_dir() {
        return Err(AppError::BadRequest(
            "Working directory is not a directory".into(),
        ));
    }
    let manager = state.terminals.clone();
    let info = tokio::task::spawn_blocking(move || {
        // A blocking task survives cancellation of the request. Keep deletion
        // excluded until the shell has been registered even in that case.
        let _admission = admission;
        manager.create(
            &project_id,
            &cwd,
            PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?
    .map_err(|e| AppError::Conflict(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(info)))
}

#[utoipa::path(
    get, path = "/projects/{project_id}/terminals",
    params(("project_id" = String, Path)),
    responses((status = 200, body = Vec<TerminalInfo>), (status = 404, body = super::error::ErrorBody)),
    tag = "terminals"
)]
pub async fn list_terminals(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<TerminalInfo>>> {
    check_origin(&headers)?;
    if crate::db::projects::get_project(&state.pool, &project_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Project not found".into()));
    }
    Ok(Json(state.terminals.list(&project_id)))
}

#[utoipa::path(
    delete, path = "/projects/{project_id}/terminals/{id}",
    params(("project_id" = String, Path), ("id" = String, Path)),
    responses((status = 204), (status = 404, body = super::error::ErrorBody)), tag = "terminals"
)]
pub async fn delete_terminal(
    State(state): State<AppState>,
    Path((project_id, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    check_origin(&headers)?;
    let manager = state.terminals.clone();
    let removed = tokio::task::spawn_blocking(move || manager.remove(&project_id, &id))
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !removed {
        return Err(AppError::NotFound("Terminal not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/projects/{project_id}/terminals/{id}/ws",
    params(("project_id" = String, Path), ("id" = String, Path), TerminalStreamQuery),
    responses((status = 101, description = "PTY stream: binary output has an 8-byte BE offset header; JSON ready/live/exit/error control frames"),
        (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)), tag = "terminals"
)]
pub async fn attach_terminal(
    State(state): State<AppState>,
    Path((project_id, id)): Path<(String, String)>,
    Query(query): Query<TerminalStreamQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> ApiResult<Response> {
    check_origin(&headers)?;
    let session = state
        .terminals
        .get(&project_id, &id)
        .ok_or_else(|| AppError::NotFound("Terminal not found".into()))?;
    let permit = session
        .controller
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Conflict("Terminal is already attached in another view".into()))?;
    let after = query.after.unwrap_or(0);
    if after > session.snapshot(after).end {
        return Err(AppError::BadRequest(
            "Output cursor exceeds terminal output".into(),
        ));
    }
    Ok(upgrade
        .max_message_size(MAX_INPUT_BYTES)
        .max_frame_size(MAX_INPUT_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            stream(socket, session, after).await;
        }))
}

async fn send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(10), socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn control(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    send(socket, Message::Text(value.to_string().into())).await
}

async fn stream(mut socket: WebSocket, session: Arc<TerminalSession>, mut cursor: u64) {
    let initial = session.snapshot(cursor);
    let size = session.size().unwrap_or_default();
    let truncated = cursor < initial.start;
    cursor = cursor.max(initial.start);
    if !control(
        &mut socket,
        json!({ "type": "ready", "offset": cursor,
        "cols": size.cols, "rows": size.rows, "truncated": truncated,
        "replay_reply_after": session.acknowledged_output() }),
    )
    .await
    {
        return;
    }
    let mut replaying = true;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut last_received = tokio::time::Instant::now();
    loop {
        // Register before reading state: notify_waiters must not race the
        // snapshot and leave an attachment asleep after the final output.
        let notified = session.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let snapshot = session.snapshot(cursor);
        if cursor < snapshot.start {
            control(&mut socket, json!({ "type": "error", "message": "Terminal output exceeded the replay buffer; reconnect to resume" })).await;
            break;
        }
        for (offset, bytes) in snapshot.chunks {
            let mut frame = Vec::with_capacity(8 + bytes.len());
            frame.extend_from_slice(&offset.to_be_bytes());
            frame.extend_from_slice(&bytes);
            if !send(&mut socket, Message::Binary(frame.into())).await {
                return;
            }
            cursor = offset + bytes.len() as u64;
        }
        if let Some(error) = snapshot.error {
            control(&mut socket, json!({ "type": "error", "message": error })).await;
            break;
        }
        if snapshot.finished {
            control(
                &mut socket,
                json!({ "type": "exit", "exit_code": snapshot.exit_code }),
            )
            .await;
            break;
        }
        if replaying {
            replaying = false;
            if !control(&mut socket, json!({ "type": "live" })).await {
                break;
            }
        }
        tokio::select! {
            _ = &mut notified => {},
            _ = heartbeat.tick() => {
                if last_received.elapsed() >= Duration::from_secs(60) { break; }
                if !send(&mut socket, Message::Ping(Vec::new().into())).await { break; }
            },
            message = socket.recv() => {
                last_received = tokio::time::Instant::now();
                let result = match message {
                    Some(Ok(Message::Binary(bytes))) => session.write(bytes.to_vec()).await,
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Control>(&text) {
                            Ok(Control::Resize { cols, rows }) => session.resize(PtySize {
                                cols, rows, pixel_width: 0, pixel_height: 0,
                            }).await,
                            Ok(Control::Ack { offset }) if offset <= cursor => {
                                session.acknowledge_output(offset);
                                Ok(())
                            },
                            Ok(Control::Ack { .. }) => Err(anyhow::anyhow!("Output acknowledgement exceeds sent output")),
                            Err(error) => Err(error.into()),
                        }
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    _ => break,
                };
                if let Err(error) = result {
                    control(&mut socket, json!({ "type": "error", "message": error.to_string() })).await;
                    break;
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
}
