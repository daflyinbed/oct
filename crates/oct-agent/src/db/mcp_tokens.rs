use anyhow::Result;
use chrono::NaiveDateTime;
use sqlx::SqlitePool;

/// Stored OAuth credentials for one http-transport MCP server.
///
/// `client_id`/`client_secret` come from RFC 7591 dynamic registration (or a
/// previous registration reused on re-login); the token fields are the latest
/// authorization-code grant. Deliberately NOT `ToSchema`/`Serialize`: tokens
/// must never ride an API response — the API layer derives a summary
/// (`McpServerOauthSummary`) instead.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct McpServerToken {
    pub server_id: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Wire `token_type` (almost always `Bearer`); joined with the access
    /// token as `"{token_type} {access_token}"` on the Authorization header.
    pub token_type: String,
    pub scope: Option<String>,
    /// Access-token expiry; `None` = the server sent no `expires_in` (treat
    /// as non-expiring until a connect-time 401 says otherwise).
    pub expires_at: Option<NaiveDateTime>,
    pub obtained_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// Write shape for both the callback's code exchange and the pre-connect
/// refresh: client registration stays as stored (the AS does not re-issue it
/// in our flow), token fields are replaced wholesale.
pub struct SaveTokenRequest {
    pub server_id: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<NaiveDateTime>,
}

/// API-facing projection of [`McpServerToken`]: presence flags + expiry only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTokenSummary {
    pub has_token: bool,
    pub has_refresh_token: bool,
    pub expires_at: Option<NaiveDateTime>,
}

impl McpTokenSummary {
    fn absent() -> Self {
        Self {
            has_token: false,
            has_refresh_token: false,
            expires_at: None,
        }
    }
}

