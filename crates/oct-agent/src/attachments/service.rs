//! Attachment application operations. The service alone coordinates filesystem
//! publication, upload tickets, immutable content and reference-driven GC.
//! Network uploads pin their paths instead of holding the publication lock;
//! local publication and message commit hold a short read lease, GC a write lease.
use super::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_MESSAGE, normalize, store::AssetStore, text,
};
use crate::attachment_types::{AttachmentDto, UserContent, attachment_url, parse_attachment_id};
use crate::db::{
    attachments as db,
    messages::{self, StoredMessage},
};
use base64::Engine;
use dashmap::{DashMap, DashSet};
use oct_llm_provider::core::{ContentPart, Message};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::{RwLock, RwLockReadGuard};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
impl From<db::ClaimFailure> for ServiceError {
    fn from(error: db::ClaimFailure) -> Self {
        match error {
            db::ClaimFailure::NotStaged => Self::Invalid(error.to_string()),
            db::ClaimFailure::Db(error) => Self::Internal(error),
        }
    }
}
impl From<sqlx::Error> for ServiceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.into())
    }
}
impl From<std::io::Error> for ServiceError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(error.into())
    }
}

#[derive(Debug, Clone)]
pub struct AttachmentService {
    store: AssetStore,
    pool: SqlitePool,
    publication: Arc<RwLock<()>>,
    uploads: Arc<DashSet<PathBuf>>,
    resources: Arc<DashMap<String, Weak<DashMap<String, PathBuf>>>>,
}

/// Pins an upload's paths without holding a publication lock over network
/// reads. Drop removes failed/cancelled partial files; orphan final files are
/// reclaimed by the next sweep if DB publication failed.
pub struct UploadGuard {
    uploads: Arc<DashSet<PathBuf>>,
    temp: PathBuf,
    final_path: PathBuf,
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
        self.uploads.remove(&self.temp);
        self.uploads.remove(&self.final_path);
    }
}

/// The private lease cannot be forgotten between preparation and commit.
/// All filesystem reads finish before a SQLite write transaction is opened.
pub struct PreparedUserMessage<'a> {
    service: &'a AttachmentService,
    _publication: RwLockReadGuard<'a, ()>,
    conversation_id: String,
    ids: Vec<String>,
    content: UserContent,
}
impl PreparedUserMessage<'_> {
    pub fn content(&self) -> &UserContent {
        &self.content
    }

    pub async fn commit(
        self,
        provider_id: &str,
        model_id: &str,
        mut details: Value,
    ) -> Result<StoredMessage, ServiceError> {
        details["user_content"] =
            serde_json::to_value(&self.content).map_err(anyhow::Error::from)?;
        // Keep the v1 metadata field readable by older clients. Domain code
        // uses the explicit UserContent contract rather than array arithmetic.
        if !self.content.attachments.is_empty() {
            details["attachments"] = json!(
                self.content
                    .attachments
                    .iter()
                    .map(|a| &a.asset)
                    .collect::<Vec<_>>()
            );
        }
        let message = self.content.storage_message();
        if self.ids.is_empty() {
            Ok(messages::insert_message(
                &self.service.pool,
                &self.conversation_id,
                &message,
                Some(provider_id),
                Some(model_id),
                Some(&details),
            )
            .await?)
        } else {
            Ok(db::claim_and_insert_message(
                &self.service.pool,
                &self.conversation_id,
                &self.ids,
                &message,
                Some(provider_id),
                Some(model_id),
                Some(&details),
            )
            .await?)
        }
    }
}

pub(crate) struct PreparedToolMessage<'a> {
    service: &'a AttachmentService,
    _publication: RwLockReadGuard<'a, ()>,
    conversation_id: String,
    ids: Vec<String>,
    pub output: crate::tool_contract::ToolOutput,
    pub message: Message,
}
impl PreparedToolMessage<'_> {
    pub async fn commit(&self) -> anyhow::Result<StoredMessage> {
        let stored = if self.ids.is_empty() {
            messages::insert_message(
                &self.service.pool,
                &self.conversation_id,
                &self.message,
                None,
                None,
                self.output.details.as_ref(),
            )
            .await?
        } else {
            db::claim_and_insert_message(
                &self.service.pool,
                &self.conversation_id,
                &self.ids,
                &self.message,
                None,
                None,
                self.output.details.as_ref(),
            )
            .await?
        };
        // Only committed media joins the live conversation's read grants;
        // uploading a draft must not authorize it for any running model.
        if !self.ids.is_empty()
            && let Some(resources) = self
                .service
                .resources
                .get(&self.conversation_id)
                .and_then(|entry| entry.value().upgrade())
        {
            for asset in db::assets_for_references(&self.service.pool, &self.ids).await? {
                resources.insert(
                    attachment_url(&asset.id),
                    self.service.store.path(&asset.storage_key)?,
                );
            }
        }
        Ok(stored)
    }
}

