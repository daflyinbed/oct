-- V2 snapshots and shared tickets cannot be represented by the legacy
-- filesystem layout. Refuse a lossy downgrade after new content is written.
CREATE TEMP TABLE attachment_downgrade_guard (
    incompatible INTEGER CONSTRAINT cannot_downgrade_v2_attachment_content CHECK (incompatible = 0)
);
INSERT INTO attachment_downgrade_guard SELECT
    (SELECT COUNT(*) FROM attachment_assets WHERE normalizer_version > 0) +
    (SELECT COUNT(*) FROM staged_uploads WHERE asset_id != id) +
    (SELECT COUNT(*) FROM messages WHERE json_extract(CASE WHEN json_valid(details_json) THEN details_json ELSE '{}' END, '$.user_content') IS NOT NULL);
DROP TABLE attachment_downgrade_guard;
DROP TABLE message_assets;
DROP INDEX idx_staged_uploads_asset;
ALTER TABLE staged_uploads DROP COLUMN asset_id;
ALTER TABLE staged_uploads RENAME TO attachments;
DROP TABLE attachment_assets;
