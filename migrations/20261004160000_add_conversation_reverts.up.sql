ALTER TABLE conversations ADD COLUMN origin_kind TEXT NOT NULL DEFAULT 'normal' CHECK (origin_kind IN ('normal', 'fork', 'revert_backup'));
ALTER TABLE conversations ADD COLUMN history_epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE conversations ADD COLUMN backup_of_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL;
ALTER TABLE conversations ADD COLUMN backup_at_revision INTEGER;
UPDATE conversations SET origin_kind = 'fork' WHERE forked_at_revision IS NOT NULL;

ALTER TABLE agent_runs ADD COLUMN history_epoch INTEGER NOT NULL DEFAULT 0;

CREATE TABLE conversation_revert_operations (
    id TEXT PRIMARY KEY,
    source_conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    backup_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    request_json TEXT NOT NULL,
    response_json TEXT NOT NULL
);
