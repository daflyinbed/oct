//! Generic key-value application settings (the `settings` table): the
//! persisted side of process-level toggles. Known keys are interpreted by
//! their typed application values; simple toggle mappings live alongside
//! storage so callers can share one transaction without a migration.

use anyhow::Result;
use sqlx::{SqliteConnection, SqlitePool};

use crate::tool_contract::McpToolMode;

/// Whether LLM wire logging (per-conversation JSONL under the db
/// directory's `llm-wire/`) is enabled. Row value `off` disables; absent
/// or any other value keeps the default (on).
pub const KEY_LLM_WIRE_LOG: &str = "llm_wire_log";

/// Global/project MCP exposure mode (code/direct, with project inheritance).
pub const KEY_MCP_TOOL_MODE: &str = "mcp_tool_mode";

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

/// Resolve mode with the same project -> global defaults as code mode, on
/// this transaction's connection rather than through a second pool checkout.
pub(crate) async fn mcp_tool_mode(
    conn: &mut SqliteConnection,
    project: Option<&str>,
) -> Result<McpToolMode> {
    let key = KEY_MCP_TOOL_MODE;
    if let Some(id) = project {
        let value = sqlx::query_scalar!(
            "SELECT value FROM project_settings WHERE project_id = ? AND key = ?",
            id,
            key
        )
        .fetch_optional(&mut *conn)
        .await?;
        match value.as_deref() {
            Some("code") => return Ok(McpToolMode::Code),
            Some("direct") => return Ok(McpToolMode::Direct),
            _ => {}
        }
    }
    let value = sqlx::query_scalar!("SELECT value FROM settings WHERE key = ?", key)
        .fetch_optional(conn)
        .await?;
    Ok(if value.as_deref() == Some("direct") {
        McpToolMode::Direct
    } else {
        McpToolMode::Code
    })
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
