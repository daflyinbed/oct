//! MCP OAuth e2e: the full authorization-code + PKCE chain over real HTTP —
//! mock AS (see `support/oauth_as.rs`) + mock MCP http server (whose RFC 8414
//! well-known points at the AS) + the real backend (discovery → DCR →
//! authorize redirect → callback code exchange → token row → bearer on
//! connect), plus the refresh path, the failure branches, and the migration
//! rollback check. No real network, no real model.

mod support;

use std::time::Duration;

use serde_json::Value;
use sqlx::SqlitePool;
use support::TestApp;
use support::oauth_as::OAuthAsMock;

/// Drive one complete login: create the http row, POST oauth/start, follow
/// the authorization URL (the mock AS 302s back to the backend's callback),
/// and return (server id, authorization_url, final callback response).
async fn login(
    app: &TestApp,
    as_mock: &OAuthAsMock,
    mcp_url: &str,
    expected_first_access: &str,
) -> (String, String, reqwest::Response) {
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "remote-oauth",
                "transport": "http",
                "url": mcp_url,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201, "http row creation should succeed");
    let server: Value = resp.json().await.unwrap();
    let id = server["id"].as_str().unwrap().to_string();
    assert_eq!(
        server["oauth"],
        serde_json::json!({"has_token": false, "has_refresh_token": false, "expires_at": null}),
        "fresh rows carry the absent OAuth summary"
    );

    let (id, authorization_url, response) = login_existing(app, as_mock, &id).await;
    // The MCP mock must see the issued access token as its bearer.
    assert!(
        expected_first_access.starts_with("oauth-access-"),
        "caller must pass the AS-issued token shape"
    );
    (id, authorization_url, response)
}

