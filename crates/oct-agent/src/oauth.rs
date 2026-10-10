//! OAuth 2.0 authorization-code + PKCE machinery for http-transport MCP
//! servers (MCP spec's client flow: RFC 8414 server metadata discovery with
//! an OIDC fallback, RFC 7591 dynamic client registration, S256 PKCE, RFC
//! 8707 `resource` parameter on the authorization request).
//!
//! Why hand-rolled instead of rmcp 3.4's `auth` feature: that abstraction is
//! a per-connection `AuthorizationManager` with its own credential/state
//! stores and eager discovery probing, built for reactive 401 challenges in
//! apps that own the credential lifecycle in-process. Our shape is a web
//! backend: the flow spans three independent HTTP requests (start → browser
//! round trip at the AS → callback), tokens persist in SQLite keyed by
//! server id, and the transport only ever consumes a static Authorization
//! header anyway (rmcp's `auth_header`/custom-headers seam, which we already
//! use). Adapting the manager with DB-backed stores would cost more than
//! these ~4 standard calls, and would drag `oauth2`/`url` into the oct-mcp
//! quarantine crate. So the flow lives here, plain reqwest, and oct-mcp
//! stays untouched.
//!
//! Short-lived flow state (state ↔ verifier mapping) lives in memory only
//! ([`OAuthFlowStore`]): losing it on restart merely aborts logins in
//! flight — the user clicks login again. Tokens themselves are persisted by
//! the db layer.

use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dashmap::DashMap;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

/// How long a started flow stays valid. The user needs to finish
/// authorization in the opened browser tab; anything longer than a coffee
/// break is a lost cause. Expired entries are swept lazily on insert (the
/// map holds at most a handful of entries, no background task needed).
const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

/// Includes receiving the response body: an AS that accepts TCP but stops
/// responding must not hold a shared MCP connect gate forever.
pub(crate) const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// reqwest client for the OAuth endpoint calls. Redirects are refused: a
/// token/registration POST must never be silently replayed against a
/// redirect target (the verifier and code are single-use secrets).
pub fn oauth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(OAUTH_REQUEST_TIMEOUT)
        .build()
        .context("failed to build OAuth HTTP client")
}

/// The pieces of authorization-server metadata oct needs, from RFC 8414
/// (`/.well-known/oauth-authorization-server`) or the OIDC fallback
/// (`/.well-known/openid-configuration`).
#[derive(Debug, Clone)]
pub struct AuthorizationServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

/// Client credentials from RFC 7591 dynamic registration (secret omitted for
/// public clients, which is what oct registers as — PKCE is the proof).
#[derive(Debug, Clone)]
pub struct ClientRegistration {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// One token-endpoint success (RFC 6749 §5.1).
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

/// In-flight authorization flow between `oauth/start` and the callback:
/// everything the code exchange needs, keyed by the `state` parameter.
/// Memory-only by design (see the module docs); single-use — the callback
/// removes it.
#[derive(Debug, Clone)]
pub struct PendingFlow {
    pub server_id: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub code_verifier: String,
    pub resource: String,
    /// Flow creation time (TTL anchor); callers set `Instant::now()`.
    pub(crate) created: Instant,
}

/// Process-wide map of pending flows (`state` → [`PendingFlow`]), shared
/// through AppState. Restart-unsafe by design; see module docs.
#[derive(Default)]
pub struct OAuthFlowStore {
    flows: DashMap<String, PendingFlow>,
}

impl OAuthFlowStore {
    /// Store a flow under `state`, sweeping expired entries first.
    pub fn insert(&self, state: String, flow: PendingFlow) {
        self.flows
            .retain(|_, flow| flow.created.elapsed() < FLOW_TTL);
        self.flows.insert(state, flow);
    }

    /// Take the flow for `state` out of the map (single use). `None` covers
    /// unknown, expired and already-consumed states alike — the callback
    /// treats them identically, so replaying a callback URL cannot exchange
    /// a code twice.
    pub fn take(&self, state: &str) -> Option<PendingFlow> {
        let (key, flow) = self.flows.remove(state)?;
        if flow.created.elapsed() >= FLOW_TTL {
            let _ = key;
            return None;
        }
        Some(flow)
    }

