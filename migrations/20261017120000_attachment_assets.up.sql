-- Assets own immutable bytes. Upload tickets only authorize a single send;
-- message references, including fork-shared/COW originals, own their lifetime.
CREATE TABLE attachment_assets (
    id TEXT PRIMARY KEY NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    model_storage_key TEXT NOT NULL,
    model_mime_type TEXT NOT NULL,
    model_size_bytes INTEGER NOT NULL,
    width INTEGER,
    height INTEGER,
    normalizer_version INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE attachments RENAME TO staged_uploads;
ALTER TABLE staged_uploads ADD COLUMN asset_id TEXT REFERENCES attachment_assets(id);
INSERT INTO attachment_assets (id, storage_key, mime_type, size_bytes,
    model_storage_key, model_mime_type, model_size_bytes, created_at)
SELECT id, conversation_id || '/' || id, mime_type, size_bytes,
    conversation_id || '/' || id, mime_type, size_bytes, created_at
FROM staged_uploads;
UPDATE staged_uploads SET asset_id = id;
CREATE INDEX idx_staged_uploads_asset ON staged_uploads(asset_id);

CREATE TABLE message_assets (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES attachment_assets(id),
    PRIMARY KEY (message_id, asset_id)
);
CREATE INDEX idx_message_assets_asset ON message_assets(asset_id);

-- Legacy messages remain byte-for-byte unchanged. Backfill the ownership
-- index from known image references and text-file metadata, tolerating bad JSON.
INSERT OR IGNORE INTO message_assets (message_id, asset_id)
SELECT m.id, a.id FROM messages m,
    json_tree(CASE WHEN json_valid(m.parts_json) THEN m.parts_json ELSE '[]' END) j
JOIN attachment_assets a ON j.type = 'text' AND j.atom = 'oct-attachment://' || a.id;
INSERT OR IGNORE INTO message_assets (message_id, asset_id)
SELECT m.id, u.asset_id FROM messages m,
    json_each(CASE WHEN json_valid(m.details_json) THEN m.details_json ELSE '{}' END, '$.attachments') j
JOIN staged_uploads u ON json_extract(CASE WHEN json_valid(j.value) THEN j.value ELSE '{}' END, '$.id') = u.id;
