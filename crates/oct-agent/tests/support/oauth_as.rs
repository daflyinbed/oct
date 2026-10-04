//! Mock OAuth 2.0 authorization server for the e2e tests: RFC 8414 discovery
//! is served by the MCP http mock (same origin as the "protected resource"),
//! this mock plays the AS itself — dynamic registration, the authorize
//! redirect (302 back to oct's callback with a code), and the token endpoint
//! (code exchange with real PKCE S256 verification, plus rotating refresh
//! tokens). Behavior knobs (`fail_refresh`, `reject_authorize`) let tests
//! drive the failure branches; everything is recorded for assertions.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use axum::extract::Query as AxumQuery;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use base64::Engine;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

/// One token-endpoint call, as recorded for assertions.
#[derive(Debug, Clone)]
pub struct RecordedTokenRequest {
    pub grant_type: String,
    /// Decoded form body.
    pub form: HashMap<String, String>,
    /// `Authorization` header when the client used HTTP Basic auth.
    pub authorization: Option<String>,
}

#[derive(Default)]
struct AsState {
    registrations: Vec<serde_json::Value>,
    /// Query-parameter maps of every /authorize hit, in order.
    authorizes: Vec<HashMap<String, String>>,
    token_requests: Vec<RecordedTokenRequest>,
    /// Outstanding authorization codes: code → S256 challenge.
    codes: HashMap<String, String>,
    /// Valid refresh tokens (rotated: refresh consumes the old one).
    refresh_tokens: HashSet<String>,
    fail_refresh: bool,
    reject_authorize: bool,
    access_counter: u32,
    refresh_gate: Option<(CancellationToken, CancellationToken)>,
}

/// A running mock authorization server.
pub struct OAuthAsMock {
    pub base_url: String,
    shared: Arc<Mutex<AsState>>,
}

impl OAuthAsMock {
    pub async fn spawn() -> Self {
        let shared: Arc<Mutex<AsState>> = Arc::default();
        let app = axum::Router::new()
            .route("/register", axum::routing::post(register).get(not_found))
            .route("/authorize", axum::routing::get(authorize).post(not_found))
            .route("/token", axum::routing::post(token))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock AS");
        let addr = listener.local_addr().expect("mock AS addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock AS run");
        });
        Self {
            base_url: format!("http://{addr}"),
            shared,
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut AsState) -> R) -> R {
        let mut guard = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    /// RFC 7591 registration bodies the mock saw, in order.
    pub fn registrations(&self) -> Vec<serde_json::Value> {
        self.with_state(|state| state.registrations.clone())
    }

    /// Query-parameter maps of every /authorize request, in order.
    pub fn authorize_params(&self) -> Vec<HashMap<String, String>> {
        self.with_state(|state| state.authorizes.clone())
    }

    /// Every token-endpoint call, in order.
    pub fn token_requests(&self) -> Vec<RecordedTokenRequest> {
        self.with_state(|state| state.token_requests.clone())
    }

    /// Make every refresh grant fail with `invalid_grant` (until re-enabled).
    pub fn set_fail_refresh(&self, fail: bool) {
        self.with_state(|state| state.fail_refresh = fail);
    }

    /// Hold the next refresh response after rotating the credentials. The
    /// first token signals arrival; cancelling the second releases it.
    pub fn gate_next_refresh(&self) -> (CancellationToken, CancellationToken) {
        let entered = CancellationToken::new();
        let release = CancellationToken::new();
        self.with_state(|state| state.refresh_gate = Some((entered.clone(), release.clone())));
        (entered, release)
    }

    /// Make /authorize redirect back with `error=access_denied` instead of a
    /// code (drives the callback's error branch).
    pub fn set_reject_authorize(&self, reject: bool) {
        self.with_state(|state| state.reject_authorize = reject);
    }

    /// The access token the Nth grant (exchange or refresh) will/did issue.
    pub fn access_token(&self, n: u32) -> String {
        format!("oauth-access-{n}")
    }
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// RFC 7591 dynamic registration: record the body, hand out a fixed public
/// client id.
async fn register(
    State(shared): State<Arc<Mutex<AsState>>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Response {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .registrations
        .push(body.clone());
    let redirect_uris = body["redirect_uris"].clone();
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "client_id": "oct-dcr-client-1",
            "client_id_issued_at": 0,
            "client_name": "oct",
            "redirect_uris": redirect_uris,
        })),
    )
        .into_response()
}

