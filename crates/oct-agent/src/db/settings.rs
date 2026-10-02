//! Generic key-value application settings (the `settings` table): the
//! persisted side of process-level toggles. Known keys are owned by the
//! API layer's typed DTOs — this module is deliberately dumb storage so
//! new toggles never need a migration.

use anyhow::Result;
use sqlx::SqlitePool;

/// Whether LLM wire logging (per-conversation JSONL under the db
/// directory's `llm-wire/`) is enabled. Row value `off` disables; absent
/// or any other value keeps the default (on).
pub const KEY_LLM_WIRE_LOG: &str = "llm_wire_log";

/// The effective `llm_wire_log` state for a stored row value: `off` (and
/// only `off`) turns the sink off — everything else, including no row at
/// all, is the default-on.
pub fn llm_wire_log_enabled(stored: Option<&str>) -> bool {
    stored != Some("off")
}

pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let value = sqlx::query_scalar!("SELECT value FROM settings WHERE key = ?", key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = CURRENT_TIMESTAMP",
        key,
        value
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_log_row_mapping_is_off_only_on_the_exact_value() {
        // Absent / "on" / garbage all keep the default-on; only "off" (and
        // nothing else) disables — a hand-edited row must not silently
        // kill the sink on a typo.
        assert!(llm_wire_log_enabled(None));
        assert!(llm_wire_log_enabled(Some("on")));
        assert!(llm_wire_log_enabled(Some("ON")));
        assert!(llm_wire_log_enabled(Some("disabled?")));
        assert!(!llm_wire_log_enabled(Some("off")));
    }
}
