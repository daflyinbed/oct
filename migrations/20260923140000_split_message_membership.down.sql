-- Reverse the split: fold membership back into per-conversation message
-- rows. Copy-on-write swaps are RESTORED first — every slot pointing at a
-- cleared copy goes back to the original it supersedes — so the fold carries
-- the original content forward and no row is lost; the copies become orphans
-- and are dropped together with the split tables.

UPDATE conversation_messages
SET message_id = (
    SELECT c.supersedes_message_id FROM messages c
    WHERE c.id = conversation_messages.message_id
)
WHERE rowid IN (
    SELECT cm.rowid FROM conversation_messages cm
    JOIN messages c ON c.id = cm.message_id
    WHERE c.supersedes_message_id IS NOT NULL
);

ALTER TABLE messages RENAME TO messages_content;

CREATE TABLE messages (
    id              TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role            TEXT NOT NULL,
    parts_json      TEXT NOT NULL,
    ordering        INTEGER NOT NULL,
    provider_id     TEXT,
    model_id        TEXT,
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    reasoning_tokens INTEGER,
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    details_json    TEXT,
    UNIQUE(conversation_id, ordering)
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id, ordering);

INSERT INTO messages (id, conversation_id, role, parts_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at, details_json)
SELECT c.id, cm.conversation_id, c.role, c.parts_json, cm.ordering, c.provider_id, c.model_id, c.input_tokens, c.output_tokens, c.reasoning_tokens, c.created_at, c.details_json
FROM messages_content c
JOIN conversation_messages cm ON cm.message_id = c.id;

DROP TABLE conversation_messages;
DROP TABLE messages_content;

ALTER TABLE conversations DROP COLUMN context_revision;