/// Authorization endpoint: validate nothing beyond recording (the assertions
/// inspect the params), mint a single-use code bound to the S256 challenge,
/// 302 back to oct's callback. `reject_authorize` redirects with
/// `error=access_denied` instead.
async fn authorize(
    State(shared): State<Arc<Mutex<AsState>>>,
    AxumQuery(params): AxumQuery<HashMap<String, String>>,
) -> Response {
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let state_value = params.get("state").cloned().unwrap_or_default();
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.authorizes.push(params.clone());
    if state.reject_authorize {
        return redirect_with_error(&redirect_uri, &state_value, "access_denied");
    }
    let challenge = params.get("code_challenge").cloned().unwrap_or_default();
    let code = uuid::Uuid::new_v4().simple().to_string();
    state.codes.insert(code.clone(), challenge);
    let location = format!("{redirect_uri}?code={code}&state={state_value}");
    (StatusCode::FOUND, [("Location", location)]).into_response()
}

fn redirect_with_error(redirect_uri: &str, state_value: &str, error: &str) -> Response {
    let location = format!(
        "{redirect_uri}?error={error}&error_description=scripted+rejection&state={state_value}"
    );
    (StatusCode::FOUND, [("Location", location)]).into_response()
}

/// Token endpoint: authorization_code exchanges verify the PKCE verifier
/// against the stored challenge (real S256 math, so a broken client fails
/// here exactly like against a real AS); refresh grants rotate both tokens
/// and consume the old refresh token.
async fn token(
    State(shared): State<Arc<Mutex<AsState>>>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Response {
    let gate = if form
        .get("grant_type")
        .is_some_and(|grant| grant == "refresh_token")
    {
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .refresh_gate
            .take()
    } else {
        None
    };
    let response = token_response(&shared, headers, form);
    if let Some((entered, release)) = gate {
        entered.cancel();
        release.cancelled().await;
    }
    response
}

fn token_response(
    shared: &Arc<Mutex<AsState>>,
    headers: axum::http::HeaderMap,
    form: HashMap<String, String>,
) -> Response {
    let grant_type = form.get("grant_type").cloned().unwrap_or_default();
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.token_requests.push(RecordedTokenRequest {
        grant_type: grant_type.clone(),
        form: form.clone(),
        authorization,
    });
    let n = state.access_counter + 1;
    match grant_type.as_str() {
        "authorization_code" => {
            let Some(code) = form.get("code") else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_request", "missing code");
            };
            // Single use: take the challenge out; a replayed code misses.
            let Some(challenge) = state.codes.remove(code) else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown code");
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()));
            if computed != challenge {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "PKCE verification failed",
                );
            }
            state.access_counter = n;
            let refresh = format!("oauth-refresh-{n}");
            state.refresh_tokens.insert(refresh.clone());
            token_success(&format!("oauth-access-{n}"), Some(&refresh))
        }
        "refresh_token" => {
            if state.fail_refresh {
                return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "scripted failure");
            }
            let refresh_token = form.get("refresh_token").cloned().unwrap_or_default();
            // Rotating AS: the old refresh token dies with its use.
            if !state.refresh_tokens.remove(&refresh_token) {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "unknown refresh token",
                );
            }
            state.access_counter = n;
            let refresh = format!("oauth-refresh-{n}");
            state.refresh_tokens.insert(refresh.clone());
            token_success(&format!("oauth-access-{n}"), Some(&refresh))
        }
        other => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("unknown grant_type {other}"),
        ),
    }
}

fn token_success(access_token: &str, refresh_token: Option<&str>) -> Response {
    let mut body = json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "mcp",
    });
    if let Some(refresh_token) = refresh_token {
        body["refresh_token"] = json!(refresh_token);
    }
    (StatusCode::OK, axum::Json(body)).into_response()
}

fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": error, "error_description": description})),
    )
        .into_response()
}