impl AttachmentService {
    pub fn new(root: PathBuf, pool: SqlitePool) -> Self {
        Self {
            store: AssetStore::new(root),
            pool,
            publication: Arc::new(RwLock::new(())),
            uploads: Arc::new(DashSet::new()),
            resources: Arc::new(DashMap::new()),
        }
    }
    pub fn root(&self) -> &Path {
        self.store.root()
    }
    pub async fn ensure_root(&self) -> std::io::Result<()> {
        self.store.ensure_root().await
    }
    pub async fn ensure_dir(&self, conversation_id: &str) -> std::io::Result<()> {
        self.store.ensure_dir(conversation_id).await
    }
    /// Storage inspection for diagnostics/tests; production consumers resolve
    /// resource ids through the service, never compose conversation paths.
    pub fn upload_path(&self, conversation_id: &str, id: &str) -> PathBuf {
        self.store
            .path(&format!("{conversation_id}/{id}"))
            .expect("system-generated storage path")
    }
    pub async fn begin_upload(
        &self,
        conversation_id: &str,
        id: &str,
    ) -> std::io::Result<(UploadGuard, tokio::fs::File)> {
        let _publication = self.publication.read().await;
        self.store.ensure_dir(conversation_id).await?;
        let key = format!("{conversation_id}/{id}");
        let temp = format!("{key}.part");
        let file = self.store.create(&temp).await?;
        let temp = self.store.path(&temp)?;
        let final_path = self.store.path(&key)?;
        self.uploads.insert(temp.clone());
        self.uploads.insert(final_path.clone());
        Ok((
            UploadGuard {
                uploads: self.uploads.clone(),
                temp,
                final_path,
            },
            file,
        ))
    }
    /// Multipart decoding is transport work; all validation, normalization and
    /// atomic publication of the completed upload live here.
    pub async fn finish_upload(
        &self,
        conversation_id: &str,
        id: &str,
        filename: &str,
        mime: &str,
        _upload: UploadGuard,
    ) -> Result<AttachmentDto, ServiceError> {
        let key = format!("{conversation_id}/{id}");
        let temp = format!("{key}.part");
        let bytes = self.store.read(&temp).await?;
        if bytes.is_empty() {
            return Err(ServiceError::Invalid("空文件".into()));
        }
        if bytes.len() as i64 > MAX_ATTACHMENT_BYTES {
            return Err(ServiceError::Invalid("文件超过大小上限（10 MiB）".into()));
        }
        if let Some(reason) = normalize::unsupported_mime_reason(mime) {
            return Err(ServiceError::Invalid(reason.into()));
        }
        let canonical_mime = normalize::image_mime(mime)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                mime.split(';')
                    .next()
                    .unwrap_or("application/octet-stream")
                    .trim()
                    .to_ascii_lowercase()
            });
        let normalized = if normalize::image_mime(mime).is_some() {
            Some(
                normalize::normalize_image(bytes.clone(), canonical_mime.clone())
                    .await
                    .map_err(ServiceError::Invalid)?,
            )
        } else {
            normalize::validate_text(&bytes).map_err(ServiceError::Invalid)?;
            None
        };
        let _publication = self.publication.read().await;
        let (model_key, model_mime, model_size, width, height, version) = match normalized {
            Some(image) if image.bytes != bytes || image.mime_type != canonical_mime => {
                use tokio::io::AsyncWriteExt;
                let model_key = format!("{key}.model");
                let model_temp = format!("{key}.model.part");
                let mut file = self.store.create(&model_temp).await?;
                file.write_all(&image.bytes).await?;
                file.sync_all().await?;
                drop(file);
                self.store.publish(&model_temp, &model_key).await?;
                (
                    model_key,
                    image.mime_type,
                    image.bytes.len() as i64,
                    Some(i64::from(image.width)),
                    Some(i64::from(image.height)),
                    normalize::NORMALIZER_VERSION,
                )
            }
            Some(image) => (
                key.clone(),
                canonical_mime.clone(),
                bytes.len() as i64,
                Some(i64::from(image.width)),
                Some(i64::from(image.height)),
                normalize::NORMALIZER_VERSION,
            ),
            None => (
                key.clone(),
                canonical_mime.clone(),
                bytes.len() as i64,
                None,
                None,
                0,
            ),
        };
        self.store.publish(&temp, &key).await?;
        let asset = db::AssetRow {
            id: id.into(),
            storage_key: key,
            mime_type: canonical_mime.clone(),
            size_bytes: bytes.len() as i64,
            model_storage_key: model_key,
            model_mime_type: model_mime,
            model_size_bytes: model_size,
            width,
            height,
            normalizer_version: version,
            created_at: chrono::Utc::now().naive_utc(),
        };
        let ticket = db::AttachmentRow {
            id: id.into(),
            conversation_id: conversation_id.into(),
            filename: filename.into(),
            mime_type: canonical_mime.clone(),
            size_bytes: asset.size_bytes,
            state: "staged".into(),
            created_at: asset.created_at,
            asset_id: id.into(),
        };
        db::publish_upload(&self.pool, &asset, &ticket).await?;
        Ok(AttachmentDto {
            id: id.into(),
            filename: filename.into(),
            mime_type: canonical_mime,
            size_bytes: asset.size_bytes,
        })
    }
    pub async fn copy_attachment(
        &self,
        conversation_id: &str,
        reference: &str,
    ) -> Result<AttachmentDto, ServiceError> {
        let _publication = self.publication.read().await;
        let asset = self.asset(reference).await?;
        if !db::is_referenced(&self.pool, &asset.id).await? {
            return Err(ServiceError::Invalid("只能从已发送的附件恢复草稿".into()));
        }
        let filename = db::filename_for_asset(&self.pool, &asset.id)
            .await?
            .unwrap_or_else(|| "file".into());
        let path = self.store.path(&asset.storage_key)?;
        if tokio::fs::metadata(path)
            .await
            .map_err(|_| ServiceError::NotFound("Attachment file not found".into()))?
            .len() as i64
            != asset.size_bytes
        {
            return Err(ServiceError::Invalid("附件文件已改变，请重新上传".into()));
        }
        let id = Uuid::new_v4().to_string();
        let ticket = db::AttachmentRow {
            id: id.clone(),
            conversation_id: conversation_id.into(),
            filename: filename.clone(),
            mime_type: asset.mime_type.clone(),
            size_bytes: asset.size_bytes,
            state: "staged".into(),
            created_at: chrono::Utc::now().naive_utc(),
            asset_id: asset.id,
        };
        db::copy_ticket(&self.pool, &ticket).await?;
        Ok(AttachmentDto {
            id,
            filename,
            mime_type: asset.mime_type,
            size_bytes: asset.size_bytes,
        })
    }
    pub async fn prepare_message(
        &self,
        conversation_id: &str,
        body: &str,
        ids: &[String],
        window: Option<u32>,
    ) -> Result<PreparedUserMessage<'_>, ServiceError> {
        if ids.len() > MAX_ATTACHMENTS_PER_MESSAGE {
            return Err(ServiceError::Invalid(format!(
                "单条消息最多 {MAX_ATTACHMENTS_PER_MESSAGE} 个附件"
            )));
        }
        let publication = self.publication.read().await;
        let rows = db::load_staged(&self.pool, conversation_id, ids).await?;
        let content = text::build_content(self, &rows, body, window).await?;
        Ok(PreparedUserMessage {
            service: self,
            _publication: publication,
            conversation_id: conversation_id.into(),
            ids: ids.into(),
            content,
        })
    }
    /// Every tool source (direct MCP, code mode and builtins) enters this
    /// common boundary before its result is persisted or sent to the model.
    pub(crate) async fn prepare_tool_result(
        &self,
        conversation_id: &str,
        call_id: &str,
        mut output: crate::tool_contract::ToolOutput,
    ) -> PreparedToolMessage<'_> {
        let mut ids = Vec::new();
        let mut images = Vec::new();
        for (index, image) in std::mem::take(&mut output.images).into_iter().enumerate() {
            let prepared = async {
                if image.data.len() > (MAX_ATTACHMENT_BYTES as usize).div_ceil(3) * 4 {
                    return Err(ServiceError::Invalid("工具图片超过大小上限".into()));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .map_err(|_| ServiceError::Invalid("工具图片不是有效 base64".into()))?;
                let id = Uuid::new_v4().to_string();
                let (upload, mut file) = self.begin_upload(conversation_id, &id).await?;
                use tokio::io::AsyncWriteExt;
                file.write_all(&bytes).await?;
                file.sync_all().await?;
                drop(file);
                let dto = self
                    .finish_upload(
                        conversation_id,
                        &id,
                        &format!("{call_id}-{index}"),
                        &image.mime_type,
                        upload,
                    )
                    .await?;
                Ok::<_, ServiceError>(dto)
            }
            .await;
            match prepared {
                Ok(dto) => {
                    ids.push(dto.id.clone());
                    images.push(crate::tool_contract::ToolImage {
                        mime_type: dto.mime_type,
                        data: attachment_url(&dto.id),
                    });
                }
                Err(error) => {
                    output
                        .content
                        .push_str(&format!("\n[image attachment omitted: {error}]"));
                }
            }
        }
        output.images = images;
        let message = Message {
            role: oct_llm_provider::core::Role::Tool,
            parts: vec![ContentPart::ToolResult(
                oct_llm_provider::core::ToolResult {
                    call_id: call_id.into(),
                    content: Value::String(output.content.clone()),
                    is_error: output.is_error,
                    images: output.images.clone().into_iter().map(From::from).collect(),
                },
            )],
        };
        PreparedToolMessage {
            service: self,
            _publication: self.publication.read().await,
            conversation_id: conversation_id.into(),
            ids,
            output,
            message,
        }
    }
    pub async fn read_file(&self, row: &db::AttachmentRow) -> std::io::Result<Vec<u8>> {
        let asset = self
            .asset(&row.asset_id)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.store.read(&asset.storage_key).await
    }
    pub async fn asset(&self, reference: &str) -> Result<db::AssetRow, ServiceError> {
        db::asset_for_reference(&self.pool, reference)
            .await?
            .ok_or_else(|| ServiceError::NotFound("Attachment not found".into()))
    }
    pub async fn open(&self, reference: &str) -> Result<(tokio::fs::File, String), ServiceError> {
        let asset = self.asset(reference).await?;
        let file = tokio::fs::File::open(self.store.path(&asset.storage_key)?)
            .await
            .map_err(|_| ServiceError::NotFound("Attachment file not found".into()))?;
        Ok((file, asset.mime_type))
    }
    pub async fn resource_paths(
        &self,
        conversation_id: &str,
    ) -> Result<Arc<DashMap<String, PathBuf>>, ServiceError> {
        let resources = {
            let mut entry = self.resources.entry(conversation_id.into()).or_default();
            match entry.upgrade() {
                Some(resources) => resources,
                None => {
                    let resources = Arc::new(DashMap::new());
                    *entry = Arc::downgrade(&resources);
                    resources
                }
            }
        };
        for (id, key) in db::resource_paths(&self.pool, conversation_id).await? {
            resources.insert(attachment_url(&id), self.store.path(&key)?);
        }
        Ok(resources)
    }
    pub async fn assets(&self, ids: &[String]) -> Result<Vec<db::AssetRow>, ServiceError> {
        Ok(db::assets_for_references(&self.pool, ids).await?)
    }
    /// Gate/budget selection happens before this IO-only materialization pass.
    /// A missing file is an explicit omission in the final projection plan.
    pub async fn hydrate_messages(&self, messages: Vec<Message>) -> Vec<Message> {
        self.hydrate_with_failures(messages).await.0
    }
    pub(super) async fn hydrate_with_failures(
        &self,
        mut messages: Vec<Message>,
    ) -> (Vec<Message>, HashSet<String>) {
        let mut missing_ids = HashSet::new();
        for message in &mut messages {
            for part in &mut message.parts {
                if let ContentPart::ImageUrl { url } = part
                    && let Some(id) = parse_attachment_id(url)
                {
                    let result = async {
                        let asset = self.asset(id).await?;
                        let bytes = self.store.read(&asset.model_storage_key).await?;
                        Ok::<_, ServiceError>(format!(
                            "data:{};base64,{}",
                            asset.model_mime_type,
                            base64::engine::general_purpose::STANDARD.encode(bytes)
                        ))
                    }
                    .await;
                    match result {
                        Ok(data) => *url = data,
                        Err(_) => {
                            missing_ids.insert(id.to_owned());
                            *part = ContentPart::Text(format!(
                                "[image attachment omitted: file for {id} could not be read]"
                            ));
                        }
                    }
                }
                if let ContentPart::ToolResult(result) = part {
                    let mut kept = Vec::new();
                    let mut missing = 0;
                    for mut image in std::mem::take(&mut result.images) {
                        if let Some(id) = parse_attachment_id(&image.data) {
                            let bytes = async {
                                let asset = self.asset(id).await?;
                                let bytes = self.store.read(&asset.model_storage_key).await?;
                                Ok::<_, ServiceError>((asset.model_mime_type, bytes))
                            }
                            .await;
                            match bytes {
                                Ok((mime, bytes)) => {
                                    image.mime_type = mime;
                                    image.data =
                                        base64::engine::general_purpose::STANDARD.encode(bytes);
                                }
                                Err(_) => {
                                    missing_ids.insert(id.to_owned());
                                    missing += 1;
                                    continue;
                                }
                            }
                        }
                        kept.push(image);
                    }
                    result.images = kept;
                    if missing > 0 {
                        let text = crate::context::tool_result_text(&result.content);
                        result.content = json!(format!(
                            "{text}\n[{missing} image attachments omitted: files could not be read]"
                        ));
                    }
                }
            }
        }
        (messages, missing_ids)
    }
    /// Reference-driven GLOBAL reclamation: shared content rows, including
    /// COW originals and revert backups, retain their assets across forks.
    pub async fn sweep(&self) {
        let _exclusive = self.publication.write().await;
        let cutoff = chrono::Utc::now().naive_utc() - chrono::TimeDelta::hours(24);
        if let Err(error) = db::expire_tickets(&self.pool, cutoff).await {
            tracing::error!(%error, "cannot expire attachment tickets; skipping sweep");
            return;
        }
        let (live, rows) = match (
            db::live_asset_ids(&self.pool, cutoff).await,
            db::list_assets(&self.pool).await,
        ) {
            (Ok(live), Ok(rows)) => (live, rows),
            (Err(error), _) | (_, Err(error)) => {
                tracing::error!(%error, "cannot inspect attachment ownership; skipping sweep");
                return;
            }
        };
        let mut paths = HashSet::new();
        for row in rows {
            let pinned = self
                .store
                .path(&row.storage_key)
                .is_ok_and(|path| self.uploads.contains(&path));
            if live.contains(&row.id) || pinned {
                paths.insert(row.storage_key);
                paths.insert(row.model_storage_key);
                continue;
            }
            if let Err(error) = db::delete_asset(&self.pool, &row.id).await {
                tracing::warn!(%error, "cannot reclaim attachment row");
                paths.insert(row.storage_key);
                paths.insert(row.model_storage_key);
                continue;
            }
            for entry in self.resources.iter() {
                if let Some(resources) = entry.value().upgrade() {
                    resources.remove(&attachment_url(&row.id));
                }
            }
            for key in [row.storage_key, row.model_storage_key] {
                if let Err(error) = self.store.remove(&key).await {
                    tracing::warn!(%error, "cannot reclaim attachment file");
                }
            }
        }
        // The fixed two-level layout also contains failed-publication orphans.
        // Active temp/final paths are pinned; slow network reads hold no lock.
        let Ok(mut dirs) = tokio::fs::read_dir(self.root()).await else {
            return;
        };
        while let Ok(Some(dir)) = dirs.next_entry().await {
            // A stray symlink must never turn orphan collection into a walk
            // outside the asset root.
            if !dir.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(mut files) = tokio::fs::read_dir(dir.path()).await else {
                continue;
            };
            while let Ok(Some(file)) = files.next_entry().await {
                let path = file.path();
                let Ok(key) = path.strip_prefix(self.root()) else {
                    continue;
                };
                if paths.contains(&key.to_string_lossy().to_string())
                    || self.uploads.contains(&path)
                {
                    continue;
                }
                if let Err(error) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(%error, "cannot reclaim orphan attachment");
                }
            }
            let _ = tokio::fs::remove_dir(dir.path()).await;
        }
    }
}
