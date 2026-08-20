mod handler;
mod oauth;
mod tools;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use http::header::AUTHORIZATION;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Map, Value};
use tokio::net::TcpListener;

pub use handler::{SlopdMcp, parse_backend};

#[derive(Clone)]
pub struct ServeConfig {
    pub socket: PathBuf,
    pub token: Option<Arc<str>>,
    pub allowed_hosts: Vec<String>,
    pub path: String,
    pub public_url: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Auth {
    pub password: Option<Arc<str>>,
    pub oauth: oauth::OAuthStore,
    pub public_url: Option<String>,
    pub mcp_path: String,
}

pub fn schema(value: Value) -> Arc<Map<String, Value>> {
    Arc::new(
        value
            .as_object()
            .expect("tool input schema must be a JSON object")
            .clone(),
    )
}

pub fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

pub fn default_allowed_hosts(bind: SocketAddr) -> Vec<String> {
    let mut hosts = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
        bind.ip().to_string(),
        bind.to_string(),
    ];
    hosts.sort();
    hosts.dedup();
    hosts
}

pub fn bearer_matches(expected: &str, authorization: Option<&str>) -> bool {
    presented_bearer(authorization)
        .is_some_and(|presented| constant_time_eq(expected.as_bytes(), presented.as_bytes()))
}

fn presented_bearer(authorization: Option<&str>) -> Option<&str> {
    let header = authorization?;
    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub fn router(config: ServeConfig) -> Router {
    let path = if config.path.starts_with('/') {
        config.path.clone()
    } else {
        format!("/{}", config.path)
    };
    let auth = Auth {
        password: config.token.clone(),
        oauth: oauth::OAuthStore::new(),
        public_url: config.public_url.clone(),
        mcp_path: path.clone(),
    };
    let mcp = StreamableHttpService::new(
        {
            let socket = config.socket.clone();
            move || Ok(SlopdMcp::new(socket.clone()))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_sse_retry(None)
            .with_allowed_hosts(config.allowed_hosts.clone()),
    );
    let mcp_routes = Router::new()
        .nest_service(&path, mcp)
        .layer(middleware::from_fn_with_state(auth.clone(), require_bearer));
    if config.token.is_some() {
        oauth::routes(auth).merge(mcp_routes)
    } else {
        mcp_routes
    }
}

pub async fn serve(listener: TcpListener, config: ServeConfig) -> std::io::Result<()> {
    axum::serve(listener, router(config)).await
}

async fn require_bearer(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    let Some(expected) = auth.password.as_deref() else {
        return next.run(request).await;
    };
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if let Some(presented) = presented_bearer(authorization) {
        let static_ok = constant_time_eq(expected.as_bytes(), presented.as_bytes());
        if static_ok || auth.oauth.token_valid(presented).await {
            return next.run(request).await;
        }
        return oauth::challenge(&auth, request.headers(), true);
    }
    oauth::challenge(&auth, request.headers(), false)
}

#[cfg(test)]
mod tests {
    use super::{bearer_matches, is_loopback};

    #[test]
    fn bearer_compares_the_token_only() {
        assert!(bearer_matches("secret", Some("Bearer secret")));
        assert!(!bearer_matches("secret", Some("Bearer other")));
        assert!(!bearer_matches("secret", None));
        assert!(!bearer_matches("secret", Some("secret")));
    }

    #[test]
    fn loopback_detects_localhost() {
        assert!(is_loopback("127.0.0.1:8780".parse().unwrap()));
        assert!(!is_loopback("10.77.77.2:8780".parse().unwrap()));
    }
}
