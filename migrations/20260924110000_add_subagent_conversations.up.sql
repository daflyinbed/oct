-- Subagent sessions (ADR 0002 §6.2): subagent runs live in their own
-- conversations, linked back to the root chat conversation they serve.
--   * kind — 'chat' (sidebar-visible) or 'subagent' (managed child session;
--     never listed, never auto-titled, never sent to directly).
--   * parent_conversation_id — the conversation whose run delegated this
--     task (informational lineage; no FK so deleting a tree is one
--     application-level statement).
--   * root_conversation_id — the root chat conversation of the whole task
--     tree (equals id for kind='chat' rows is NOT enforced; NULL for chats).
--   * agent_type — 'worker' for subagent rows; NULL for chats.
-- agent_runs.description carries the task's UI title (the `task` tool's
-- `description` argument). The (parent_run_id, parent_tool_call_id) pair is
-- the registration identity, so its index becomes UNIQUE: one child run per
-- parent tool call, enforced at the storage layer for idempotent
-- registration under concurrent re-entry.
ALTER TABLE conversations ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE conversations ADD COLUMN parent_conversation_id TEXT;
ALTER TABLE conversations ADD COLUMN root_conversation_id TEXT;
ALTER TABLE conversations ADD COLUMN agent_type TEXT;
ALTER TABLE agent_runs ADD COLUMN description TEXT;
CREATE INDEX IF NOT EXISTS idx_conversations_root ON conversations(root_conversation_id);
DROP INDEX IF EXISTS idx_agent_runs_parent_call;
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_runs_parent_call ON agent_runs(parent_run_id, parent_tool_call_id);
