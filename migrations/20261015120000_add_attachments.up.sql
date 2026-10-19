-- Chat attachment uploads: bytes live on the filesystem under the data
-- directory's `attachments/` tree; this table is the staging ledger and
-- display-metadata source (id → file, filename/mime/size for the API).
--
-- Deliberately NO FK on conversation_id and NO cascade: content rows are
-- shared across forks, so a fork's parts_json can reference an attachment
-- uploaded in the original conversation — deleting that conversation must
-- not destroy the bytes the fork still renders. Reclamation is entirely the
-- sweep's job (see attachments::AttachmentStore::sweep): rows/files die when
-- no live parts_json references them (state='sent') or when a staged upload
-- was never sent within the TTL. `state`: staged | sent.
CREATE TABLE IF NOT EXISTS attachments (
    id              TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL,
    filename        TEXT NOT NULL,
    mime_type       TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    state           TEXT NOT NULL DEFAULT 'staged',
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_attachments_conversation ON attachments(conversation_id);
CREATE INDEX IF NOT EXISTS idx_attachments_state_created ON attachments(state, created_at);
