ALTER TABLE conversations DROP COLUMN permission_mode;
DROP INDEX IF EXISTS idx_permission_rules_project;
DROP TABLE IF EXISTS permission_rules;
