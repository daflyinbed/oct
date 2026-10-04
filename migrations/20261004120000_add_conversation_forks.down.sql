DROP TABLE conversation_fork_operations;
ALTER TABLE conversations DROP COLUMN forked_at_revision;
ALTER TABLE conversations DROP COLUMN forked_at_message_id;
ALTER TABLE conversations DROP COLUMN forked_from_conversation_id;