/// [`login`] for a row that already exists (tests that create their row
/// differently reuse this).
async fn login_existing(
    app: &TestApp,
    as_mock: &OAuthAsMock,
    id: &str,
) -> (String, String, reqwest::Response) {
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{id}/oauth/start"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200, "oauth/start should succeed");
    let body: Value = resp.json().await.unwrap();
    let authorization_url = body["authorization_url"]
        .as_str()
        .expect("authorization_url in body")
        .to_string();
    assert!(
        authorization_url.starts_with(&as_mock.base_url),
        "authorization URL must point at the AS: {authorization_url}"
    );

    // The "browser": follow the AS redirect back into the backend callback.
    // reqwest's default client follows the 302; the final response is the
    // callback page.
    let response = reqwest::Client::new()
        .get(&authorization_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("authorize round trip");
    (id.to_string(), authorization_url, response)
}

/// POST the project's ensure endpoint and return the effective list entry
/// for `id` (panics when absent).
async fn ensure_entry(app: &TestApp, project_id: &str, id: &str) -> Value {
    let resp = app
        .post_json(
            &format!("/api/projects/{project_id}/mcp/ensure"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let effective: Vec<Value> = resp.json().await.unwrap();
    effective
        .into_iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("server {id} missing from the effective set"))
}

/// Age the stored token row out (drives the pre-connect refresh): set
/// expires_at into the past directly on the app's own database.
async fn expire_stored_token(pool: &SqlitePool, id: &str) {
    sqlx::query(
        "UPDATE mcp_server_tokens SET expires_at = '2000-01-01 00:00:00' WHERE server_id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("age the token row");
}

/// The full happy chain: DCR → authorize (PKCE S256 + state + resource +
/// redirect_uri asserted) → callback → token row (no token bodies in API
/// responses) → ensure connects with the issued bearer.
#[tokio::test]
async fn oauth_login_full_chain_to_connected_bearer() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;

    let (id, _authorization_url, response) =
        login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;

    // The callback answered the browser with the success page.
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        Some("text/html; charset=utf-8".to_string())
    );
    let page = response.text().await.unwrap();
    assert!(page.contains("授权成功"), "success page, got: {page}");

    // DCR happened exactly once, as a public client for our callback.
    let registrations = as_mock.registrations();
    assert_eq!(registrations.len(), 1, "one dynamic registration");
    assert_eq!(registrations[0]["client_name"], "oct");
    assert_eq!(registrations[0]["token_endpoint_auth_method"], "none");
    assert_eq!(
        registrations[0]["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    let redirect_uri = registrations[0]["redirect_uris"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        redirect_uri.starts_with("http://127.0.0.1:")
            && redirect_uri.ends_with("/api/mcp/oauth/callback"),
        "redirect_uri derived from the start request's Host: {redirect_uri}"
    );

    // The authorize request carried the full MCP-spec shape.
    let authorizes = as_mock.authorize_params();
    assert_eq!(authorizes.len(), 1);
    let params = &authorizes[0];
    assert_eq!(
        params.get("response_type").map(String::as_str),
        Some("code")
    );
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(
        params.get("code_challenge").is_some_and(|c| c.len() >= 43),
        "S256 challenges are 43+ base64url chars"
    );
    assert_eq!(
        params.get("resource").map(String::as_str),
        Some(mcp_mock.url.as_str())
    );
    assert_eq!(
        params.get("redirect_uri").map(String::as_str),
        Some(redirect_uri.as_str())
    );
    assert!(params.get("state").is_some_and(|s| !s.is_empty()));

    // The token exchange used the matching verifier (the AS enforces the
    // real S256 math — its 200 IS the proof) and the registered client.
    let token_requests = as_mock.token_requests();
    assert_eq!(token_requests.len(), 1);
    assert_eq!(token_requests[0].grant_type, "authorization_code");
    assert_eq!(
        token_requests[0].form.get("client_id").map(String::as_str),
        Some("oct-dcr-client-1")
    );
    assert_eq!(
        token_requests[0]
            .form
            .get("redirect_uri")
            .map(String::as_str),
        Some(redirect_uri.as_str())
    );
    assert!(token_requests[0].form.contains_key("code_verifier"));
    assert!(
        token_requests[0].authorization.is_none(),
        "public client authenticates via body, not Basic"
    );

    // Detail carries the OAuth summary and never a token body.
    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let detail: Value = resp.json().await.unwrap();
    assert_eq!(detail["oauth"]["has_token"], true);
    assert_eq!(detail["oauth"]["has_refresh_token"], true);
    assert!(detail["oauth"]["expires_at"].is_string());
    let raw = detail.to_string();
    assert!(
        !raw.contains("oauth-access-") && !raw.contains("oauth-refresh-"),
        "token bodies must never ride an API response: {raw}"
    );

    // ensure connects through the stored bearer: the MCP mock saw the
    // AS-issued token on every request.
    let entry = ensure_entry(&app, &project_id, &id).await;
    assert_eq!(entry["status"], "connected");
    assert_eq!(entry["tool_count"], 1);
    let requests = mcp_mock.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer oauth-access-1"),
            "request `{}` missed the OAuth bearer",
            request.method
        );
    }

    // A re-login reuses the registered client: second start registers
    // nothing new (the flow state is fresh, the DB row carries client_id).
    let (id, _url, response) = login_existing(&app, &as_mock, &id).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        as_mock.registrations().len(),
        1,
        "stored client registration is reused, not re-registered"
    );
    let _ = id;
}

/// Replaying the callback URL must fail: `state` is single-use, and the mock
/// AS sees exactly ONE code exchange.
#[tokio::test]
async fn oauth_callback_state_replay_is_rejected() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;

    let (_id, _authorization_url, response) =
        login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);

    // The callback URL the AS redirected to (the final URL after the
    // redirect chain) — hit it again with redirects NOT followed.
    let callback_url = response.url().to_string();
    assert!(callback_url.contains("/api/mcp/oauth/callback"));
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let replay = no_redirect.get(&callback_url).send().await.unwrap();
    assert_eq!(replay.status(), 400, "replayed state must be rejected");
    let page = replay.text().await.unwrap();
    assert!(page.contains("授权失败"), "failure page, got: {page}");

    // And the AS never exchanged a second code.
    let token_requests = as_mock.token_requests();
    assert_eq!(
        token_requests
            .iter()
            .filter(|request| request.grant_type == "authorization_code")
            .count(),
        1,
        "exactly one code exchange despite the replay"
    );
    let _ = project_id;
}

