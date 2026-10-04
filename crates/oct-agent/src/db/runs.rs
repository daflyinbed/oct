use anyhow::Result;
use chrono::NaiveDateTime;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;
use uuid::Uuid;

/// One persisted agent run: the lifecycle row created when a
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
    pub history_epoch: i64,
    /// Effective-config snapshot (model identity + window only — never API
    /// keys or OAuth material).
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_window: Option<i64>,
    pub agent_config_json: Option<String>,
    pub error: Option<String>,
    /// The run's final persisted assistant message row.
    pub final_message_id: Option<String>,
    /// Aggregated usage over the run's requests; NULL = unknown (no Usage
    /// event ever arrived — not the same as zero).
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// For subagent runs: the task's UI title (the `task` tool's
    /// `description` argument).
    pub description: Option<String>,
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
    /// Task UI title for subagent runs.
    pub description: Option<String>,
}

pub async fn create_run(pool: &SqlitePool, new: NewRun) -> Result<AgentRun> {
    // Epoch capture reads before the INSERT. Acquire the write reservation
    // first so concurrent automatic title updates cannot poison a deferred
    // SQLite snapshot upgrade while registering the root run.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let run = create_run_in_tx(&mut tx, new).await?;
    if run.kind == "root" {
        sqlx::query!(
            "UPDATE wake_intents SET run_id = $1 WHERE conversation_id = $2",
            run.id,
            run.conversation_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(run)
}

/// Create a run row inside a caller-owned transaction (the subagent
/// registration creates the child conversation, delegation message and run
/// row atomically). The `(parent_run_id,
/// parent_tool_call_id)` UNIQUE index makes double registration under
/// concurrent re-entry fail here; callers catch the constraint error and
/// re-read the existing row.
pub async fn create_run_in_tx(tx: &mut SqliteConnection, new: NewRun) -> Result<AgentRun> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    // Root runs capture the current timeline; children inherit their parent's.
    let history_epoch = sqlx::query_scalar!(
        "SELECT COALESCE((SELECT history_epoch FROM agent_runs WHERE id = $1), (SELECT history_epoch FROM conversations WHERE id = $2)) AS \"epoch!: i64\"",
        new.parent_run_id, new.conversation_id
    ).fetch_one(&mut *tx).await?;

    sqlx::query!(
        "INSERT INTO agent_runs (id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, history_epoch, provider_id, model_id, context_window, description, started_at) VALUES ($1, $2, $3, $4, $5, $6, 'running', 1, $12, $7, $8, $9, $10, $11)",
        id,
        new.conversation_id,
        new.kind,
        new.parent_run_id,
        new.root_run_id,
        new.parent_tool_call_id,
        new.provider_id,
        new.model_id,
        new.context_window,
        new.description,
        now,
        history_epoch,
    )
    .execute(&mut *tx)
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
        history_epoch,
        provider_id: new.provider_id,
        model_id: new.model_id,
        context_window: new.context_window,
        agent_config_json: None,
        error: None,
        final_message_id: None,
        input_tokens: None,
        output_tokens: None,
        description: new.description,
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

/// Settle a run: flip a live (`running`, or `lost` — the cancel-lost path)
/// row to the terminal status, stamp `finished_at` and record the final
/// message / error / aggregated usage. The status guard makes the write
/// one-shot: `false` means the row was already terminal, so a late write
/// from a stale execution instance must not overwrite the fresh one. The
/// epoch guard tightens that for resumed runs: a `lost` run picked up by a
/// new execution instance has its `execution_epoch` bumped first, and an
/// in-flight stale instance settling with its old epoch matches neither the
/// epoch check nor the fresh instance's write.
#[allow(clippy::too_many_arguments)] // flat terminal-state signature; a struct would not clarify it
pub async fn finish_run(
    pool: &SqlitePool,
    run_id: &str,
    status: RunFinalStatus,
    final_message_id: Option<&str>,
    error: Option<&str>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    expected_epoch: Option<i64>,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    // Bound first: the query! macro's generated argument borrows outlive the
    // statement, so the &str from as_str() must not be a temporary here.
    let status = status.as_str();
    super::persist_critical(pool, || async {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE agent_runs SET status = $1, finished_at = $2, final_message_id = $3, error = $4, input_tokens = $5, output_tokens = $6 WHERE id = $7 AND status IN ('running', 'lost') AND ($8 IS NULL OR execution_epoch = $8)",
        status,
        now,
        final_message_id,
        error,
        input_tokens,
        output_tokens,
        run_id,
        expected_epoch,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
    }).await
}

/// Persist a `lost` run as cancelled WITHOUT taking any epoch — used by the
/// subagent cancel endpoint. Deliberately guarded on `status = 'lost'` only:
/// a concurrent recovery may have just bumped the row back to `running` with
/// a fresh execution instance, and this cancel must NOT settle that live
/// instance's row (its own epoch-guarded settlement owns it from here).
pub async fn cancel_lost_run(
    pool: &SqlitePool,
    run_id: &str,
    final_message_id: Option<&str>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result = sqlx::query!(
        "UPDATE agent_runs SET status = 'cancelled', finished_at = $1, final_message_id = $2, input_tokens = $3, output_tokens = $4 WHERE id = $5 AND status = 'lost'",
        now,
        final_message_id,
        input_tokens,
        output_tokens,
        run_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Pick a not-yet-settled run (`running` or `lost`) up by a NEW execution
/// instance: bump `execution_epoch`, flip the row back to `running` and
/// clear `finished_at`. Returns the new epoch, or `None` when the row is
/// already terminal (a terminal run must never restart).
pub async fn bump_execution_epoch(pool: &SqlitePool, run_id: &str) -> Result<Option<i64>> {
    let epoch = sqlx::query_scalar!(
        "UPDATE agent_runs SET execution_epoch = execution_epoch + 1, status = 'running', finished_at = NULL WHERE id = $1 AND status IN ('running', 'lost') RETURNING execution_epoch",
        run_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(epoch)
}

/// Crash recovery: runs a previous process left `running`
/// become `lost` — the executing instance is gone, but the persisted history
/// and the conversation's resume eligibility are untouched. Called once at
/// server startup, before any new run can be created.
pub async fn mark_stale_runs_lost(pool: &SqlitePool) -> Result<Vec<AgentRun>> {
    let now = chrono::Utc::now().naive_utc();
    let rows = sqlx::query_as!(
        AgentRun,
        "UPDATE agent_runs SET status = 'lost', finished_at = $1 WHERE status = 'running' RETURNING id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, history_epoch, provider_id, model_id, context_window, agent_config_json, error, final_message_id, input_tokens, output_tokens, description, started_at, finished_at",
        now,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_run(pool: &SqlitePool, run_id: &str) -> Result<Option<AgentRun>> {
    let row = sqlx::query_as!(
        AgentRun,
        "SELECT id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, history_epoch, provider_id, model_id, context_window, agent_config_json, error, final_message_id, input_tokens, output_tokens, description, started_at, finished_at FROM agent_runs WHERE id = $1",
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
        "SELECT id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, history_epoch, provider_id, model_id, context_window, agent_config_json, error, final_message_id, input_tokens, output_tokens, description, started_at, finished_at FROM agent_runs WHERE conversation_id = $1 AND (kind = 'subagent' OR history_epoch = (SELECT history_epoch FROM conversations WHERE id = $1)) ORDER BY started_at DESC, rowid DESC LIMIT 1",
        conversation_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// The child run registered for one parent tool call, if any — the
/// registration identity of a `task` delegation (the
/// `(parent_run_id, parent_tool_call_id)` pair is unique).
pub async fn find_run_by_parent_call(
    pool: &SqlitePool,
    parent_run_id: &str,
    parent_tool_call_id: &str,
) -> Result<Option<AgentRun>> {
    let row = sqlx::query_as!(
        AgentRun,
        "SELECT id, conversation_id, kind, parent_run_id, root_run_id, parent_tool_call_id, status, execution_epoch, history_epoch, provider_id, model_id, context_window, agent_config_json, error, final_message_id, input_tokens, output_tokens, description, started_at, finished_at FROM agent_runs WHERE parent_run_id = $1 AND parent_tool_call_id = $2",
        parent_run_id,
        parent_tool_call_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The managed child runs of `call_ids` whose parent run belongs to
/// `parent_conversation_id` — the subset of a conversation's dangling tool
/// calls that are `task` delegations (the recovery coordinator rebuilds the
/// wait for exactly these instead of repairing them with error placeholders).
pub async fn runs_for_parent_calls(
    pool: &SqlitePool,
    parent_conversation_id: &str,
    call_ids: &[String],
) -> Result<Vec<AgentRun>> {
    if call_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids_json = serde_json::to_string(call_ids)?;
    let rows = sqlx::query_as!(
        AgentRun,
        "SELECT r.id, r.conversation_id, r.kind, r.parent_run_id, r.root_run_id, r.parent_tool_call_id, r.status, r.execution_epoch, r.history_epoch, r.provider_id, r.model_id, r.context_window, r.agent_config_json, r.error, r.final_message_id, r.input_tokens, r.output_tokens, r.description, r.started_at, r.finished_at FROM agent_runs r JOIN agent_runs p ON r.parent_run_id = p.id AND p.conversation_id = $1 AND p.history_epoch = (SELECT history_epoch FROM conversations WHERE id = $1) WHERE r.parent_tool_call_id IN (SELECT value FROM json_each($2))",
        parent_conversation_id,
        ids_json,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One row of [`list_subagent_runs`]: the run columns plus its child
/// conversation's title (the task description as shown in the UI). Flat on
/// purpose — `query_as!` maps columns to fields directly.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SubagentRunRow {
    pub id: String,
    pub conversation_id: String,
    pub kind: String,
    pub parent_run_id: Option<String>,
    pub root_run_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
    pub status: String,
    pub execution_epoch: i64,
    pub history_epoch: i64,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub context_window: Option<i64>,
    pub agent_config_json: Option<String>,
    pub error: Option<String>,
    pub final_message_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub description: Option<String>,
    pub started_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
    pub title: String,
}

impl SubagentRunRow {
    /// The run half as the shared [`AgentRun`] shape.
    pub fn into_run(self) -> AgentRun {
        AgentRun {
            id: self.id,
            conversation_id: self.conversation_id,
            kind: self.kind,
            parent_run_id: self.parent_run_id,
            root_run_id: self.root_run_id,
            parent_tool_call_id: self.parent_tool_call_id,
            status: self.status,
            execution_epoch: self.execution_epoch,
            history_epoch: self.history_epoch,
            provider_id: self.provider_id,
            model_id: self.model_id,
            context_window: self.context_window,
            agent_config_json: self.agent_config_json,
            error: self.error,
            final_message_id: self.final_message_id,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            description: self.description,
            started_at: self.started_at,
            finished_at: self.finished_at,
        }
    }
}

/// Every subagent run of a root conversation's task tree (via the child
/// conversations' `root_conversation_id`), running first then oldest first —
/// the ordering the bottom-bar list and the API share.
pub async fn list_subagent_runs(
    pool: &SqlitePool,
    root_conversation_id: &str,
) -> Result<Vec<SubagentRunRow>> {
    let rows = sqlx::query_as!(
        SubagentRunRow,
        "SELECT r.id, r.conversation_id, r.kind, r.parent_run_id, r.root_run_id, r.parent_tool_call_id, r.status, r.execution_epoch, r.history_epoch, r.provider_id, r.model_id, r.context_window, r.agent_config_json, r.error, r.final_message_id, r.input_tokens, r.output_tokens, r.description, r.started_at, r.finished_at, c.title AS title FROM agent_runs r JOIN conversations c ON c.id = r.conversation_id AND c.root_conversation_id = $1 ORDER BY CASE WHEN r.status = 'running' THEN 0 ELSE 1 END, r.started_at ASC",
        root_conversation_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
