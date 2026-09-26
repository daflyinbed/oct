-- Tool permission layer (ADR 0004): the persistent rule chain every tool
-- call is evaluated against, plus the per-conversation mode that provides
-- the default decision when no rule matches.
--   * permission_rules — one allow/deny/ask rule per row. project_id NULL =
--     a global rule, non-NULL = scoped to that project; conversation_id is
--     reserved for future "always allow within this conversation" rules.
--     tool is an exact tool name or the `mcp__{server}__*` wildcard; pattern
--     is NULL (any invocation of the tool), a command prefix for
--     execute_command, or an absolute-path glob for the file tools. source
--     records who created the rule: `user` (settings page) or `approval`
--     (an "always allow" answer).
--   * conversations.permission_mode — default | accept_edits | bypass, the
--     mode axis of the ADR 0004 §2 decision table. Unlike the status-ish
--     columns elsewhere, these two enumerations ARE CHECK-constrained: the
--     value sets are part of the design, not an implementation state that
--     new rows might extend.
CREATE TABLE IF NOT EXISTS permission_rules (
    id              TEXT PRIMARY KEY NOT NULL,
    project_id      TEXT REFERENCES projects(id) ON DELETE CASCADE,
    conversation_id TEXT REFERENCES conversations(id) ON DELETE CASCADE,
    effect          TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'ask')),
    tool            TEXT NOT NULL,
    pattern         TEXT,
    source          TEXT NOT NULL DEFAULT 'user' CHECK (source IN ('user', 'approval')),
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_permission_rules_project ON permission_rules(project_id);

ALTER TABLE conversations ADD COLUMN permission_mode TEXT NOT NULL DEFAULT 'default';
