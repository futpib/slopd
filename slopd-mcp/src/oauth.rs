use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::Form;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::Auth;

const CODE_TTL: Duration = Duration::from_secs(5 * 60);
pub const PUBLIC_CLIENT_ID: &str = "slopd-mcp";

#[derive(Clone)]
pub struct OAuthStore {
    inner: std::sync::Arc<Mutex<Inner>>,
}

struct Inner {
    clients: HashMap<String, Client>,
    codes: HashMap<String, AuthCode>,
    tokens: HashMap<String, AccessToken>,
    binding: String,
    file: File,
}

#[derive(Clone, Serialize, Deserialize)]
struct Client {
    redirect_uris: Vec<String>,
}

struct AuthCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    expires: Instant,
}

#[derive(Serialize, Deserialize)]
struct AccessToken {
    binding: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum OAuthEvent {
    Version { version: u32 },
    ClientRegistered { client_id: String, client: Client },
    TokenIssued { token: String, access: AccessToken },
}

impl OAuthStore {
    pub fn open(path: PathBuf, binding: String) -> std::io::Result<Self> {
        let mut clients = HashMap::new();
        clients.insert(
            PUBLIC_CLIENT_ID.into(),
            Client {
                redirect_uris: Vec::new(),
            },
        );
        let mut tokens = HashMap::new();
        let mut file = libslop::jsonl::open(&path)?;
        libslop::jsonl::replay(&path, |event: OAuthEvent| match event {
            OAuthEvent::Version { .. } => {}
            OAuthEvent::ClientRegistered { client_id, client } => {
                clients.insert(client_id, client);
            }
            OAuthEvent::TokenIssued { token, access } => {
                tokens.insert(token, access);
            }
        })?;
        if file.metadata()?.len() == 0 {
            libslop::jsonl::append(&mut file, &OAuthEvent::Version { version: 1 })?;
            file.sync_data()?;
        }
        Ok(Self {
            inner: std::sync::Arc::new(Mutex::new(Inner {
                clients,
                codes: HashMap::new(),
                tokens,
                binding,
                file,
            })),
        })
    }

    async fn register(&self, redirect_uris: Vec<String>) -> std::io::Result<String> {
        let id = format!("dcr-{}", uuid::Uuid::new_v4());
        let mut inner = self.inner.lock().await;
        let client = Client { redirect_uris };
        libslop::jsonl::append(
            &mut inner.file,
            &OAuthEvent::ClientRegistered {
                client_id: id.clone(),
                client: client.clone(),
            },
        )?;
        inner.file.sync_data()?;
        inner.clients.insert(id.clone(), client);
        Ok(id)
    }

    async fn client(&self, id: &str) -> Option<Client> {
        self.inner.lock().await.clients.get(id).cloned()
    }

    async fn put_code(&self, code: String, issued: AuthCode) {
        self.inner.lock().await.codes.insert(code, issued);
    }

    async fn take_code(&self, code: &str) -> Option<AuthCode> {
        let mut inner = self.inner.lock().await;
        let issued = inner.codes.remove(code)?;
        if issued.expires <= Instant::now() {
            None
        } else {
            Some(issued)
        }
    }

    async fn put_token(&self, token: String) -> std::io::Result<()> {
        let mut inner = self.inner.lock().await;
        let access = AccessToken {
            binding: inner.binding.clone(),
        };
        libslop::jsonl::append(
            &mut inner.file,
            &OAuthEvent::TokenIssued {
                token: token.clone(),
                access: AccessToken {
                    binding: access.binding.clone(),
                },
            },
        )?;
        inner.file.sync_data()?;
        inner.tokens.insert(token, access);
        Ok(())
    }

    pub async fn token_valid(&self, token: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .tokens
            .get(token)
            .is_some_and(|issued| issued.binding == inner.binding)
    }
}

pub fn routes(auth: Auth) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*rest}",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/oauth-authorization-server/{*rest}",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize_get).post(authorize_post))
        .route("/oauth/token", post(token))
        .with_state(auth)
}

