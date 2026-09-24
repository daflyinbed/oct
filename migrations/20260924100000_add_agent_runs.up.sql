-- Persistent run lifecycle rows (ADR 0002 §6.2): one row per agent
-- execution over a conversation. Root runs only for now — subagent runs
-- (later PRs) reuse this table via kind/parent_run_id/parent_tool_call_id.
-- Status values are enforced in code, not CHECK-constrained, so new states
-- never need a table rewrite: running | completed | failed | cancelled | lost
-- (`lost` = the executing process died before settling the run).
CREATE TABLE IF NOT EXISTS agent_runs (
    id                  TEXT PRIMARY KEY NOT NULL,
    conversation_id     TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL DEFAULT 'root',
    parent_run_id       TEXT REFERENCES agent_runs(id) ON DELETE CASCADE,
    root_run_id         TEXT,
    parent_tool_call_id TEXT,
    status              TEXT NOT NULL DEFAULT 'running',
    execution_epoch     INTEGER NOT NULL DEFAULT 1,
    provider_id         TEXT,
    model_id            TEXT,
    context_window      INTEGER,
    error               TEXT,
    final_message_id    TEXT,
    input_tokens        INTEGER,
    output_tokens       INTEGER,
    started_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at         TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_agent_runs_conversation ON agent_runs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_agent_runs_parent_call ON agent_runs(parent_run_id, parent_tool_call_id);
