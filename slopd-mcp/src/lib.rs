mod handler;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::header::AUTHORIZATION;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Map, Value};
use tokio::net::TcpListener;

pub use handler::{SlopdMcp, parse_backend};

#[derive(Clone)]
pub struct ServeConfig {
    pub socket: PathBuf,
    pub allow_run: bool,
    pub token: Option<Arc<str>>,
    pub allowed_hosts: Vec<String>,
    pub path: String,
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
    let Some(header) = authorization else {
        return false;
    };
    let Some(presented) = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
    else {
        return false;
    };
    constant_time_eq(expected.as_bytes(), presented.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
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
    let mcp = StreamableHttpService::new(
        {
            let socket = config.socket.clone();
            let allow_run = config.allow_run;
            move || Ok(SlopdMcp::new(socket.clone(), allow_run))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_sse_retry(None)
            .with_allowed_hosts(config.allowed_hosts.clone()),
    );
    Router::new()
        .nest_service(&path, mcp)
        .layer(middleware::from_fn_with_state(
            Auth(config.token.clone()),
            require_bearer,
        ))
}

pub async fn serve(listener: TcpListener, config: ServeConfig) -> std::io::Result<()> {
    axum::serve(listener, router(config)).await
}

#[derive(Clone)]
struct Auth(Option<Arc<str>>);

async fn require_bearer(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    if let Some(expected) = auth.0.as_deref()
        && !bearer_matches(
            expected,
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        )
    {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    next.run(request).await
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