pub fn challenge(auth: &Auth, headers: &HeaderMap, had_token: bool) -> Response {
    let metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        issuer(auth, headers)
    );
    let www = if had_token {
        format!(
            r#"Bearer realm="slopd-mcp", error="invalid_token", resource_metadata="{metadata}""#
        )
    } else {
        format!(r#"Bearer realm="slopd-mcp", resource_metadata="{metadata}""#)
    };
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, www)],
        "Unauthorized",
    )
        .into_response()
}

pub fn issuer(auth: &Auth, headers: &HeaderMap) -> String {
    if let Some(url) = auth.public_url.as_deref() {
        return url.trim_end_matches('/').to_string();
    }
    let proto = header_str(headers, "x-forwarded-proto")
        .or_else(|| header_str(headers, "x-forwarded-protocol"))
        .unwrap_or("http");
    let host = header_str(headers, "x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .unwrap_or("127.0.0.1");
    format!("{proto}://{host}")
}

fn header_str<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn resource_url(auth: &Auth, headers: &HeaderMap) -> String {
    format!("{}{}", issuer(auth, headers), auth.mcp_path)
}

async fn protected_resource(State(auth): State<Auth>, headers: HeaderMap) -> Json<Value> {
    let issuer = issuer(&auth, &headers);
    Json(json!({
        "resource": resource_url(&auth, &headers),
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp"],
        "resource_name": "slopd-mcp",
    }))
}

async fn authorization_server(State(auth): State<Auth>, headers: HeaderMap) -> Json<Value> {
    let issuer = issuer(&auth, &headers);
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"],
    }))
}

#[derive(Deserialize)]
struct RegisterBody {
    #[serde(default)]
    redirect_uris: Vec<String>,
}

async fn register(State(auth): State<Auth>, Json(body): Json<RegisterBody>) -> impl IntoResponse {
    if body.redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_client_metadata"})),
        )
            .into_response();
    }
    if !body.redirect_uris.iter().all(|uri| redirect_ok(uri)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_redirect_uri"})),
        )
            .into_response();
    }
    let client_id = match auth.oauth.register(body.redirect_uris.clone()).await {
        Ok(client_id) => client_id,
        Err(error) => return storage_error(error),
    };
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "redirect_uris": body.redirect_uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

#[derive(Clone, Deserialize)]
struct AuthorizeQuery {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
}

#[derive(Deserialize)]
struct AuthorizeForm {
    password: String,
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    response_type: String,
}

async fn authorize_get(State(auth): State<Auth>, Query(query): Query<AuthorizeQuery>) -> Response {
    match validate_authorize(&auth, &query).await {
        Ok(()) => login_page(&query, None),
        Err(error) => error,
    }
}

async fn authorize_post(State(auth): State<Auth>, Form(form): Form<AuthorizeForm>) -> Response {
    let query = AuthorizeQuery {
        response_type: form.response_type,
        client_id: form.client_id,
        redirect_uri: form.redirect_uri,
        state: form.state,
        code_challenge: form.code_challenge,
        code_challenge_method: form.code_challenge_method,
    };
    if let Err(error) = validate_authorize(&auth, &query).await {
        return error;
    }
    let Some(expected) = auth.password.as_deref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "oauth is not configured").into_response();
    };
    if !crate::constant_time_eq(expected.as_bytes(), form.password.as_bytes()) {
        return login_page(&query, Some("invalid password"));
    }
    let code = random_secret();
    auth.oauth
        .put_code(
            code.clone(),
            AuthCode {
                client_id: query.client_id,
                redirect_uri: query.redirect_uri.clone(),
                challenge: query.code_challenge,
                expires: Instant::now() + CODE_TTL,
            },
        )
        .await;
    let mut location = query.redirect_uri;
    location.push(if location.contains('?') { '&' } else { '?' });
    location.push_str("code=");
    location.push_str(&urlencode(&code));
    if !query.state.is_empty() {
        location.push_str("&state=");
        location.push_str(&urlencode(&query.state));
    }
    Redirect::to(&location).into_response()
}

