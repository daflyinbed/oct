use anyhow::Result;
use chrono::NaiveDateTime;
use sqlx::SqlitePool;

/// One persisted background bash task (ADR 0003): the lifecycle row created
/// when `execute_command` starts a command with `run_in_background = true`
/// and settled exactly once by [`finish_task`] (or by
/// [`mark_stale_tasks_lost`] after a crash). The in-memory registry holds
/// only ACTIVE tasks; this row is the durable truth for queries, the
/// notification catch-up and the restart sweep.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BackgroundTask {
    pub id: String,
    pub conversation_id: String,
    pub parent_run_id: Option<String>,
    pub parent_call_id: String,
    pub command: String,
    pub working_dir: Option<String>,
    /// `running` while live; terminal: `completed` | `failed` | `cancelled`;
    /// `lost` = the backend process died before the supervisor settled.
    pub status: String,
    /// Raw exit code if the command exited normally; NULL when it was
    /// terminated by a signal (watchdog kills) or unknown.
    pub exit_code: Option<i64>,
    pub output_path: String,
    pub output_bytes: Option<i64>,
    pub error: Option<String>,
    /// 1 once a `<task-notification>` message for this task has been
    /// inserted (set in the SAME transaction as the message insert, so
    /// wake runs and catch-up flushes never double-deliver).
    pub notified: i64,
    pub created_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
}

impl BackgroundTask {
    pub fn is_terminal(&self) -> bool {
        self.status != "running"
    }
}

/// Insert payload for a new background task row. The id is CALLER-owned
/// (generated where the output file is named, so the registry key, the
/// file name and the row id are one identity).
pub struct NewBackgroundTask {
    pub id: String,
    pub conversation_id: String,
    pub parent_run_id: Option<String>,
    pub parent_call_id: String,
    pub command: String,
    pub working_dir: Option<String>,
    pub output_path: String,
}

pub async fn create_task(pool: &SqlitePool, new: NewBackgroundTask) -> Result<BackgroundTask> {
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO background_tasks (id, conversation_id, parent_run_id, parent_call_id, command, working_dir, status, output_path, created_at) VALUES ($1, $2, $3, $4, $5, $6, 'running', $7, $8)",
        new.id,
        new.conversation_id,
        new.parent_run_id,
        new.parent_call_id,
        new.command,
        new.working_dir,
        new.output_path,
        now,
    )
    .execute(pool)
    .await?;

    Ok(BackgroundTask {
        id: new.id,
        conversation_id: new.conversation_id,
        parent_run_id: new.parent_run_id,
        parent_call_id: new.parent_call_id,
        command: new.command,
        working_dir: new.working_dir,
        status: "running".to_string(),
        exit_code: None,
        output_path: new.output_path,
        output_bytes: None,
        error: None,
        notified: 0,
        created_at: now,
        finished_at: None,
    })
}

/// Terminal status of a background task, as persisted to `status`.
pub enum TaskFinalStatus {
    Completed,
    Failed,
    Cancelled,
}

impl TaskFinalStatus {
    fn as_str(&self) -> &'static str {
        match self {
            TaskFinalStatus::Completed => "completed",
            TaskFinalStatus::Failed => "failed",
            TaskFinalStatus::Cancelled => "cancelled",
        }
    }
}

