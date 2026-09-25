-- Background bash task lifecycle rows (ADR 0003): one row per command the
-- execute_command tool started with run_in_background = true. The in-memory
-- registry holds only ACTIVE tasks; this table is the durable truth a restart
-- (mark-stale-lost) and the notification catch-up read from. Status values
-- are enforced in code, not CHECK-constrained (same policy as agent_runs):
-- running | completed | failed | cancelled | lost
-- (`lost` = the backend process died before the supervisor settled the row;
--  PDEATHSIG already terminated the command itself).
-- `notified` = 1 once a <task-notification> user message for this task has
-- been inserted (same transaction as the message), so wake runs and the
-- send/resume catch-up never deliver the same completion twice.
CREATE TABLE IF NOT EXISTS background_tasks (
    id              TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    parent_run_id   TEXT REFERENCES agent_runs(id) ON DELETE CASCADE,
    parent_call_id  TEXT NOT NULL,
    command         TEXT NOT NULL,
    working_dir     TEXT,
    status          TEXT NOT NULL DEFAULT 'running',
    exit_code       INTEGER,
    output_path     TEXT NOT NULL,
    output_bytes    INTEGER,
    error           TEXT,
    notified        INTEGER NOT NULL DEFAULT 0,
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at     TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_background_tasks_conversation ON background_tasks(conversation_id);
