//! 附件上传与读取端点。
//!
//! 上传是 multipart/form-data 单字段（`file`）：中间件层 `DefaultBodyLimit`
//! 是路由硬顶（见 `api_routes` 的挂载），handler 只做精确的产品语义校验
//! ——类型放行集、magic/文本嗅探、单文件精确上限——然后临时名落盘、
//! 原子改名、行进 `attachments` 表（staged）。发送（`api/chat.rs`）时行才
//! 被认领进消息；staged 未发送的行由 sweep 按 TTL 回收。

use axum::Json;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::multipart::Multipart;
use axum::http::{StatusCode, header};
use axum::response::Response;
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::attachments;
use crate::attachments::{ServiceError, normalize::image_mime};

pub use crate::attachment_types::AttachmentDto;

/// OpenAPI 文档形态（真实请求是 multipart 的单一 `file` 字段）。
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 仅 schema 文档用
pub struct AttachmentUploadDoc {
    #[schema(format = Binary)]
    pub file: String,
}

/// 展示用文件名：取最后一段路径成分并限长——DB 里的 filename 纯展示，
/// 永不参与磁盘路径，但也不能让恶意客户端往历史里塞路径或超长串。
fn display_filename(raw: Option<&str>) -> String {
    let name = raw
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .unwrap_or("file");
    name.chars().take(255).collect()
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/attachments",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body(content = AttachmentUploadDoc, content_type = "multipart/form-data", description = "Single `file` field; filename and Content-Type come from the browser"),
    responses(
        (status = 201, description = "Staged attachment (referenced by id in the next send_message)", body = AttachmentDto),
        (status = 400, description = "No file field, unsupported type (v1: images and UTF-8 text only), or over the size limit"),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "Managed subagent session or read-only (revert backup) conversation")
    ),
    tag = "chat"
)]
pub async fn upload_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<AttachmentDto>)> {
    ensure_writable_conversation(&state, &id).await?;

    let field = loop {
        let Some(field) = multipart.next_field().await.map_err(|_| upload_invalid())? else {
            return Err(AppError::BadRequest("缺少 file 字段".into()));
        };
        if field.name() == Some("file") {
            break field;
        }
    };

    let filename = display_filename(field.file_name());
    let mime = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();

    // Receive the multipart stream without retaining the publication lock.
    // The service owns validation, immutable asset publication and tickets.
    let attachment_id = Uuid::new_v4().to_string();
    let (upload, mut file) = state
        .attachments
        .begin_upload(&id, &attachment_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut field = field;
    let mut size = 0usize;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = field.chunk().await.map_err(|_| upload_invalid())? {
        size += chunk.len();
        if size > attachments::MAX_ATTACHMENT_BYTES as usize {
            return Err(upload_invalid());
        }
        file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
    }
    file.sync_all().await.map_err(anyhow::Error::from)?;
    drop(file);
    let dto = state
        .attachments
        .finish_upload(&id, &attachment_id, &filename, &mime, upload)
        .await
        .map_err(service_error)?;
    Ok((StatusCode::CREATED, Json(dto)))
}

pub(super) fn service_error(error: ServiceError) -> AppError {
    error.into()
}

async fn ensure_writable_conversation(state: &AppState, id: &str) -> ApiResult<()> {
    let conversation = crate::db::conversations::get_conversation(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    // 子代理会话不能直接发消息，也就没有附件的落点；只读备份同理。
    if conversation.kind == "subagent" {
        return Err(AppError::ManagedConversation(
            "Conversation is a managed subagent session".into(),
        ));
    }
    conversation.ensure_writable()
}

/// Restore a history draft without reusing an already claimed id. The copy
/// owns its bytes and staged row, so deleting the source cannot invalidate it.
#[utoipa::path(
    post,
    path = "/conversations/{id}/attachments/{attachment_id}/copy",
    params(
        ("id" = String, Path, description = "Target conversation ID"),
        ("attachment_id" = String, Path, description = "Sent source attachment ID")
    ),
    responses(
        (status = 201, description = "New staged attachment in the target conversation", body = AttachmentDto),
        (status = 400, description = "Source attachment is not sent or its bytes changed"),
        (status = 404, description = "Conversation or source attachment not found"),
        (status = 409, description = "Managed or read-only conversation")
    ),
    tag = "chat"
)]
pub async fn copy_attachment(
    State(state): State<AppState>,
    Path((id, attachment_id)): Path<(String, String)>,
) -> ApiResult<(StatusCode, Json<AttachmentDto>)> {
    ensure_writable_conversation(&state, &id).await?;
    let dto = state
        .attachments
        .copy_attachment(&id, &attachment_id)
        .await
        .map_err(service_error)?;
    Ok((StatusCode::CREATED, Json(dto)))
}

/// multipart 读取失败的统一出口：无效请求与超限都从这里出来——中间件的
/// body 上限错误也以流错误形态抵达这里。
fn upload_invalid() -> AppError {
    AppError::BadRequest("上传中断：请求无效或文件超过大小上限".into())
}

/// 已发送附件的字节读取：前端 `<img>` 与预览直连。内容按 id 寻址、永不
/// 变更，缓存头直接给 immutable。
#[utoipa::path(
    get,
    path = "/attachments/{id}",
    params(("id" = String, Path, description = "Attachment ID")),
    responses(
        (status = 200, description = "Attachment bytes", content_type = "application/octet-stream"),
        (status = 404, description = "Unknown attachment or missing file")
    ),
    tag = "chat"
)]
pub async fn get_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    // id 形状先校验：不是 uuid 的直接 404，省一次数据库往返。
    if Uuid::parse_str(&id).is_err() {
        return Err(AppError::NotFound("Attachment not found".into()));
    }
    let (file, mime) = state.attachments.open(&id).await.map_err(service_error)?;

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);
    // MIME 来自上传者。只有经过 magic 校验的图片保留图片类型；其他文件
    // 都以纯文本查看，HTML/XML 等附件不能在应用同源执行脚本。
    let content_type = if image_mime(&mime).is_some() {
        mime.as_str()
    } else {
        "text/plain; charset=utf-8"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(body)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("attachment response: {e}")))
}

// Image decoding, MIME validation and stable resizing live in the shared
// MediaNormalizer so uploads and tools obey the same ingress policy.

#[cfg(test)]
mod tests {
    use super::display_filename;

    #[test]
    fn filename_takes_last_component_and_is_capped() {
        assert_eq!(display_filename(Some("/etc/passwd")), "passwd");
        assert_eq!(display_filename(Some("C:\\dir\\a.png")), "a.png");
        assert_eq!(display_filename(Some("  ")), "file");
        assert_eq!(display_filename(None), "file");
        assert_eq!(
            display_filename(Some(&"长".repeat(400))).chars().count(),
            255
        );
    }
}