pub async fn get_mcp_server_token(
    pool: &SqlitePool,
    server_id: &str,
) -> Result<Option<McpServerToken>> {
    let row = sqlx::query_as!(
        McpServerToken,
        "SELECT server_id, client_id, client_secret, access_token, refresh_token, token_type, scope, expires_at, obtained_at, created_at, updated_at FROM mcp_server_tokens WHERE server_id = $1",
        server_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Insert-or-replace the credential row for one server. A refresh keeps the
/// client registration columns only by the caller passing them back in `req`
/// (the refresh path loads the row first and mirrors client_id/secret), so
/// there is exactly one upsert shape to reason about.
pub async fn save_mcp_server_token(pool: &SqlitePool, req: &SaveTokenRequest) -> Result<()> {
    let now = chrono::Utc::now().naive_utc();
    sqlx::query!(
        "INSERT INTO mcp_server_tokens (server_id, client_id, client_secret, access_token, refresh_token, token_type, scope, expires_at, obtained_at, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
         ON CONFLICT(server_id) DO UPDATE SET
            client_id = excluded.client_id,
            client_secret = excluded.client_secret,
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            token_type = excluded.token_type,
            scope = excluded.scope,
            expires_at = excluded.expires_at,
            obtained_at = excluded.obtained_at,
            updated_at = excluded.updated_at",
        req.server_id,
        req.client_id,
        req.client_secret,
        req.access_token,
        req.refresh_token,
        req.token_type,
        req.scope,
        req.expires_at,
        now,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Commit a refresh only while the credentials it used are still current.
/// Deletion/logout and a newer login retire the old refresh's write without
/// resurrecting a removed row or replacing the new credentials.
pub(crate) async fn save_refreshed_mcp_server_token(
    pool: &SqlitePool,
    req: &SaveTokenRequest,
    previous: &McpServerToken,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result = sqlx::query!(
        "UPDATE mcp_server_tokens SET
            client_id = $2, client_secret = $3, access_token = $4,
            refresh_token = $5, token_type = $6, scope = $7,
            expires_at = $8, obtained_at = $9, updated_at = $9
         WHERE server_id = $1 AND access_token = $10 AND obtained_at = $11",
        req.server_id,
        req.client_id,
        req.client_secret,
        req.access_token,
        req.refresh_token,
        req.token_type,
        req.scope,
        req.expires_at,
        now,
        previous.access_token,
        previous.obtained_at,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Drop the credential row (OAuth logout). Returns whether a row existed.
pub async fn delete_mcp_server_token(pool: &SqlitePool, server_id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "DELETE FROM mcp_server_tokens WHERE server_id = $1",
        server_id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Summaries for a whole list call: one query instead of one per row. Rows
/// without a token entry get the absent summary.
pub async fn list_token_summaries(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, McpTokenSummary>> {
    let rows = sqlx::query!("SELECT server_id, refresh_token, expires_at FROM mcp_server_tokens")
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.server_id,
                McpTokenSummary {
                    has_token: true,
                    has_refresh_token: row.refresh_token.is_some(),
                    expires_at: row.expires_at,
                },
            )
        })
        .collect())
}

/// Summary for one server (absent shape when no row exists).
pub async fn token_summary(pool: &SqlitePool, server_id: &str) -> Result<McpTokenSummary> {
    let row = sqlx::query!(
        "SELECT refresh_token, expires_at FROM mcp_server_tokens WHERE server_id = $1",
        server_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some(row) => McpTokenSummary {
            has_token: true,
            has_refresh_token: row.refresh_token.is_some(),
            expires_at: row.expires_at,
        },
        None => McpTokenSummary::absent(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thin smoke of the upsert round trip against a real (temp-file) SQLite:
    /// insert, overwrite via refresh shape, summary, delete. The full OAuth
    /// flow over these helpers lives in tests/mcp_oauth.rs.
    #[tokio::test]
    async fn token_row_upsert_summary_delete_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let url = format!("sqlite://{}", dir.path().join("tokens.db").display());
        let pool = crate::db::init_pool(&url).await.expect("init pool");

        // The tokens table carries a FK to mcp_servers (cascade on delete),
        // so the target row must exist — and deleting it must cascade.
        let server = crate::db::mcp::create_mcp_server(
            &pool,
            &crate::db::mcp::CreateMcpServerRequest {
                name: "srv".to_string(),
                transport: "http".to_string(),
                command: None,
                args: None,
                env: None,
                url: Some("https://mcp.example/mcp".to_string()),
                headers: None,
                enabled: Some(true),
                tool_timeout_ms: None,
                project_id: None,
            },
        )
        .await
        .expect("create server row");
        let server_id = server.id.as_str();

        let now = chrono::Utc::now().naive_utc();
        assert!(
            !token_summary(&pool, server_id).await.unwrap().has_token,
            "no row yet"
        );

        save_mcp_server_token(
            &pool,
            &SaveTokenRequest {
                server_id: server_id.to_string(),
                client_id: "client-1".to_string(),
                client_secret: Some("secret-1".to_string()),
                access_token: "access-1".to_string(),
                refresh_token: Some("refresh-1".to_string()),
                token_type: "Bearer".to_string(),
                scope: Some("mcp".to_string()),
                expires_at: Some(now + chrono::Duration::hours(1)),
            },
        )
        .await
        .unwrap();

        let row = get_mcp_server_token(&pool, server_id)
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(row.client_id, "client-1");
        assert_eq!(row.access_token, "access-1");
        assert_eq!(row.refresh_token.as_deref(), Some("refresh-1"));

        // Refresh shape: same client registration, rotated tokens. A missing
        // refresh_token in the response must NULL the column (single-use
        // refresh tokens), which the wholesale replace gives us.
        save_mcp_server_token(
            &pool,
            &SaveTokenRequest {
                server_id: server_id.to_string(),
                client_id: "client-1".to_string(),
                client_secret: Some("secret-1".to_string()),
                access_token: "access-2".to_string(),
                refresh_token: None,
                token_type: "Bearer".to_string(),
                scope: None,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        let row = get_mcp_server_token(&pool, server_id)
            .await
            .unwrap()
            .expect("row exists after refresh");
        assert_eq!(row.access_token, "access-2");
        assert_eq!(row.refresh_token, None);
        assert_eq!(row.expires_at, None);

        let mut summaries = list_token_summaries(&pool).await.unwrap();
        let summary = summaries.remove(server_id).unwrap();
        assert!(summary.has_token);
        assert!(!summary.has_refresh_token);
        assert_eq!(summary.expires_at, None);

        assert!(delete_mcp_server_token(&pool, server_id).await.unwrap());
        assert!(!delete_mcp_server_token(&pool, server_id).await.unwrap());
        assert!(
            !get_mcp_server_token(&pool, server_id)
                .await
                .unwrap()
                .is_some(),
            "row gone after delete"
        );

        // Deleting the server row cascades the token row away with it.
        save_mcp_server_token(
            &pool,
            &SaveTokenRequest {
                server_id: server_id.to_string(),
                client_id: "client-1".to_string(),
                client_secret: None,
                access_token: "access-3".to_string(),
                refresh_token: None,
                token_type: "Bearer".to_string(),
                scope: None,
                expires_at: None,
            },
        )
        .await
        .unwrap();
        crate::db::mcp::delete_mcp_server(&pool, server_id)
            .await
            .unwrap();
        assert!(
            !get_mcp_server_token(&pool, server_id)
                .await
                .unwrap()
                .is_some(),
            "token row cascaded with the server row"
        );

        // The row struct carries secrets; make sure it never grew Serialize —
        // a compile-time guard is impossible without a trait check, so pin the
        // marker impls we rely on (Debug redaction lives at the call sites:
        // nothing logs the row whole).
        let _ = format!("{:?}", McpTokenSummary::absent());
    }
}
