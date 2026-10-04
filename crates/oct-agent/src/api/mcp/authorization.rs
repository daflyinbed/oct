//! HTTP OAuth routes and browser callback rendering.
use super::*;

// --- OAuth 2.0 (authorization code + PKCE) for http-transport servers ------

/// Response body of `POST /mcp/servers/{id}/oauth/start`.
#[derive(Debug, Serialize, ToSchema)]
pub struct OAuthStartResponse {
    /// The authorization-server URL the client should open in a browser
    /// (already carrying PKCE challenge, state and the RFC 8707 `resource`
    /// binding to this server's url).
    pub authorization_url: String,
}

/// Derive this backend's OAuth callback URL from the request's Host header.
///
/// Convention: oct IS the web backend, so the AS redirects back to oct
/// itself at `/api/mcp/oauth/callback` on the SAME host the caller (the
/// frontend) reached the API through — no extra configuration for the
/// common browser-on-localhost shape. Caveat: behind a TLS-terminating
/// reverse proxy the derived scheme stays `http` (the proxy→oct hop is
/// plain), and a non-standard public port is only visible if the browser
/// sends it in Host. Honoring `X-Forwarded-Proto`/`X-Forwarded-Host` is
/// deliberately left out of the MVP (trust boundary needs a configured
/// allowlist); deploy behind a proxy that preserves Host, or front the whole
/// app on one origin.
fn derive_redirect_uri(headers: &axum::http::HeaderMap) -> Result<String, AppError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            AppError::BadRequest(
                "cannot derive the OAuth redirect URI: the request carried no Host header".into(),
            )
        })?;
    Ok(format!("http://{host}/api/mcp/oauth/callback"))
}

/// Minimal HTML-escape for the callback page (error strings are untrusted).
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The callback answers a BROWSER navigation, not the frontend's fetch layer:
/// a tiny standalone page (styles inlined, no app bootstrap) that tells the
/// user whether the login landed, so no routing/scripting machinery is
/// needed. The frontend discovers success by polling the server detail.
fn oauth_callback_page(success: bool, message: &str) -> Html<String> {
    let (title, body) = if success {
        (
            "OAuth 授权成功".to_string(),
            "授权成功，可关闭此页返回应用。".to_string(),
        )
    } else {
        (
            "OAuth 授权失败".to_string(),
            format!("授权失败：{}", escape_html(message)),
        )
    };
    Html(format!(
        "<!doctype html><html lang=\"zh\"><head><meta charset=\"utf-8\"><title>{}</title>\
         <style>body{{font-family:system-ui,sans-serif;display:flex;min-height:100vh;\
         align-items:center;justify-content:center;margin:0;background:#1c1c1e;color:#e8e8ea}}\
         div{{max-width:32em;padding:2rem;border-radius:12px;background:#2a2a2e;\
         box-shadow:0 8px 30px rgba(0,0,0,.4)}}h1{{font-size:1.1rem;margin:0 0 .5rem}}\
         p{{margin:0;color:#b8b8bd;line-height:1.6;word-break:break-word}}</style></head>\
         <body><div><h1>{}</h1><p>{}</p></div></body></html>",
        escape_html(&title),
        escape_html(&title),
        body,
    ))
}

