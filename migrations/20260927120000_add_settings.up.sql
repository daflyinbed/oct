-- Generic key-value application settings: the persisted side of
-- process-level toggles that must survive restarts. Known keys are owned
-- (validated and typed) by the API layer, not by this schema; the file is
-- deliberately dumb storage. v1 key: `llm_wire_log` ('on'/'off', absent or
-- any other value = on) driving the LLM wire log sink in
-- oct-llm-provider's `wire` module.
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
