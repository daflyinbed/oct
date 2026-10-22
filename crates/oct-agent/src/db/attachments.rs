//! Immutable assets, one-send upload tickets and message ownership queries.
//! Filesystem publication is coordinated by AttachmentService. Every message
//! insert indexes its references in the caller's transaction, including tools
//! and COW copies; conversation/fork membership never owns bytes directly.
use super::messages::{StoredMessage, insert_message_in_tx};
use anyhow::Result;
use chrono::NaiveDateTime;
use oct_llm_provider::core::Message;
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::HashSet;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AttachmentRow {
    pub id: String,
    pub conversation_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub state: String,
    pub created_at: NaiveDateTime,
    pub asset_id: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AssetRow {
    pub id: String,
    pub storage_key: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub model_storage_key: String,
    pub model_mime_type: String,
    pub model_size_bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub normalizer_version: i64,
    pub created_at: NaiveDateTime,
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<AttachmentRow>> {
    Ok(sqlx::query_as!(AttachmentRow,
        "SELECT id, conversation_id, filename, mime_type, size_bytes, state, created_at, asset_id AS \"asset_id!\" FROM staged_uploads WHERE id = $1", id
    ).fetch_optional(pool).await?)
}

/// Accept old ticket references and new immutable asset references alike.
pub async fn asset_for_reference(pool: &SqlitePool, id: &str) -> Result<Option<AssetRow>> {
    Ok(sqlx::query_as!(AssetRow,
        "SELECT id, storage_key, mime_type, size_bytes, model_storage_key, model_mime_type, model_size_bytes, width, height, normalizer_version, created_at FROM attachment_assets WHERE id = $1 OR id = (SELECT asset_id FROM staged_uploads WHERE id = $1) LIMIT 1", id
    ).fetch_optional(pool).await?)
}

pub async fn assets_for_references(pool: &SqlitePool, ids: &[String]) -> Result<Vec<AssetRow>> {
    let ids = serde_json::to_string(ids)?;
    Ok(sqlx::query_as!(AssetRow,
        "SELECT id, storage_key, mime_type, size_bytes, model_storage_key, model_mime_type, model_size_bytes, width, height, normalizer_version, created_at FROM attachment_assets WHERE id IN (SELECT value FROM json_each($1)) OR id IN (SELECT asset_id FROM staged_uploads WHERE id IN (SELECT value FROM json_each($1)))", ids
    ).fetch_all(pool).await?)
}

pub async fn publish_upload(
    pool: &SqlitePool,
    asset: &AssetRow,
    ticket: &AttachmentRow,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!("INSERT INTO attachment_assets (id, storage_key, mime_type, size_bytes, model_storage_key, model_mime_type, model_size_bytes, width, height, normalizer_version) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        asset.id, asset.storage_key, asset.mime_type, asset.size_bytes, asset.model_storage_key,
        asset.model_mime_type, asset.model_size_bytes, asset.width, asset.height, asset.normalizer_version
    ).execute(&mut *tx).await?;
    insert_ticket(&mut tx, ticket).await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_ticket(tx: &mut SqliteConnection, ticket: &AttachmentRow) -> Result<()> {
    let result = sqlx::query!(
        "INSERT INTO staged_uploads (id, conversation_id, filename, mime_type, size_bytes, state, asset_id) SELECT $1,$2,$3,$4,$5,'staged',$6 WHERE EXISTS (SELECT 1 FROM conversations WHERE id = $2 AND origin_kind != 'revert_backup')",
        ticket.id, ticket.conversation_id, ticket.filename, ticket.mime_type, ticket.size_bytes, ticket.asset_id
    ).execute(tx).await?;
    anyhow::ensure!(
        result.rows_affected() == 1,
        "Conversation is missing or no longer writable"
    );
    Ok(())
}

/// Creating a ticket pins an existing immutable asset without copying bytes.
pub async fn copy_ticket(pool: &SqlitePool, ticket: &AttachmentRow) -> Result<()> {
    let mut tx = pool.begin().await?;
    insert_ticket(&mut tx, ticket).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn is_referenced(pool: &SqlitePool, asset_id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM message_assets WHERE asset_id = $1) AS \"present!: bool\"",
        asset_id
    )
    .fetch_one(pool)
    .await?)
}

pub(crate) async fn filename_for_asset(
    pool: &SqlitePool,
    asset_id: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar!(
        "SELECT filename FROM staged_uploads WHERE asset_id = $1 ORDER BY created_at LIMIT 1",
        asset_id
    )
    .fetch_optional(pool)
    .await?)
}

/// File reads precede the write transaction. The service's publication lease
/// covers this validation through the final atomic claim/message commit.
pub(crate) async fn load_staged(
    pool: &SqlitePool,
    conversation_id: &str,
    ids: &[String],
) -> Result<Vec<AttachmentRow>, ClaimFailure> {
    let mut seen = HashSet::new();
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.insert(id) {
            return Err(ClaimFailure::NotStaged);
        }
        let row = get(pool, id)
            .await?
            .filter(|row| {
                row.conversation_id == conversation_id
                    && row.state == "staged"
                    && row.created_at
                        >= chrono::Utc::now().naive_utc() - chrono::TimeDelta::hours(24)
            })
            .ok_or(ClaimFailure::NotStaged)?;
        rows.push(row);
    }
    Ok(rows)
}

