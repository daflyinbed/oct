use anyhow::Result;
use chrono::NaiveDateTime;
use sqlx::SqlitePool;
use uuid::Uuid;

/// One persisted agent run (ADR 0002 §6.2): the lifecycle row created when a
/// run starts and settled exactly once by [`finish_run`] (or by
/// [`mark_stale_runs_lost`] after a crash). Root runs reference the
/// conversation directly; subagent runs (later PRs) additionally carry the
/// parent/root run linkage columns.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentRun {
    pub id: String,
    pub conversation_id: String,
    /// `root` today; `subagent` rows arrive with the task tool (PR2).
    pub kind: String,
    pub parent_run_id: Option<String>,
    pub root_run_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
    /// `running` while live; terminal: `completed` | `failed` | `cancelled`;
    /// `lost` = the executing process died before settling the run.
    pub status: String,
    /// Bumped when a `lost` run is picked up by a new execution instance;
    /// PR1 always creates runs at epoch 1.
    pub execution_epoch: i64,
    /// Effective-config snapshot (model identity + window only — never API
    /// keys or OAuth material).
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_window: Option<i64>,
    pub error: Option<String>,
    /// The run's final persisted assistant message row.
    pub final_message_id: Option<String>,
    /// Aggregated usage over the run's requests; NULL = unknown (no Usage
    /// event ever arrived — not the same as zero).
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub started_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
}

/// Insert payload for a new run row.
pub struct NewRun {
    pub conversation_id: String,
    /// `root` or `subagent`.
    pub kind: &'static str,
    pub parent_run_id: Option<String>,
    pub root_run_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_window: Option<i64>,
}

pub async fn create_run(pool: &SqlitePool, new: NewRun) -> Result<AgentRun> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO agent_runs (id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, provider_id, model_id, context_window, started_at) VALUES ($1, $2, $3, $4, $5, $6, 'running', 1, $7, $8, $9, $10)",
        id,
        new.conversation_id,
        new.kind,
        new.parent_run_id,
        new.root_run_id,
        new.parent_tool_call_id,
        new.provider_id,
        new.model_id,
        new.context_window,
        now,
    )
    .execute(pool)
    .await?;

    Ok(AgentRun {
        id,
        conversation_id: new.conversation_id,
        kind: new.kind.to_string(),
        parent_run_id: new.parent_run_id,
        root_run_id: new.root_run_id,
        parent_tool_call_id: new.parent_tool_call_id,
        status: "running".to_string(),
        execution_epoch: 1,
        provider_id: new.provider_id,
        model_id: new.model_id,
        context_window: new.context_window,
        error: None,
        final_message_id: None,
        input_tokens: None,
        output_tokens: None,
        started_at: now,
        finished_at: None,
    })
}

/// Terminal status of a run, as persisted to `agent_runs.status`.
pub enum RunFinalStatus {
    Completed,
    Failed,
    Cancelled,
}

impl RunFinalStatus {
    fn as_str(&self) -> &'static str {
        match self {
            RunFinalStatus::Completed => "completed",
            RunFinalStatus::Failed => "failed",
            RunFinalStatus::Cancelled => "cancelled",
        }
    }
}

/// Settle a run: flip `running` to the terminal status, stamp `finished_at`
/// and record the final message / error / aggregated usage. The
/// `AND status = 'running'` guard makes the write one-shot: `false` means the
/// row was already terminal, so a late write from a stale execution instance
/// must not overwrite the fresh one (PR2 adds execution-epoch checks on top;
/// the guard alone is enough for PR1 where only one instance runs at a time).
pub async fn finish_run(
    pool: &SqlitePool,
    run_id: &str,
    status: RunFinalStatus,
    final_message_id: Option<&str>,
    error: Option<&str>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    // Bound first: the query! macro's generated argument borrows outlive the
    // statement, so the &str from as_str() must not be a temporary here.
    let status = status.as_str();
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE agent_runs SET status = $1, finished_at = $2, final_message_id = $3, error = $4, input_tokens = $5, output_tokens = $6 WHERE id = $7 AND status = 'running'",
        status,
        now,
        final_message_id,
        error,
        input_tokens,
        output_tokens,
        run_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Crash recovery (ADR 0002 §6.2): runs a previous process left `running`
/// become `lost` — the executing instance is gone, but the persisted history
/// and the conversation's resume eligibility are untouched. Called once at
/// server startup, before any new run can be created.
pub async fn mark_stale_runs_lost(pool: &SqlitePool) -> Result<Vec<AgentRun>> {
    let now = chrono::Utc::now().naive_utc();
    let rows = sqlx::query_as!(
        AgentRun,
        "UPDATE agent_runs SET status = 'lost', finished_at = $1 WHERE status = 'running' RETURNING id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, provider_id, model_id, context_window, error, final_message_id, input_tokens, output_tokens, started_at, finished_at",
        now,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<AgentRun>> {
    let row = sqlx::query_as!(
        AgentRun,
        "SELECT id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, provider_id, model_id, context_window, error, final_message_id, input_tokens, output_tokens, started_at, finished_at FROM agent_runs WHERE id = $1",
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// The most recent run started for a conversation (by `started_at`, with
/// rowid as the deterministic tiebreak).
pub async fn latest_run_for_conversation(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<AgentRun>> {
    let row = sqlx::query_as!(
        AgentRun,
        "SELECT id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, provider_id, model_id, context_window, error, final_message_id, input_tokens, output_tokens, started_at, finished_at FROM agent_runs WHERE conversation_id = $1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
        conversation_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}
