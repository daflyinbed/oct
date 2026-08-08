CREATE TABLE IF NOT EXISTS providers (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    adapter_type    TEXT NOT NULL,
    base_url        TEXT,
    api_key         TEXT NOT NULL DEFAULT '',
    doc_url         TEXT,
    source          TEXT NOT NULL DEFAULT 'builtin',
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS models (
    id                TEXT PRIMARY KEY NOT NULL,
    provider_id       TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id          TEXT NOT NULL,
    name              TEXT NOT NULL,
    family            TEXT,
    reasoning         BOOLEAN NOT NULL DEFAULT false,
    tool_call         BOOLEAN NOT NULL DEFAULT false,
    attachment        BOOLEAN NOT NULL DEFAULT false,
    structured_output BOOLEAN NOT NULL DEFAULT false,
    temperature       BOOLEAN NOT NULL DEFAULT true,
    knowledge         TEXT,
    release_date      TEXT,
    open_weights      BOOLEAN NOT NULL DEFAULT false,
    cost_input        REAL,
    cost_output       REAL,
    cost_cache_read   REAL,
    cost_cache_write  REAL,
    limit_context     INTEGER,
    limit_output      INTEGER,
    modalities_input  TEXT,
    modalities_output TEXT,
    source            TEXT NOT NULL DEFAULT 'builtin',
    is_enabled        BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(provider_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_models_provider ON models(provider_id);
CREATE INDEX IF NOT EXISTS idx_models_family ON models(family);

CREATE TABLE IF NOT EXISTS projects (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    working_dir TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS conversations (
    id          TEXT PRIMARY KEY NOT NULL,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title       TEXT NOT NULL DEFAULT 'New conversation',
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_conversations_project ON conversations(project_id);

CREATE TABLE IF NOT EXISTS messages (
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
    UNIQUE(conversation_id, ordering)
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id, ordering);