async fn validate_authorize(auth: &Auth, query: &AuthorizeQuery) -> Result<(), Response> {
    if query.response_type != "code" {
        return Err((StatusCode::BAD_REQUEST, "response_type must be code").into_response());
    }
    if query.code_challenge_method != "S256" || query.code_challenge.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "PKCE S256 is required").into_response());
    }
    if query.client_id.is_empty() || query.redirect_uri.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "client_id and redirect_uri are required",
        )
            .into_response());
    }
    if !redirect_ok(&query.redirect_uri) {
        return Err((
            StatusCode::BAD_REQUEST,
            "redirect_uri must be https or loopback http",
        )
            .into_response());
    }
    let Some(client) = auth.oauth.client(&query.client_id).await else {
        return Err((StatusCode::BAD_REQUEST, "unknown client_id").into_response());
    };
    if !client.redirect_uris.is_empty()
        && !client
            .redirect_uris
            .iter()
            .any(|uri| uri == &query.redirect_uri)
    {
        return Err((StatusCode::BAD_REQUEST, "redirect_uri is not registered").into_response());
    }
    Ok(())
}

#[derive(Deserialize)]
struct TokenForm {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
}

async fn token(State(auth): State<Auth>, Form(form): Form<TokenForm>) -> Response {
    if form.grant_type != "authorization_code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unsupported_grant_type"})),
        )
            .into_response();
    }
    let Some(code) = auth.oauth.take_code(&form.code).await else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    if code.client_id != form.client_id || code.redirect_uri != form.redirect_uri {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    if pkce_s256(&form.code_verifier) != code.challenge {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    let access_token = random_secret();
    if let Err(error) = auth.oauth.put_token(access_token.clone()).await {
        return storage_error(error);
    }
    Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "scope": "mcp",
    }))
    .into_response()
}

fn login_page(query: &AuthorizeQuery, error: Option<&str>) -> Response {
    let error = error
        .map(|message| format!("<p>{message}</p>"))
        .unwrap_or_default();
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>slopd-mcp</title>\
         <form method=post>{error}\
         <input type=hidden name=response_type value=\"{}\">\
         <input type=hidden name=client_id value=\"{}\">\
         <input type=hidden name=redirect_uri value=\"{}\">\
         <input type=hidden name=state value=\"{}\">\
         <input type=hidden name=code_challenge value=\"{}\">\
         <input type=hidden name=code_challenge_method value=\"{}\">\
         <label>Password <input type=password name=password required autofocus></label>\
         <button type=submit>Allow</button></form>",
        esc(&query.response_type),
        esc(&query.client_id),
        esc(&query.redirect_uri),
        esc(&query.state),
        esc(&query.code_challenge),
        esc(&query.code_challenge_method),
    ))
    .into_response()
}

fn redirect_ok(uri: &str) -> bool {
    if let Some(rest) = uri.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
        let host = host.rsplit_once(':').map(|(host, _)| host).unwrap_or(host);
        return matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    }
    uri.starts_with("https://")
}

fn pkce_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()
}

pub(crate) fn token_binding(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn storage_error(error: std::io::Error) -> Response {
    tracing::error!(%error, "failed to persist OAuth state");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "server_error"})),
    )
        .into_response()
}

fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{OAuthStore, pkce_s256, redirect_ok};

    #[test]
    fn pkce_matches_rfc7636() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn redirect_allows_https_and_loopback_only() {
        assert!(redirect_ok("https://grok.com/oauth/callback"));
        assert!(redirect_ok("http://127.0.0.1:9/cb"));
        assert!(!redirect_ok("http://example.com/cb"));
    }

    #[tokio::test]
    async fn clients_and_tokens_survive_reopen() {
        let dir = libsloptest::tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.jsonl");
        let store = OAuthStore::open(path.clone(), super::token_binding("password")).unwrap();
        let client_id = store
            .register(vec!["https://grok.com/oauth/callback".into()])
            .await
            .unwrap();
        store.put_token("access-token".into()).await.unwrap();
        drop(store);

        let reopened = OAuthStore::open(path.clone(), super::token_binding("password")).unwrap();
        assert!(reopened.client(&client_id).await.is_some());
        assert!(reopened.token_valid("access-token").await);
        let rotated = OAuthStore::open(path, super::token_binding("new-password")).unwrap();
        assert!(rotated.client(&client_id).await.is_some());
        assert!(!rotated.token_valid("access-token").await);
    }
}
