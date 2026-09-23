-- Split messages into pure content rows plus a per-conversation membership
-- timeline. Content rows are insert-only (usage backfill aside) and shared
-- across future forks; the membership table owns ordering and every future
-- rollback/fork/copy-on-write operation.
--
-- `supersedes_message_id` marks a copy-on-write replacement (microcompact):
-- the copy points back at the original content row it replaced (always one
-- hop — placeholder rows are never re-replaced).
--
-- The two revision columns order every view mutation independently of
-- `ordering` (which marker insertion renumbers):
--   * appended_at_revision — stamped when the row joins the timeline, never
--     changed afterwards. Rollback/fork cut points and usage-anchor
--     eligibility compare against it.
--   * swapped_at_revision — stamped when a COW pointer swap happens; NULL
--     means the slot still shows its original content.
-- Both are stamped from conversations.context_revision, which is bumped
-- inside every mutating transaction and therefore strictly orders appends,
-- marker insertions and swaps.

ALTER TABLE messages RENAME TO messages_legacy;

ALTER TABLE conversations ADD COLUMN context_revision INTEGER NOT NULL DEFAULT 0;

CREATE TABLE messages (
    id                    TEXT PRIMARY KEY NOT NULL,
    role                  TEXT NOT NULL,
    parts_json            TEXT NOT NULL,
    details_json          TEXT,
    provider_id           TEXT,
    model_id              TEXT,
    input_tokens          INTEGER,
    output_tokens         INTEGER,
    reasoning_tokens      INTEGER,
    supersedes_message_id TEXT REFERENCES messages(id),
    created_at            TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE conversation_messages (
    conversation_id      TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id           TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordering             INTEGER NOT NULL,
    appended_at_revision INTEGER NOT NULL,
    swapped_at_revision  INTEGER,
    PRIMARY KEY (conversation_id, message_id),
    UNIQUE (conversation_id, ordering)
);

CREATE INDEX idx_conversation_messages_message ON conversation_messages(message_id);

-- Backfill. The legacy table is append-only, so its per-conversation
-- `ordering` IS a valid linearization of history: reusing it as
-- appended_at_revision preserves every before/after relationship (an old
-- compaction marker keeps a strictly smaller revision than the assistants
-- generated after it, so existing usage anchors stay eligible).
INSERT INTO messages (id, role, parts_json, details_json, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at)
SELECT id, role, parts_json, details_json, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at
FROM messages_legacy;

INSERT INTO conversation_messages (conversation_id, message_id, ordering, appended_at_revision)
SELECT conversation_id, id, ordering, ordering
FROM messages_legacy;

UPDATE conversations
SET context_revision = COALESCE((
    SELECT MAX(cm.ordering) FROM conversation_messages cm
    WHERE cm.conversation_id = conversations.id
), 0);

DROP TABLE messages_legacy;