/// Write first so SQLite does not need to upgrade a stale SELECT snapshot.
async fn claim_staged(
    tx: &mut SqliteConnection,
    conversation_id: &str,
    ids: &[String],
) -> Result<(), ClaimFailure> {
    for id in ids {
        let result = sqlx::query!("UPDATE staged_uploads SET state = 'sent' WHERE id = $1 AND conversation_id = $2 AND state = 'staged' AND created_at >= datetime('now', '-24 hours')", id, conversation_id)
            .execute(&mut *tx).await.map_err(|e| ClaimFailure::Db(e.into()))?;
        if result.rows_affected() != 1 {
            return Err(ClaimFailure::NotStaged);
        }
    }
    Ok(())
}

/// Claims, immutable message content, membership and ownership index commit
/// together. Failure or future cancellation leaves every ticket retryable.
pub(crate) async fn claim_and_insert_message(
    pool: &SqlitePool,
    conversation_id: &str,
    ids: &[String],
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
) -> Result<StoredMessage, ClaimFailure> {
    let mut tx = pool.begin().await.map_err(|e| ClaimFailure::Db(e.into()))?;
    claim_staged(&mut tx, conversation_id, ids).await?;
    let stored = insert_message_in_tx(
        &mut tx,
        conversation_id,
        message,
        provider_id,
        model_id,
        details,
    )
    .await?;
    tx.commit().await.map_err(|e| ClaimFailure::Db(e.into()))?;
    Ok(stored)
}

pub(crate) async fn index_message_assets(
    tx: &mut SqliteConnection,
    message_id: &str,
    message: &Message,
    details: Option<&Value>,
) -> Result<()> {
    for id in crate::attachment_types::referenced_assets(message, details) {
        sqlx::query!("INSERT OR IGNORE INTO message_assets (message_id, asset_id) SELECT $1,id FROM attachment_assets WHERE id = $2 OR id = (SELECT asset_id FROM staged_uploads WHERE id = $2)", message_id, id)
            .execute(&mut *tx).await?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimFailure {
    #[error("附件不存在、不属于此会话或已被发送")]
    NotStaged,
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

pub(crate) async fn list_assets(pool: &SqlitePool) -> Result<Vec<AssetRow>> {
    Ok(sqlx::query_as!(AssetRow, "SELECT id, storage_key, mime_type, size_bytes, model_storage_key, model_mime_type, model_size_bytes, width, height, normalizer_version, created_at FROM attachment_assets").fetch_all(pool).await?)
}

pub(crate) async fn expire_tickets(pool: &SqlitePool, cutoff: NaiveDateTime) -> Result<()> {
    sqlx::query!(
        "DELETE FROM staged_uploads WHERE state = 'staged' AND created_at < $1",
        cutoff
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Staged tickets are roots only until their TTL. Claimed tickets are receipts;
/// live messages and COW originals, not those receipts, retain sent bytes.
pub(crate) async fn live_asset_ids(
    pool: &SqlitePool,
    cutoff: NaiveDateTime,
) -> Result<HashSet<String>> {
    let ids = sqlx::query_scalar!("SELECT asset_id AS \"asset_id!\" FROM message_assets UNION SELECT asset_id AS \"asset_id!\" FROM staged_uploads WHERE state = 'staged' AND created_at >= $1", cutoff).fetch_all(pool).await?;
    Ok(ids.into_iter().collect())
}

pub(crate) async fn delete_asset(pool: &SqlitePool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM staged_uploads WHERE asset_id = $1", id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM attachment_assets WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Resolve only the indexed asset's original bytes; paths never come from a
/// model-authored URI. Membership includes fork-shared messages and COW
/// originals. The GC ownership index alone is not authorization: arbitrary
/// text can pin a foreign URI, but only sent attachments and tool images grant
/// file access. Workspace resolution and the permission gate still run.
pub async fn resource_paths(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query!(
        "SELECT m.role, m.parts_json, m.details_json FROM conversation_messages cm JOIN messages current ON current.id = cm.message_id JOIN messages m ON m.id = current.id OR m.id = current.supersedes_message_id WHERE cm.conversation_id = $1 AND m.role IN ('user', 'tool') AND EXISTS (SELECT 1 FROM message_assets WHERE message_id = m.id)",
        conversation_id
    )
        .fetch_all(pool)
        .await?;
    let mut references = HashSet::new();
    for row in rows {
        if row.role == "user" {
            if let Some(content) = crate::attachment_types::UserContent::from_stored(
                &row.parts_json,
                row.details_json.as_deref(),
            ) {
                references.extend(content.attachments.into_iter().map(|a| a.asset.id));
            }
        } else if row.role == "tool" {
            let parts: Vec<oct_llm_provider::core::ContentPart> =
                serde_json::from_str(&row.parts_json)?;
            for part in parts {
                if let oct_llm_provider::core::ContentPart::ToolResult(result) = part {
                    references.extend(result.images.into_iter().filter_map(|image| {
                        crate::attachment_types::parse_attachment_id(&image.data).map(str::to_owned)
                    }));
                }
            }
        }
    }
    Ok(
        assets_for_references(pool, &references.into_iter().collect::<Vec<_>>())
            .await?
            .into_iter()
            .map(|row| (row.id, row.storage_key))
            .collect(),
    )
}
