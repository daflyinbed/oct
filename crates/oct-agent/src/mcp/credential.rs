//! The MCP OAuth credential service: the one path every consumer of stored
//! server credentials goes through (the connect pipeline here; the login
//! flow and API-layer auto-refresh stay in [`crate::oauth`] / `api::mcp`,
//! which share the same refresh helpers).
//!
//! Responsibilities: load the stored token row, refresh it when near expiry
//! (one best-effort attempt, the stored row survives a failing refresh),
//! and merge the `Authorization` header over the row's static headers.

use std::collections::HashMap;

use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::db;
use crate::db::mcp_tokens::McpServerToken;
use crate::oauth;

/// Lead time before `expires_at` at which a stored OAuth access token counts
/// as stale and gets refreshed pre-connect. 30s covers clock skew between
/// oct and the AS plus the MCP handshake that follows.
const TOKEN_EXPIRY_LEAD: chrono::Duration = chrono::Duration::seconds(30);

/// Result of [`authorize_headers`].
pub(crate) struct AuthorizedHeaders {
    pub headers: HashMap<String, String>,
    /// Whether a stored OAuth token contributed the `Authorization` header
    /// (drives the re-login hint when the connect looks like a 401).
    pub had_token: bool,
}

/// Headers for an http-transport connect: the row's static `headers_json`
/// with the stored OAuth token merged in as `Authorization`.
///
/// PRIORITY: the stored OAuth token WINS over a static `Authorization` in
/// the row's `headers_json` (an explicit OAuth login is the more specific
/// intent); without a token row, `headers_json` passes through verbatim —
/// the Phase-2-static-bearer behavior is unchanged.
pub(crate) async fn authorize_headers(
    pool: &SqlitePool,
    server_id: &str,
    server_url: &str,
    server_name: &str,
    static_headers: Option<&HashMap<String, String>>,
) -> AuthorizedHeaders {
    let token = match db::mcp_tokens::get_mcp_server_token(pool, server_id).await {
        Ok(token) => token,
        Err(error) => {
            warn!(
                server = %server_name,
                "cannot read the stored OAuth token, connecting without it: {error:#}"
            );
            None
        }
    };
    // Refresh first (may write the DB row), then merge the header.
    let token = match token {
        Some(token) => Some(refresh_token(pool, server_url, token, false).await.0),
        None => None,
    };
    let had_token = token.is_some();
    let mut headers = static_headers.cloned().unwrap_or_default();
    if let Some(token) = &token {
        // HTTP header names are case-insensitive. Remove every spelling so
        // normalization in oct-mcp cannot randomly retain a static bearer.
        headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
        let token_type = if token.token_type.trim().is_empty() {
            "Bearer"
        } else {
            token.token_type.trim()
        };
        headers.insert(
            "Authorization".to_string(),
            format!("{token_type} {}", token.access_token),
        );
    }
    AuthorizedHeaders { headers, had_token }
}

/// Whether a connect failure looks like an authorization rejection (an
/// OAuth token was in play and the transport surfaced a 401-shaped error).
/// rmcp's error chains render the HTTP status (`HTTP 401`) or the
/// WWW-Authenticate challenge text; match those spellings, case-blind.
pub(crate) fn looks_like_auth_rejection(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("http 401")
        || message.contains("401 unauthorized")
        || message.contains("unauthorized")
        || message.contains("authorization required")
        || message.contains("invalid_token")
}

/// Same freshness policy for connected entries and the connect pipeline.
pub(crate) fn token_expiring(expires_at: Option<chrono::NaiveDateTime>) -> bool {
    expires_at.is_some_and(|at| Utc::now().naive_utc() + TOKEN_EXPIRY_LEAD >= at)
}

