use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Conversation {
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// Where `title` came from: `default` (untitled placeholder), `ai`
    /// (auto-generated), `user` (explicitly set via API). Manual names must
    /// win over automatic ones, so the distinction is data, not string
    /// matching on the default title.
    pub title_source: String,
    /// `chat` (a normal sidebar conversation) or `subagent` (a managed
    /// worker session created by the `task` tool; never listed, never sent
    /// to directly).
    pub kind: String,
    /// For `subagent` rows: the conversation whose run delegated this task.
    pub parent_conversation_id: Option<String>,
    /// For `subagent` rows: the root chat conversation of the task tree.
    pub root_conversation_id: Option<String>,
    /// For `subagent` rows: the worker role (`worker` in v1).
    pub agent_type: Option<String>,
    /// Tool permission mode: `default` (write/execute/
    /// outside-read all ask), `accept_edits` (in-workspace writes auto-
    /// allowed), `bypass` (everything allowed — deny rules still win).
    pub permission_mode: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConversationRequest {
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConversationRequest {
    pub title: Option<String>,
    /// Tool permission mode: `default` | `accept_edits` |
    /// `bypass`. Takes effect on the active run's NEXT tool call (validated
    /// and applied by the API layer, which also flips the live gates).
    pub permission_mode: Option<String>,
}

pub async fn list_conversations_by_project(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<Conversation>> {
    let rows = sqlx::query_as!(
        Conversation,
        // Only kind='chat' rows: subagent sessions are managed children of a
        // chat conversation and must never appear in the sidebar list.
        "SELECT id, project_id, title, title_source, kind, parent_conversation_id, root_conversation_id, agent_type, permission_mode, created_at, updated_at FROM conversations WHERE project_id = $1 AND kind = 'chat' ORDER BY updated_at DESC",
        project_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn create_conversation(
    pool: &SqlitePool,
    project_id: &str,
    title: Option<&str>,
) -> Result<Conversation> {
    let mut tx = pool.begin().await?;
    let conversation = create_conversation_in_tx(&mut tx, project_id, title).await?;
    tx.commit().await?;
    Ok(conversation)
}

/// Create a conversation inside a caller-owned transaction. The subagent
/// registration uses this so the child conversation, its delegation message
/// and the agent_runs row land atomically.
pub async fn create_conversation_in_tx(
    tx: &mut SqliteConnection,
    project_id: &str,
    title: Option<&str>,
) -> Result<Conversation> {
    let id = Uuid::new_v4().to_string();
    // An explicit title at creation is a user-chosen name: automatic title
    // generation must not override it.
    let title_source = if title.is_some() { "user" } else { "default" };
    let title = title.unwrap_or("New conversation");
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO conversations (id, project_id, title, title_source, kind, parent_conversation_id, root_conversation_id, agent_type, created_at, updated_at) VALUES ($1, $2, $3, $4, 'chat', NULL, NULL, NULL, $5, $6)",
        id,
        project_id,
        title,
        title_source,
        now,
        now,
    )
    .execute(&mut *tx)
    .await?;

    Ok(Conversation {
        id,
        project_id: project_id.to_string(),
        title: title.to_string(),
        title_source: title_source.to_string(),
        kind: "chat".to_string(),
        parent_conversation_id: None,
        root_conversation_id: None,
        agent_type: None,
        // The INSERT above leaves the column at its DEFAULT.
        permission_mode: "default".to_string(),
        created_at: now,
        updated_at: now,
    })
}

/// Registration payload for a subagent's conversation (created together with
/// its delegation message and agent_runs row in ONE transaction — see the
/// `subagent` module).
pub struct NewSubagentConversation<'a> {
    pub project_id: &'a str,
    /// The conversation whose run delegated this task.
    pub parent_conversation_id: &'a str,
    /// The root chat conversation of the task tree (the parent for depth-1
    /// tasks, which is the only depth v1 allows).
    pub root_conversation_id: &'a str,
    /// Fixed worker role in v1.
    pub agent_type: &'a str,
    /// The `task` tool's `description` argument — the child's UI title. An
    /// explicit title at creation is user-chosen, so `title_source` is
    /// `'user'` and the AI titler never touches it.
    pub title: &'a str,
}

/// Create the subagent's conversation inside a caller-owned transaction.
pub async fn create_subagent_conversation_in_tx(
    tx: &mut SqliteConnection,
    new: NewSubagentConversation<'_>,
) -> Result<Conversation> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO conversations (id, project_id, title, title_source, kind, parent_conversation_id, root_conversation_id, agent_type, created_at, updated_at) VALUES ($1, $2, $3, 'user', 'subagent', $4, $5, $6, $7, $7)",
        id,
        new.project_id,
        new.title,
        new.parent_conversation_id,
        new.root_conversation_id,
        new.agent_type,
        now,
    )
    .execute(&mut *tx)
    .await?;

    Ok(Conversation {
        id,
        project_id: new.project_id.to_string(),
        title: new.title.to_string(),
        title_source: "user".to_string(),
        kind: "subagent".to_string(),
        parent_conversation_id: Some(new.parent_conversation_id.to_string()),
        root_conversation_id: Some(new.root_conversation_id.to_string()),
        agent_type: Some(new.agent_type.to_string()),
        // The INSERT above leaves the column at its DEFAULT; subagents
        // inherit their parent conversation's mode at evaluation time, not
        // here.
        permission_mode: "default".to_string(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn get_conversation(pool: &SqlitePool, id: &str) -> Result<Option<Conversation>> {
    let row = sqlx::query_as!(
        Conversation,
        "SELECT id, project_id, title, title_source, kind, parent_conversation_id, root_conversation_id, agent_type, permission_mode, created_at, updated_at FROM conversations WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Delete a conversation AND every subagent conversation hanging off its
/// task tree, in one transaction, then sweep the orphaned content rows.
/// Children reference the root through `root_conversation_id` (no FK), so
/// the cascade needs the explicit OR arm. `true` when anything was deleted.
pub async fn delete_conversation_tree(pool: &SqlitePool, id: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "DELETE FROM conversations WHERE id = $1 OR root_conversation_id = $1",
        id,
    )
    .execute(&mut *tx)
    .await?;
    let deleted = result.rows_affected() > 0;
    tx.commit().await?;

    if deleted {
        // The cascade stopped at the membership rows; sweep the content rows
        // this deletion orphaned (they are the big payloads — tool results,
        // COW originals). Rows still referenced as a live copy's original
        // survive; see `delete_orphan_content_rows`.
        super::messages::delete_orphan_content_rows(pool).await?;
    }
    Ok(deleted)
}

pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult =
        sqlx::query!("DELETE FROM conversations WHERE id = $1", id)
            .execute(pool)
            .await?;

    if result.rows_affected() > 0 {
        // The cascade stopped at the membership rows; sweep the content rows
        // this deletion orphaned (they are the big payloads — tool results,
        // COW originals). Rows still referenced as a live copy's original
        // survive; see `delete_orphan_content_rows`.
        super::messages::delete_orphan_content_rows(pool).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn update_conversation_title(pool: &SqlitePool, id: &str, title: &str) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE conversations SET title = $1, title_source = 'user', updated_at = $2 WHERE id = $3",
        title,
        now,
        id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Switch a conversation's tool permission mode — takes
/// effect on the next tool call of the active run. `mode` is one of
/// `default`/`accept_edits`/`bypass`; the column's CHECK constraint rejects
/// anything else (same storage-layer-guard policy as the rest of this file).
pub async fn update_conversation_permission_mode(
    pool: &SqlitePool,
    id: &str,
    mode: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE conversations SET permission_mode = $1, updated_at = $2 WHERE id = $3",
        mode,
        now,
        id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Write an automatically generated title, but only while the conversation
/// still carries its default one. The conditional makes the write both
/// one-shot (a second generation attempt no-ops) and lose-able: a manual
/// rename flips `title_source` to `user` and this UPDATE matches nothing, so
/// the user's name always wins without any locking. `updated_at` is
/// deliberately untouched — a background title landing must not reorder the
/// conversation list mid-session.
pub async fn set_auto_title(pool: &SqlitePool, id: &str, title: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE conversations SET title = $1, title_source = 'ai' WHERE id = $2 AND title_source = 'default'",
        title,
        id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