/// An expired stored token refreshes pre-connect (one refresh grant,
/// rotated tokens persisted) and the connection uses the fresh bearer.
#[tokio::test]
async fn oauth_expired_token_refreshes_an_existing_connection() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;
    let (id, _url, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        ensure_entry(&app, &project_id, &id).await["status"],
        "connected"
    );
    let before_refresh = mcp_mock.requests().len();

    // Age the token and let the next connect see a fresh bearer: the mock
    // now demands the SECOND access token (the one a refresh issues).
    expire_stored_token(&app.pool, &id).await;
    mcp_mock.set_expected_token("Bearer oauth-access-2");

    let entry = ensure_entry(&app, &project_id, &id).await;
    assert_eq!(entry["status"], "connected", "refreshed token connects");
    for request in &mcp_mock.requests()[before_refresh..] {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer oauth-access-2"),
            "requests after the refresh must carry the new bearer"
        );
    }

    // The refresh grant hit the AS exactly once, with the stored refresh
    // token and the registered client.
    let token_requests = as_mock.token_requests();
    let refreshes: Vec<_> = token_requests
        .iter()
        .filter(|request| request.grant_type == "refresh_token")
        .collect();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        refreshes[0].form.get("refresh_token").map(String::as_str),
        Some("oauth-refresh-1")
    );
    assert_eq!(
        refreshes[0].form.get("client_id").map(String::as_str),
        Some("oct-dcr-client-1")
    );

    // The DB row now holds the ROTATED pair.
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT access_token, refresh_token FROM mcp_server_tokens WHERE server_id = ?",
    )
    .bind(&id)
    .fetch_one(&app.pool)
    .await
    .expect("token row still present");
    assert_eq!(row.0, "oauth-access-2");
    assert_eq!(row.1.as_deref(), Some("oauth-refresh-2"));

    // The saved fresh expiry prevents a reconnect/refresh on every turn.
    let request_count = mcp_mock.requests().len();
    assert_eq!(
        ensure_entry(&app, &project_id, &id).await["status"],
        "connected"
    );
    assert_eq!(mcp_mock.requests().len(), request_count);
    assert_eq!(as_mock.token_requests().len(), token_requests.len());
}

#[tokio::test]
async fn oauth_refresh_preserves_existing_call_and_retires_its_transport_after_release() {
    assert_refresh_with_borrowed_transport(false).await;
}

#[tokio::test]
async fn oauth_logout_closes_retired_transports_still_borrowed_by_an_existing_run() {
    assert_refresh_with_borrowed_transport(true).await;
}

async fn assert_refresh_with_borrowed_transport(logout: bool) {
    use oct_agent::events::EventHub;
    use oct_agent::tool_contract::ToolContext;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project = app.create_project().await;
    let (id, _, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        ensure_entry(&app, &project, &id).await["status"],
        "connected"
    );

    let old_tools = Arc::new(app.mcp.tools_for_run(Some(&project)));
    let (entered, release) = mcp_mock.gate_next_tool_call();
    let tools = old_tools.clone();
    let call = tokio::spawn(async move {
        let context = ToolContext::new(
            "existing-call",
            Arc::new(EventHub::new()),
            CancellationToken::new(),
        );
        tools[0]
            .execute(serde_json::json!({}), &context)
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), entered.cancelled())
        .await
        .unwrap();

    expire_stored_token(&app.pool, &id).await;
    mcp_mock.set_expected_token("Bearer oauth-access-2");
    assert_eq!(
        ensure_entry(&app, &project, &id).await["status"],
        "connected"
    );
    let deleted = mcp_mock.session_deleted();
    assert!(
        !deleted.is_cancelled(),
        "refresh must not close a borrowed transport"
    );
    let context = ToolContext::new(
        "new-call",
        Arc::new(EventHub::new()),
        CancellationToken::new(),
    );
    let fresh_tools = app.mcp.tools_for_run(Some(&project));
    assert!(
        !fresh_tools[0]
            .execute(serde_json::json!({}), &context)
            .await
            .unwrap()
            .is_error
    );
    assert_eq!(
        mcp_mock.requests().last().unwrap().authorization.as_deref(),
        Some("Bearer oauth-access-2")
    );

    if logout {
        assert_eq!(
            app.http_delete(&format!("/api/mcp/servers/{id}/oauth"))
                .await
                .status(),
            204
        );
    }
    release.cancel();
    let output = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output.is_error, logout,
        "unexpected sibling call result: {}",
        output.content
    );
    if !logout {
        assert!(
            !deleted.is_cancelled(),
            "the original run still owns its tools"
        );
    }
    drop(old_tools);
    tokio::time::timeout(Duration::from_secs(5), deleted.cancelled())
        .await
        .expect("the retired transport must close after its last run releases it");
    assert_eq!(
        mcp_mock
            .requests()
            .iter()
            .filter(|request| request.method == "tools/call")
            .count(),
        2,
        "neither call may be replayed"
    );
}

