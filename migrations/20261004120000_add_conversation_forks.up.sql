ALTER TABLE conversations ADD COLUMN forked_from_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL;
ALTER TABLE conversations ADD COLUMN forked_at_message_id TEXT;
ALTER TABLE conversations ADD COLUMN forked_at_revision INTEGER;

CREATE TABLE conversation_fork_operations (
    id TEXT PRIMARY KEY,
    source_conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    result_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    request_json TEXT NOT NULL,
    response_json TEXT NOT NULL
);