    /// Drop every pending flow of one server (a fresh login supersedes the
    /// old one; cosmetic cleanup, TTL would catch them anyway).
    pub fn drop_server(&self, server_id: &str) {
        self.flows.retain(|_, flow| flow.server_id != server_id);
    }
}

/// Random `state` (128 bits, hex). Not derived from anything guessable.
pub fn new_state() -> String {
    Uuid::new_v4().simple().to_string()
}

/// PKCE code verifier: 64 hex chars (256 bits, within RFC 7636's 43–128
/// unreserved-char window).
fn new_code_verifier() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// S256 code challenge: base64url(SHA-256(verifier)), no padding.
fn code_challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Both halves of the PKCE pair for a fresh flow.
pub fn new_pkce_pair() -> (String, String) {
    let verifier = new_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    (verifier, challenge)
}

/// Split a server URL into (origin-with-slash-joined base, path) for the
/// RFC 8414 well-known candidates. `https://host/mcp` →
/// `("https://host", "/mcp")`.
fn url_parts(server_url: &str) -> Result<(String, String)> {
    let url = url::Url::parse(server_url)
        .with_context(|| format!("invalid MCP server url `{server_url}`"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("MCP server url must be http(s): `{server_url}`"));
    }
    let origin = format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    let path = url.path().trim_end_matches('/').to_string();
    Ok((origin, path))
}

/// Discover the authorization-server metadata for one MCP server URL:
/// RFC 8414 well-known candidates first (path-insert and path-append
/// spellings), then the OIDC discovery fallback for ASs that only publish
/// `openid-configuration`. First 200 response carrying both endpoints wins;
/// non-200 and unparseable candidates fall through to the next.
pub async fn discover_authorization_server(
    client: &reqwest::Client,
    server_url: &str,
) -> Result<AuthorizationServerMetadata> {
    // Candidates are fallbacks inside one budget, not four independent
    // full waits. This also bounds a response that stalls after headers.
    tokio::time::timeout(
        OAUTH_REQUEST_TIMEOUT,
        discover_authorization_server_inner(client, server_url),
    )
    .await
    .context("OAuth authorization-server discovery timed out")?
}

async fn discover_authorization_server_inner(
    client: &reqwest::Client,
    server_url: &str,
) -> Result<AuthorizationServerMetadata> {
    let (origin, path) = url_parts(server_url)?;
    let mut candidates = vec![
        // RFC 8414 §3.1: insert well-known between host and path.
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        // Common deployment spelling (path-prefixed well-known).
        format!("{origin}{path}/.well-known/oauth-authorization-server"),
        // Root shorthand when the server sits at the origin root.
        format!("{origin}/.well-known/oauth-authorization-server"),
        // OIDC discovery fallback (same document shape for our fields).
        format!("{origin}/.well-known/openid-configuration"),
    ];
    candidates.dedup();

    let mut last_error = String::new();
    for candidate in candidates {
        let response = match client.get(&candidate).send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("{candidate}: {error}");
                continue;
            }
        };
        if !response.status().is_success() {
            last_error = format!("{candidate}: HTTP {}", response.status());
            continue;
        }
        #[derive(Deserialize)]
        struct Metadata {
            authorization_endpoint: String,
            token_endpoint: String,
            registration_endpoint: Option<String>,
        }
        match response.json::<Metadata>().await {
            Ok(metadata) => {
                return Ok(AuthorizationServerMetadata {
                    authorization_endpoint: metadata.authorization_endpoint,
                    token_endpoint: metadata.token_endpoint,
                    registration_endpoint: metadata.registration_endpoint,
                });
            }
            Err(error) => {
                last_error = format!("{candidate}: {error}");
            }
        }
    }
    Err(anyhow!(
        "no OAuth authorization-server metadata found for `{server_url}` (tried: {last_error})"
    ))
}

