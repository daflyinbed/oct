DROP TABLE conversation_revert_operations;
ALTER TABLE agent_runs DROP COLUMN history_epoch;
ALTER TABLE conversations DROP COLUMN backup_at_revision;
ALTER TABLE conversations DROP COLUMN backup_of_conversation_id;
ALTER TABLE conversations DROP COLUMN history_epoch;
ALTER TABLE conversations DROP COLUMN origin_kind;
