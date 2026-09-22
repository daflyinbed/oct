-- Project scoping for MCP servers: NULL = global (visible to every
-- project), non-NULL = visible only to that project's runs. The FK + ON
-- DELETE CASCADE follows the conversations→projects pattern (the pool
-- enables the foreign_keys pragma, so deleting a project removes its
-- scoped MCP server rows just like it removes its conversations).
ALTER TABLE mcp_servers ADD COLUMN project_id TEXT REFERENCES projects(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_mcp_servers_project ON mcp_servers(project_id);
