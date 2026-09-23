-- OAuth 2.0 tokens for http-transport MCP servers (authorization code + PKCE).
-- One row per server (PRIMARY KEY on server_id doubles as the UNIQUE
-- constraint): the client registration (client_id/secret from RFC 7591 DCR or
-- an existing one reused on re-login) and the latest token set live together,
-- so a re-login reuses the registered client instead of re-registering.
-- Tokens NEVER leave the backend: the API layer exposes only has_token /
-- has_refresh_token / expires_at summaries, and the row struct deliberately
-- derives no ToSchema so it cannot leak into the OpenAPI document either.
-- FK + ON DELETE CASCADE follows the mcp_servers pattern (dropping a server
-- row drops its credentials; the pool enables the foreign_keys pragma).
CREATE TABLE IF NOT EXISTS mcp_server_tokens (
    server_id     TEXT PRIMARY KEY NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    client_id     TEXT NOT NULL,
    client_secret TEXT,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'Bearer',
    scope         TEXT,
    expires_at    TIMESTAMP,
    obtained_at   TIMESTAMP NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