/// RFC 7591 dynamic client registration. oct registers as a public client
/// (no secret, `token_endpoint_auth_method: none`) — PKCE carries the proof.
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<ClientRegistration> {
    #[derive(serde::Serialize)]
    struct RegistrationRequest {
        client_name: String,
        redirect_uris: Vec<String>,
        grant_types: Vec<String>,
        token_endpoint_auth_method: String,
        response_types: Vec<String>,
    }
    #[derive(Deserialize)]
    struct RegistrationResponse {
        client_id: String,
        client_secret: Option<String>,
    }
    let response = client
        .post(registration_endpoint)
        .json(&RegistrationRequest {
            client_name: "oct".to_string(),
            redirect_uris: vec![redirect_uri.to_string()],
            grant_types: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            token_endpoint_auth_method: "none".to_string(),
            response_types: vec!["code".to_string()],
        })
        .send()
        .await
        .with_context(|| {
            format!("dynamic client registration at `{registration_endpoint}` failed")
        })?;
    let status = response.status();
    if !status.is_success() {
        // Body text (truncated) into the error: registration failures are the
        // hardest to debug blind. No secrets are involved on this call.
        let body = response.text().await.unwrap_or_default();
        let body = body.chars().take(300).collect::<String>();
        return Err(anyhow!(
            "dynamic client registration at `{registration_endpoint}` returned HTTP {status}: {body}"
        ));
    }
    let registration: RegistrationResponse = response
        .json()
        .await
        .with_context(|| "dynamic client registration response was not valid JSON")?;
    Ok(ClientRegistration {
        client_id: registration.client_id,
        client_secret: registration.client_secret,
    })
}