/// A 401 can revoke a token before its advertised expiry (or without an
/// expiry). Refresh once under the manager's server gate, on the NEXT
/// assembly; the rejected call itself is never replayed.
pub(crate) async fn refresh_after_rejection(pool: &SqlitePool, row: &db::mcp::McpServer) -> bool {
    let Some(url) = row.url.as_deref() else {
        return false;
    };
    match db::mcp_tokens::get_mcp_server_token(pool, &row.id).await {
        Ok(Some(token)) => refresh_token(pool, url, token, true).await.1,
        Ok(None) => false,
        Err(error) => {
            warn!("cannot load rejected OAuth credential: {error:#}");
            false
        }
    }
}

/// One best-effort attempt. The bool records a durable refresh, allowing a
/// failed handshake to retain an earlier rejection until recovery succeeds.
async fn refresh_token(
    pool: &SqlitePool,
    server_url: &str,
    token: McpServerToken,
    force: bool,
) -> (McpServerToken, bool) {
    let Some(refresh_token) = token.refresh_token.as_deref() else {
        return (token, false); // No refresh token: nothing to do, try as-is.
    };
    if !force && !token_expiring(token.expires_at) {
        return (token, false);
    }
    info!("stored OAuth access token is near expiry, refreshing");
    let refreshed = async {
        let client = oauth::oauth_http_client()?;
        let metadata = oauth::discover_authorization_server(&client, server_url).await?;
        oauth::refresh_access_token(
            &client,
            &metadata.token_endpoint,
            &token.client_id,
            token.client_secret.as_deref(),
            refresh_token,
        )
        .await
    }
    .await;
    match refreshed {
        Ok(response) => {
            let now = Utc::now().naive_utc();
            let expires_at = response
                .expires_in
                .filter(|seconds| *seconds > 0)
                .map(|seconds| now + chrono::Duration::seconds(seconds));
            // RFC 6749 §6: an absent refresh_token in the response means the
            // old one stays valid — keep it instead of NULLing the column.
            let refresh_token = response
                .refresh_token
                .or_else(|| token.refresh_token.clone());
            let updated = McpServerToken {
                server_id: token.server_id.clone(),
                client_id: token.client_id.clone(),
                client_secret: token.client_secret.clone(),
                access_token: response.access_token,
                refresh_token,
                token_type: if response.token_type.trim().is_empty() {
                    "Bearer".to_string()
                } else {
                    response.token_type
                },
                scope: response.scope,
                expires_at,
                obtained_at: now,
                created_at: token.created_at,
                updated_at: token.updated_at,
            };
            // Rotation can revoke the old refresh token immediately. Retain
            // the new credentials in an independent writer until durable;
            // dropping a connection attempt must not drop the only copy.
            // Removed/replaced credentials retire the writer instead of
            // retrying forever or resurrecting a server's OAuth login.
            let pool = pool.clone();
            let request = db::mcp_tokens::SaveTokenRequest {
                server_id: updated.server_id.clone(),
                client_id: updated.client_id.clone(),
                client_secret: updated.client_secret.clone(),
                access_token: updated.access_token.clone(),
                refresh_token: updated.refresh_token.clone(),
                token_type: updated.token_type.clone(),
                scope: updated.scope.clone(),
                expires_at: updated.expires_at,
            };
            let writer = tokio::spawn(async move {
                db::persist_critical(&pool, || {
                    db::mcp_tokens::save_refreshed_mcp_server_token(&pool, &request, &token)
                })
                .await
            });
            let persisted = match writer.await {
                Ok(Ok(true)) => true,
                Ok(Ok(false)) => {
                    info!("refreshed credentials discarded after removal or replacement");
                    false
                }
                Ok(Err(error)) => {
                    warn!("credential persistence stopped: {error}");
                    false
                }
                Err(error) => {
                    warn!("credential writer could not complete: {error}");
                    false
                }
            };
            (updated, persisted)
        }
        Err(error) => {
            // Keep the stored row (refresh may be a transient failure) and
            // let the connect decide with the old access token.
            warn!("OAuth token refresh failed, connecting with the stored token: {error:#}");
            (token, false)
        }
    }
}