/// Start the OAuth flow for one http-transport MCP server: discover the
/// authorization-server metadata (RFC 8414 well-known with an OIDC fallback),
/// register a client via DCR when none is stored, mint state + PKCE pair,
/// and hand back the authorization URL for the browser. Discovery /
/// registration failures surface as 502 (the upstream AS is the failing
/// party); anything else as 400/404.
#[utoipa::path(
    post,
    path = "/mcp/servers/{id}/oauth/start",
    params(("id" = String, Path, description = "MCP server ID")),
    responses(
        (status = 200, description = "Flow started; open `authorization_url` in a browser", body = OAuthStartResponse),
        (status = 400, description = "Server is not http transport, or no Host header to derive the redirect URI from"),
        (status = 404, description = "MCP server not found"),
        (status = 502, description = "Authorization-server discovery or dynamic client registration failed")
    ),
    tag = "mcp"
)]
pub async fn start_mcp_oauth(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<OAuthStartResponse>> {
    let server = db::get_mcp_server(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP server not found".into()))?;
    if server.transport != "http" {
        return Err(AppError::BadRequest(
            "OAuth login is only available for http-transport MCP servers".into(),
        ));
    }
    let url = server
        .url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| AppError::BadRequest("http transport requires a url".into()))?;

    let redirect_uri = derive_redirect_uri(&headers)?;
    let client = oauth::oauth_http_client()?;

    // Discovery: the well-known candidates live at the MCP server's origin.
    let metadata = oauth::discover_authorization_server(&client, url)
        .await
        .map_err(|error| {
            // Log WITHOUT any credential material (none is involved yet);
            // the page-facing message stays short.
            AppError::BadGateway(format!("OAuth discovery failed: {error:#}"))
        })?;

    // Client identity: a stored credential row's registration is reused
    // (re-login must not pile up DCR registrations); otherwise register now.
    let (client_id, client_secret) =
        match mcp_tokens::get_mcp_server_token(&state.pool, &id).await? {
            Some(stored) => (stored.client_id, stored.client_secret),
            None => {
                let registration_endpoint =
                    metadata.registration_endpoint.clone().ok_or_else(|| {
                        AppError::BadGateway(
                            "the authorization server advertises no dynamic registration \
                             endpoint and no client is registered for this server"
                                .into(),
                        )
                    })?;
                let registration =
                    oauth::register_client(&client, &registration_endpoint, &redirect_uri)
                        .await
                        .map_err(|error| {
                            AppError::BadGateway(format!(
                                "dynamic client registration failed: {error:#}"
                            ))
                        })?;
                (registration.client_id, registration.client_secret)
            }
        };

    let state_value = oauth::new_state();
    let (verifier, challenge) = oauth::new_pkce_pair();
    state.oauth_flows.insert(
        state_value.clone(),
        oauth::PendingFlow {
            server_id: id.clone(),
            token_endpoint: metadata.token_endpoint.clone(),
            client_id: client_id.clone(),
            client_secret,
            redirect_uri: redirect_uri.clone(),
            code_verifier: verifier,
            resource: url.to_string(),
            created: std::time::Instant::now(),
        },
    );
    let authorization_url = oauth::build_authorization_url(
        &metadata.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &state_value,
        &challenge,
        url,
    )?;
    Ok(Json(OAuthStartResponse { authorization_url }))
}

/// Query parameters of `GET /mcp/oauth/callback` (RFC 6749 §4.1.2/§4.1.2.1):
/// success carries `code`+`state`; the AS's error path carries `error`
/// (+ optional `error_description`).
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// The authorization-server redirect target: validate `state` (single use),
/// exchange the code with the stored PKCE verifier, persist the tokens, and
/// drop the server's live connection so the next reconcile picks the fresh
/// credential up. Answers with a minimal HTML page — the browser (not the
/// frontend) lands here.
#[utoipa::path(
    get,
    path = "/mcp/oauth/callback",
    params(OAuthCallbackQuery),
    responses(
        (status = 200, description = "Tokens stored; success page", body = String, content_type = "text/html"),
        (status = 400, description = "AS error response, unknown/expired/replayed state, or token exchange failed; failure page", body = String, content_type = "text/html")
    ),
    tag = "mcp"
)]
pub async fn mcp_oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<OAuthCallbackQuery>,
) -> axum::response::Response {
    if let Some(error) = query.error.as_deref() {
        let message = match query.error_description.as_deref() {
            Some(description) => format!("{error}: {description}"),
            None => error.to_string(),
        };
        return (
            StatusCode::BAD_REQUEST,
            oauth_callback_page(false, &message),
        )
            .into_response();
    }
    let (code, state_value) = match (query.code, query.state) {
        (Some(code), Some(state_value)) if !code.is_empty() && !state_value.is_empty() => {
            (code, state_value)
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                oauth_callback_page(false, "回调缺少 code 或 state 参数"),
            )
                .into_response();
        }
    };

    // Single-use: take() removes the flow, so a replayed callback URL finds
    // nothing and a second code exchange is impossible.
    let Some(flow) = state.oauth_flows.take(&state_value) else {
        return (
            StatusCode::BAD_REQUEST,
            oauth_callback_page(
                false,
                "state 无效或已过期（可能是回调被重复访问，或后端已重启）",
            ),
        )
            .into_response();
    };

    let client = match oauth::oauth_http_client() {
        Ok(client) => client,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                oauth_callback_page(false, &format!("{error:#}")),
            )
                .into_response();
        }
    };
    match oauth::exchange_code(&client, &flow, &code).await {
        Ok(token) => {
            let now = chrono::Utc::now().naive_utc();
            let expires_at = token
                .expires_in
                .filter(|seconds| *seconds > 0)
                .map(|seconds| now + chrono::Duration::seconds(seconds));
            let save = mcp_tokens::SaveTokenRequest {
                server_id: flow.server_id.clone(),
                client_id: flow.client_id.clone(),
                client_secret: flow.client_secret.clone(),
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                token_type: token.token_type,
                scope: token.scope,
                expires_at,
            };
            if let Err(error) = mcp_tokens::save_mcp_server_token(&state.pool, &save).await {
                return (
                    StatusCode::BAD_REQUEST,
                    oauth_callback_page(false, &format!("token 入库失败：{error:#}")),
                )
                    .into_response();
            }
            // The fresh credential only matters once a connection is rebuilt
            // with it: drop the current entry so the next reconcile (chat
            // turn or ensure endpoint) reconnects authenticated.
            state.mcp.invalidate(&flow.server_id).await;
            (StatusCode::OK, oauth_callback_page(true, "")).into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            oauth_callback_page(false, &format!("{error:#}")),
        )
            .into_response(),
    }
}

/// OAuth logout: drop the stored credential row (client registration
/// included — a future login re-registers) and force a reconnect without a
/// token (a headers_json bearer, if any, takes over again).
#[utoipa::path(
    delete,
    path = "/mcp/servers/{id}/oauth",
    params(("id" = String, Path, description = "MCP server ID")),
    responses(
        (status = 204, description = "Stored OAuth credential cleared"),
        (status = 404, description = "MCP server not found")
    ),
    tag = "mcp"
)]
pub async fn delete_mcp_oauth(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    db::get_mcp_server(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP server not found".into()))?;
    mcp_tokens::delete_mcp_server_token(&state.pool, &id).await?;
    state.oauth_flows.drop_server(&id);
    state.mcp.invalidate(&id).await;
    Ok(StatusCode::NO_CONTENT)
}
