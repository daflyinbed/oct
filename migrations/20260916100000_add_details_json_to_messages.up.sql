-- UI-only structured tool-result metadata (JSON). Kept separate from
-- parts_json so it is never fed back to the LLM.
ALTER TABLE messages ADD COLUMN details_json TEXT;