/// Build the authorization URL the browser gets sent to. `resource` (RFC
/// 8707) is the MCP spec's binding of the token to this exact server URL.
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    resource: &str,
) -> Result<String> {
    let mut url = url::Url::parse(authorization_endpoint)
        .with_context(|| format!("invalid authorization_endpoint `{authorization_endpoint}`"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", resource);
    Ok(url.to_string())
}

/// Start a POST against the token-style endpoint with the client identity
/// applied: `client_id` rides the form body (public client), upgraded to
/// HTTP Basic auth when a client secret exists (confidential registration).
fn authenticated_post(
    client: &reqwest::Client,
    endpoint: &str,
    form: &mut Vec<(&'static str, String)>,
    client_id: &str,
    client_secret: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = client.post(endpoint);
    if let Some(secret) = client_secret {
        use base64::Engine;
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
        request = request.header("Authorization", format!("Basic {basic}"));
    } else {
        form.push(("client_id", client_id.to_string()));
    }
    request
}

/// Exchange the authorization code (callback path). The verifier must match
/// the challenge sent in the authorization URL — the AS enforces it.
pub async fn exchange_code(
    client: &reqwest::Client,
    flow: &PendingFlow,
    code: &str,
) -> Result<TokenResponse> {
    let mut form: Vec<(&'static str, String)> = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", flow.redirect_uri.clone()),
        ("code_verifier", flow.code_verifier.clone()),
    ];
    let request = authenticated_post(
        client,
        &flow.token_endpoint,
        &mut form,
        &flow.client_id,
        flow.client_secret.as_deref(),
    );
    request_token(request, &form, "authorization code exchange").await
}

/// Refresh the access token (pre-connect path when the stored token is near
/// expiry). One attempt only — a failing refresh keeps the stored row (the
/// AS may be briefly down) and the caller proceeds to try the old token.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let mut form: Vec<(&'static str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    let request = authenticated_post(client, token_endpoint, &mut form, client_id, client_secret);
    request_token(request, &form, "token refresh").await
}

/// POST the form to the token endpoint and map failures to a useful error.
/// The error text never echoes the form (it holds the verifier/refresh
/// token); only the endpoint, HTTP status and the AS's own `error` field.
async fn request_token(
    request: reqwest::RequestBuilder,
    form: &[(&'static str, String)],
    what: &str,
) -> Result<TokenResponse> {
    let request = request.form(form);
    let response = request.send().await;
    let response = match response {
        Ok(response) => response,
        Err(error) => return Err(anyhow!("{what} request failed: {error}")),
    };
    let status = response.status();
    if !status.is_success() {
        #[derive(Deserialize)]
        struct TokenError {
            error: Option<String>,
            error_description: Option<String>,
        }
        let body = response.text().await.unwrap_or_default();
        let parsed: std::result::Result<TokenError, _> = serde_json::from_str(&body);
        let detail = match parsed {
            Ok(token_error) => format!(
                "{}{}",
                token_error.error.unwrap_or_default(),
                token_error
                    .error_description
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            ),
            Err(_) => body.chars().take(200).collect::<String>(),
        };
        return Err(anyhow!("{what} returned HTTP {status}: {detail}"));
    }
    #[derive(Deserialize)]
    struct RawToken {
        access_token: String,
        token_type: String,
        #[serde(default)]
        expires_in: Option<i64>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        scope: Option<String>,
    }
    let raw: RawToken = response
        .json()
        .await
        .with_context(|| format!("{what} response was not a valid token JSON"))?;
    Ok(TokenResponse {
        access_token: raw.access_token,
        token_type: raw.token_type,
        expires_in: raw.expires_in,
        refresh_token: raw.refresh_token,
        scope: raw.scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovery_has_one_deadline_even_with_an_unbounded_http_client() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (entered, started) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            stream.read(&mut request).await.unwrap();
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(stream);
        });
        let discovery = tokio::spawn(async move {
            discover_authorization_server(&reqwest::Client::new(), &url).await
        });
        started.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(OAUTH_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        let error = discovery.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("discovery timed out"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn http_timeout_includes_a_stalled_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await
                .unwrap();
            // Headers arrived, but the OAuth endpoint never finishes its body.
            std::future::pending::<()>().await;
            drop(stream);
        });
        let response = oauth_http_client().unwrap().get(url).send().await.unwrap();
        let body = tokio::spawn(async move { response.text().await });
        tokio::time::pause();
        tokio::time::advance(OAUTH_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        let error = body.await.unwrap().unwrap_err();
        assert!(error.is_timeout(), "{error}");
        server.abort();
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        // RFC 7636 appendix B worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // Two fresh pairs never collide and the verifier is well-formed.
        let (v1, c1) = new_pkce_pair();
        let (v2, c2) = new_pkce_pair();
        assert_eq!(v1.len(), 64);
        assert_ne!(v1, v2);
        assert_ne!(c1, c2);
        assert_eq!(code_challenge_s256(&v1), c1);
    }

    #[test]
    fn authorization_url_carries_pkce_state_and_resource() {
        let url = build_authorization_url(
            "https://as.example/authorize?foo=bar",
            "client-1",
            "http://oct.local/api/mcp/oauth/callback",
            "state-123",
            "challenge-abc",
            "https://mcp.example/mcp",
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("as.example"));
        assert_eq!(parsed.path(), "/authorize");
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(query.get("client_id").map(String::as_str), Some("client-1"));
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://oct.local/api/mcp/oauth/callback")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("state-123"));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some("challenge-abc")
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("resource").map(String::as_str),
            Some("https://mcp.example/mcp")
        );
    }

    #[test]
    fn flow_store_take_is_single_use_and_ttl_bound() {
        let store = OAuthFlowStore::default();
        let flow = |server_id: &str| PendingFlow {
            server_id: server_id.to_string(),
            token_endpoint: "https://as/token".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            redirect_uri: "http://oct/api/mcp/oauth/callback".to_string(),
            code_verifier: "v".to_string(),
            resource: "https://mcp".to_string(),
            created: Instant::now(),
        };
        store.insert("state-1".to_string(), flow("srv"));
        assert!(store.take("state-1").is_some(), "first take succeeds");
        assert!(
            store.take("state-1").is_none(),
            "second take (replay) is rejected"
        );
        assert!(store.take("unknown").is_none());

        // TTL: an aged-out flow is refused even though the entry exists.
        store.insert(
            "state-2".to_string(),
            PendingFlow {
                created: Instant::now() - FLOW_TTL - Duration::from_secs(1),
                ..flow("srv")
            },
        );
        assert!(store.take("state-2").is_none(), "expired flow refused");

        // drop_server removes only that server's flows.
        store.insert("state-3".to_string(), flow("a"));
        store.insert("state-4".to_string(), flow("b"));
        store.drop_server("a");
        assert!(store.take("state-3").is_none());
        assert!(store.take("state-4").is_some());
    }

    #[test]
    fn url_parts_splits_origin_and_path() {
        assert_eq!(
            url_parts("https://example.com/mcp").unwrap(),
            ("https://example.com".to_string(), "/mcp".to_string())
        );
        assert_eq!(
            url_parts("http://127.0.0.1:9910/mcp/").unwrap(),
            ("http://127.0.0.1:9910".to_string(), "/mcp".to_string())
        );
        assert_eq!(
            url_parts("https://example.com").unwrap(),
            ("https://example.com".to_string(), "".to_string())
        );
        assert!(url_parts("ftp://example.com/mcp").is_err());
        assert!(url_parts("not a url").is_err());
    }
}
