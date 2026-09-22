-- MCP server registrations for the agent's tool providers.
-- Phase 1 only uses the 'stdio' transport; the column is deliberately NOT
-- CHECK-constrained so Phase 2 can add 'http' without a table rewrite.
CREATE TABLE IF NOT EXISTS mcp_servers (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    transport       TEXT NOT NULL,
    command         TEXT,
    args_json       TEXT,
    env_json        TEXT,
    url             TEXT,
    headers_json    TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    tool_timeout_ms INTEGER,
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