#[tokio::test]
async fn oauth_rejection_retries_refresh_without_replaying_tools() {
    use oct_agent::events::EventHub;
    use oct_agent::tool_contract::ToolContext;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project = app.create_project().await;
    let (id, _, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        ensure_entry(&app, &project, &id).await["status"],
        "connected"
    );
    // Revoke BEFORE expires_at. Recovery must force refresh even though
    // the stored credential's advertised expiry is still in the future.
    mcp_mock.set_expected_token("Bearer oauth-access-2");
    let context = ToolContext::new(
        "rejected-call",
        Arc::new(EventHub::new()),
        CancellationToken::new(),
    );
    let tools = app.mcp.tools_for_run(Some(&project));
    let output = tools[0]
        .execute(serde_json::json!({}), &context)
        .await
        .unwrap();
    assert!(output.is_error);
    assert!(matches!(
        app.mcp.status_of(&id),
        Some(oct_agent::mcp::ServerStatus::Failed { .. })
    ));
    assert_eq!(
        mcp_mock
            .requests()
            .iter()
            .filter(|request| request.method == "tools/call")
            .count(),
        1
    );

    as_mock.set_fail_refresh(true);
    mcp_mock.set_post_failure(Some(axum::http::StatusCode::SERVICE_UNAVAILABLE));
    assert_eq!(ensure_entry(&app, &project, &id).await["status"], "failed");
    // A non-auth handshake error must not erase the original rejection.
    mcp_mock.set_post_failure(None);
    assert_eq!(ensure_entry(&app, &project, &id).await["status"], "failed");
    assert_eq!(
        mcp_mock
            .requests()
            .iter()
            .filter(|request| request.method == "tools/call")
            .count(),
        1
    );
    as_mock.set_fail_refresh(false);
    assert_eq!(
        ensure_entry(&app, &project, &id).await["status"],
        "connected"
    );
    // Reassembly performs discovery/handshake only, never the failed call.
    assert_eq!(
        mcp_mock
            .requests()
            .iter()
            .filter(|request| request.method == "tools/call")
            .count(),
        1
    );
    assert_eq!(
        as_mock
            .token_requests()
            .iter()
            .filter(|request| request.grant_type == "refresh_token")
            .count(),
        3
    );
    let tools = app.mcp.tools_for_run(Some(&project));
    assert!(
        !tools[0]
            .execute(serde_json::json!({}), &context)
            .await
            .unwrap()
            .is_error
    );
}

#[tokio::test]
async fn cancelled_refresh_waiter_preserves_shared_rotation_and_connection() {
    use tokio_util::sync::CancellationToken;

    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project = app.create_project().await;
    let (id, _, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        ensure_entry(&app, &project, &id).await["status"],
        "connected"
    );
    expire_stored_token(&app.pool, &id).await;
    mcp_mock.set_expected_token("Bearer oauth-access-2");
    let (entered, release) = as_mock.gate_next_refresh();
    let cancel = CancellationToken::new();
    let waiter = {
        let manager = app.mcp.clone();
        let pool = app.pool.clone();
        let project = project.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            manager
                .ensure_connections_cancelled(&pool, Some(&project), &cancel)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), entered.cancelled())
        .await
        .unwrap();
    cancel.cancel();
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
    );
    release.cancel();
    let entry = ensure_entry(&app, &project, &id).await;
    assert_eq!(entry["status"], "connected");
    let token = oct_agent::db::mcp_tokens::get_mcp_server_token(&app.pool, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(token.access_token, "oauth-access-2");
    assert_eq!(token.refresh_token.as_deref(), Some("oauth-refresh-2"));
    assert_eq!(
        as_mock
            .token_requests()
            .iter()
            .filter(|request| request.grant_type == "refresh_token")
            .count(),
        1
    );
}