/// Settle a task: flip a live row to the terminal status, stamp
/// `finished_at` and record the exit code / error / final output size. The
/// `status = 'running'` guard makes the write one-shot — a `lost` row
/// (post-crash) belongs to the restart sweep, never to a late supervisor
/// (which cannot exist: the supervisor dies with the process).
pub async fn finish_task(
    pool: &SqlitePool,
    task_id: &str,
    status: TaskFinalStatus,
    exit_code: Option<i64>,
    error: Option<&str>,
    output_bytes: i64,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let status = status.as_str();
    let result = sqlx::query!(
        "UPDATE background_tasks SET status = $1, finished_at = $2, exit_code = $3, error = $4, output_bytes = $5 WHERE id = $6 AND status = 'running'",
        status,
        now,
        exit_code,
        error,
        output_bytes,
        task_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Crash recovery (ADR 0003, mirrors `runs::mark_stale_runs_lost`): task
/// rows a previous process left `running` become `lost`. PDEATHSIG already
/// terminated the commands themselves when that process died; the row just
/// records the fact so the catch-up can tell the model what happened.
pub async fn mark_stale_tasks_lost(pool: &SqlitePool) -> Result<Vec<BackgroundTask>> {
    let now = chrono::Utc::now().naive_utc();
    let rows = sqlx::query_as!(
        BackgroundTask,
        "UPDATE background_tasks SET status = 'lost', finished_at = $1 WHERE status = 'running' RETURNING id, conversation_id, parent_run_id, parent_call_id, command, working_dir, status, exit_code, output_path, output_bytes, error, notified, created_at, finished_at",
        now,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Persist a `lost` task as cancelled without touching live rows (the HTTP
/// cancel endpoint's lost branch): a lost task's process is already gone.
pub async fn cancel_lost_task(pool: &SqlitePool, task_id: &str) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result = sqlx::query!(
        "UPDATE background_tasks SET status = 'cancelled', finished_at = $1 WHERE id = $2 AND status = 'lost'",
        now,
        task_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Every task of a conversation, running first then oldest first — the
/// ordering the bottom-bar list and the API share (mirrors
/// `runs::list_subagent_runs`).
pub async fn list_tasks_for_conversation(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<BackgroundTask>> {
    let rows = sqlx::query_as!(
        BackgroundTask,
        "SELECT id, conversation_id, parent_run_id, parent_call_id, command, working_dir, status, exit_code, output_path, output_bytes, error, notified, created_at, finished_at FROM background_tasks WHERE conversation_id = $1 ORDER BY CASE WHEN status = 'running' THEN 0 ELSE 1 END, created_at ASC",
        conversation_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_task(pool: &SqlitePool, task_id: &str) -> Result<Option<BackgroundTask>> {
    let row = sqlx::query_as!(
        BackgroundTask,
        "SELECT id, conversation_id, parent_run_id, parent_call_id, command, working_dir, status, exit_code, output_path, output_bytes, error, notified, created_at, finished_at FROM background_tasks WHERE id = $1",
        task_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The settled tasks of a conversation whose notification has not been
/// delivered yet (`notified = 0`), oldest first — the wake drainer's and the
/// send/resume catch-up's work list. Includes `lost` rows (crash leftovers
/// that owe the model an explanation).
pub async fn list_pending_notifications(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<BackgroundTask>> {
    let rows = sqlx::query_as!(
        BackgroundTask,
        "SELECT id, conversation_id, parent_run_id, parent_call_id, command, working_dir, status, exit_code, output_path, output_bytes, error, notified, created_at, finished_at FROM background_tasks WHERE conversation_id = $1 AND notified = 0 AND status != 'running' ORDER BY created_at ASC",
        conversation_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Which conversations have unnotified settled tasks — the restart sweep
/// surfaces them so logs can name the affected conversations.
pub async fn conversations_with_pending_notifications(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar!(
        "SELECT DISTINCT conversation_id FROM background_tasks WHERE notified = 0 AND status != 'running'"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Insert the notification message and mark every task it covers as
/// notified, in ONE transaction: a crash between the two writes is
/// impossible, so a delivered notification can never be re-delivered and an
/// undelivered one is never silently swallowed. Returns the message row id
/// (the wake run's start boundary).
pub async fn insert_notification_marking_tasks(
    pool: &SqlitePool,
    conversation_id: &str,
    message: &oct_llm_provider::core::Message,
    details: Option<&serde_json::Value>,
    task_ids: &[String],
) -> Result<String> {
    let ids_json = serde_json::to_string(task_ids)?;
    let mut tx = pool.begin().await?;

    let stored = super::messages::insert_message_in_tx(
        &mut tx,
        conversation_id,
        message,
        None,
        None,
        details,
    )
    .await?;

    sqlx::query!(
        "UPDATE background_tasks SET notified = 1 WHERE id IN (SELECT value FROM json_each($1))",
        ids_json,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(stored.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::{Message, Role};

    /// End-to-end row lifecycle over a migrated temp-file database: create →
    /// pending query sees nothing while running → finish → pending sees it →
    /// deliver marks notified atomically → list ordering puts running first.
    #[tokio::test]
    async fn task_row_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("t.db").display());
        let pool = crate::db::init_pool(&url).await.unwrap();

        let project = crate::db::projects::create_project(
            &pool,
            &crate::db::projects::CreateProjectRequest {
                name: "p".into(),
                working_dir: dir.path().display().to_string(),
                additional_dirs: None,
            },
        )
        .await
        .unwrap();
        let conv = crate::db::conversations::create_conversation(&pool, &project.id, Some("c1"))
            .await
            .unwrap();

        let running = create_task(
            &pool,
            NewBackgroundTask {
                id: format!("bg-test-2"),
                conversation_id: conv.id.clone(),
                parent_run_id: None,
                parent_call_id: "call-1".into(),
                command: "sleep 5".into(),
                working_dir: None,
                output_path: "/tmp/x.log".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(running.status, "running");
        assert!(!running.is_terminal());

        // A running task owes no notification and blocks nothing.
        assert!(
            list_pending_notifications(&pool, &conv.id)
                .await
                .unwrap()
                .is_empty()
        );

        // One-shot settlement: a second finish is a no-op.
        assert!(
            finish_task(
                &pool,
                &running.id,
                TaskFinalStatus::Completed,
                Some(0),
                None,
                42,
            )
            .await
            .unwrap()
        );
        assert!(
            !finish_task(
                &pool,
                &running.id,
                TaskFinalStatus::Cancelled,
                None,
                Some("second"),
                1,
            )
            .await
            .unwrap()
        );

        let pending = list_pending_notifications(&pool, &conv.id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].exit_code, Some(0));
        assert_eq!(pending[0].output_bytes, Some(42));

        // Deliver: message insert + notified flag in one transaction.
        let msg = Message::text(Role::User, "<task-notification>ok</task-notification>");
        let msg_id =
            insert_notification_marking_tasks(&pool, &conv.id, &msg, None, &[running.id.clone()])
                .await
                .unwrap();
        assert!(!msg_id.is_empty());
        assert!(
            list_pending_notifications(&pool, &conv.id)
                .await
                .unwrap()
                .is_empty()
        );
        let row = get_task(&pool, &running.id).await.unwrap().unwrap();
        assert_eq!(row.notified, 1);

        // Restart sweep: a fresh running row flips to lost, then a lost-only
        // cancel settles it; the lost row still owes its notification until
        // delivered.
        let stuck = create_task(
            &pool,
            NewBackgroundTask {
                id: format!("bg-test-3"),
                conversation_id: conv.id.clone(),
                parent_run_id: None,
                parent_call_id: "call-2".into(),
                command: "sleep 999".into(),
                working_dir: None,
                output_path: "/tmp/y.log".into(),
            },
        )
        .await
        .unwrap();
        let swept = mark_stale_tasks_lost(&pool).await.unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].id, stuck.id);
        assert!(cancel_lost_task(&pool, &stuck.id).await.unwrap());
        let pending = list_pending_notifications(&pool, &conv.id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "cancelled");

        // Ordering: running rows come before settled ones.
        let live = create_task(
            &pool,
            NewBackgroundTask {
                id: format!("bg-test-4"),
                conversation_id: conv.id.clone(),
                parent_run_id: None,
                parent_call_id: "call-3".into(),
                command: "yes".into(),
                working_dir: None,
                output_path: "/tmp/z.log".into(),
            },
        )
        .await
        .unwrap();
        let listed = list_tasks_for_conversation(&pool, &conv.id).await.unwrap();
        assert_eq!(listed[0].id, live.id);
        assert_eq!(listed.len(), 3);
    }
}
