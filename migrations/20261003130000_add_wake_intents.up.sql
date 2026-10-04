-- Durable distinction between a delivered notification and a consumed wake.
CREATE TABLE wake_intents (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id),
    run_id TEXT REFERENCES agent_runs(id) ON DELETE SET NULL
);