/// Server deletion retires an in-flight refresh writer.
#[tokio::test]
async fn oauth_server_deleted_during_refresh_does_not_block_reconciliation() {
    remove_credentials_during_refresh(true).await;
}

#[tokio::test]
async fn oauth_logout_during_refresh_does_not_restore_credentials() {
    remove_credentials_during_refresh(false).await;
}

async fn remove_credentials_during_refresh(delete_server: bool) {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;
    let (id, _, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);
    expire_stored_token(&app.pool, &id).await;
    app.mcp.invalidate(&id).await;

    let (entered, release) = as_mock.gate_next_refresh();
    let manager = app.mcp.clone();
    let pool = app.pool.clone();
    let reconciliation = tokio::spawn(async move {
        manager.ensure_connections(&pool, Some(&project_id)).await;
    });
    tokio::time::timeout(Duration::from_secs(10), entered.cancelled())
        .await
        .expect("refresh reached the authorization server");
    let path = if delete_server {
        format!("/api/mcp/servers/{id}")
    } else {
        format!("/api/mcp/servers/{id}/oauth")
    };
    assert_eq!(app.http_delete(&path).await.status(), 204);
    release.cancel();

    tokio::time::timeout(Duration::from_secs(10), reconciliation)
        .await
        .expect("removed credentials must stop persistence retries")
        .unwrap();
    assert!(
        oct_agent::db::mcp_tokens::get_mcp_server_token(&app.pool, &id)
            .await
            .unwrap()
            .is_none(),
        "the late refresh must not restore removed credentials"
    );
    assert!(app.mcp.status_of(&id).is_none());
}

/// A failing refresh keeps the stored row (transient AS trouble) and the
/// connect proceeds with the old token; when that token is no longer
/// accepted either, the server lands in `failed` with the re-login hint.
#[tokio::test]
async fn oauth_failed_refresh_keeps_token_and_fails_with_hint() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;
    let (id, _url, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);

    // Refresh fails (AS trouble) AND the MCP server stopped accepting the
    // old access token: the connect must fail with the re-login hint, and
    // the stored row must survive (refresh failure never deletes tokens).
    expire_stored_token(&app.pool, &id).await;
    as_mock.set_fail_refresh(true);
    mcp_mock.set_expected_token("Bearer something-else");
    app.mcp.invalidate(&id).await;

    let entry = ensure_entry(&app, &project_id, &id).await;
    assert_eq!(entry["status"], "failed");
    let error = entry["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("re-run OAuth login"),
        "error should tell the user to re-login: {error}"
    );

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mcp_server_tokens WHERE server_id = ?")
            .bind(&id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count.0, 1, "failed refresh keeps the stored token");

    // Recovery: the AS comes back and the old refresh token still works.
    as_mock.set_fail_refresh(false);
    mcp_mock.set_expected_token("Bearer oauth-access-2");
    app.mcp.invalidate(&id).await;
    let entry = ensure_entry(&app, &project_id, &id).await;
    assert_eq!(entry["status"], "connected");
}

/// OAuth logout: DELETE clears the row, the summary goes absent, and the
/// next connect carries no bearer (the mock 401s → failed).
#[tokio::test]
async fn oauth_delete_clears_credentials() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let project_id = app.create_project().await;
    let (id, _url, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 200);

    let resp = app
        .http_delete(&format!("/api/mcp/servers/{id}/oauth"))
        .await;
    assert_eq!(resp.status(), 204);

    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let detail: Value = resp.json().await.unwrap();
    assert_eq!(
        detail["oauth"],
        serde_json::json!({"has_token": false, "has_refresh_token": false, "expires_at": null})
    );

    // Token row gone; the summary endpoint agrees.
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mcp_server_tokens WHERE server_id = ?")
            .bind(&id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0);

    let entry = ensure_entry(&app, &project_id, &id).await;
    assert_eq!(
        entry["status"], "failed",
        "no bearer after logout → the mock 401s"
    );
    let _ = project_id;
}

