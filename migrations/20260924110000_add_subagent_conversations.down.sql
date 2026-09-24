DROP INDEX IF EXISTS idx_agent_runs_parent_call;
CREATE INDEX IF NOT EXISTS idx_agent_runs_parent_call ON agent_runs(parent_run_id, parent_tool_call_id);
DROP INDEX IF EXISTS idx_conversations_root;
ALTER TABLE agent_runs DROP COLUMN description;
ALTER TABLE conversations DROP COLUMN agent_type;
ALTER TABLE conversations DROP COLUMN root_conversation_id;
ALTER TABLE conversations DROP COLUMN parent_conversation_id;
ALTER TABLE conversations DROP COLUMN kind;