/// The AS error branch: an authorize rejection redirects back with
/// `error=access_denied`, the callback renders the failure page, and no
/// token lands in the DB.
#[tokio::test]
async fn oauth_authorize_rejection_lands_on_failure_page() {
    let app = TestApp::new().await;
    let as_mock = OAuthAsMock::spawn().await;
    as_mock.set_reject_authorize(true);
    let mcp_mock = support::mcp_http::McpHttpMock::spawn_with_oauth(
        "Bearer oauth-access-1",
        &as_mock.base_url,
    )
    .await;
    let _project_id = app.create_project().await;

    let (id, _url, response) = login(&app, &as_mock, &mcp_mock.url, "oauth-access-1").await;
    assert_eq!(response.status(), 400, "error= branch answers 400");
    let page = response.text().await.unwrap();
    assert!(page.contains("授权失败"), "failure page, got: {page}");
    assert!(
        page.contains("access_denied"),
        "the AS error code is shown: {page}"
    );

    let resp = app.get_json(&format!("/api/mcp/servers/{id}")).await;
    let detail: Value = resp.json().await.unwrap();
    assert_eq!(detail["oauth"]["has_token"], false);
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mcp_server_tokens WHERE server_id = ?")
            .bind(&id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count.0, 0);
    assert!(
        as_mock.token_requests().is_empty(),
        "no code exchange happened"
    );
}

/// oauth/start guards: unknown server 404s, stdio transport 400s, and a
/// server whose origin publishes no OAuth metadata 502s.
#[tokio::test]
async fn oauth_start_guards() {
    let app = TestApp::new().await;
    let _project_id = app.create_project().await;

    // Unknown server.
    let resp = app
        .post_json(
            "/api/mcp/servers/missing/oauth/start",
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 404);

    // stdio transport: OAuth is http-only.
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "local-stdio",
                "transport": "stdio",
                "command": "/nonexistent/oct-mcp-oauth-server",
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let stdio_id = server["id"].as_str().unwrap().to_string();
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{stdio_id}/oauth/start"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("http-transport"),
        "error names the transport constraint: {body}"
    );

    // No OAuth metadata at the origin: discovery fails → 502.
    let plain = support::mcp_http::McpHttpMock::spawn().await;
    let resp = app
        .post_json(
            "/api/mcp/servers",
            serde_json::json!({
                "name": "no-oauth",
                "transport": "http",
                "url": plain.url,
            }),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let server: Value = resp.json().await.unwrap();
    let http_id = server["id"].as_str().unwrap().to_string();
    let resp = app
        .post_json(
            &format!("/api/mcp/servers/{http_id}/oauth/start"),
            serde_json::json!(null),
        )
        .await;
    assert_eq!(resp.status(), 502);
}

/// The tokens migration rolls all the way down and back up: run → undo(0) →
/// run against a bare temp-file SQLite (no `query!` macros here — the
/// runtime API keeps this test independent of the offline cache).
#[tokio::test]
async fn mcp_server_tokens_migration_rolls_back_and_reapplies() {
    use sqlx::sqlite::SqlitePoolOptions;

    let dir = tempfile::tempdir().expect("temp dir");
    let db_url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("migrate.db").display()
    );
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url)
        .await
        .expect("connect bare sqlite");

    let migrator = sqlx::migrate::Migrator::new(std::path::Path::new("../../migrations"))
        .await
        .expect("load migrations");
    migrator.run(&pool).await.expect("apply all up");

    async fn table_exists(pool: &SqlitePool) -> bool {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='mcp_server_tokens'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        count == 1
    }
    assert!(table_exists(&pool).await, "table exists after up");

    migrator
        .undo(&pool, 0)
        .await
        .expect("roll all the way down");
    assert!(!table_exists(&pool).await, "table gone after down");

    migrator.run(&pool).await.expect("re-apply after rollback");
    assert!(table_exists(&pool).await, "table back after re-up");
}
