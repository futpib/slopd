use axum::{Json, Router, extract::State, http::header, routing::post};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Clone)]
struct Model {
    root: PathBuf,
    v2: bool,
    requests: Arc<Mutex<Vec<Value>>>,
    tool_started: Arc<AtomicBool>,
}

async fn respond(
    State(model): State<Model>,
    Json(body): Json<Value>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    model.requests.lock().unwrap().push(body.clone());
    let messages = body["messages"].as_array().unwrap();
    let last_user = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .unwrap();
    let completed = messages[last_user + 1..]
        .iter()
        .any(|message| message["role"] == "tool");
    let prompt = messages[last_user].to_string();
    let tool = if model.v2 { "shell" } else { "bash" };
    let tools = body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|entry| entry["function"]["name"] == tool));
    let mut delta = json!({"role":"assistant", "content":"SLOPD_LIVE_DONE"});
    let mut reason = "stop";
    if tools && !completed && !prompt.contains("SLOPD_COLD_START") {
        let release = model.root.join(if prompt.contains("SLOPD_INTERRUPT") {
            "interrupt-tool"
        } else {
            "release-tool"
        });
        let command = format!(
            "while [ ! -f {} ]; do sleep 0.1; done; printf SLOPD_TOOL_DONE",
            super::support::shell_quote(release.to_str().unwrap())
        );
        delta = json!({"role":"assistant", "tool_calls":[{
            "index":0,"id":"call_slopd_live","type":"function",
            "function":{"name":tool,"arguments":json!({"command":command,"description":"Wait for test release","timeout":120000}).to_string()}
        }]});
        reason = "tool_calls";
        model.tool_started.store(true, Ordering::SeqCst);
    }
    let data = json!({"id":"chatcmpl-slopd-live","object":"chat.completion.chunk","created":1,"model":"test",
        "choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let mut end = data.clone();
    end["choices"] = json!([{"index":0,"delta":{},"finish_reason":reason}]);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("data: {data}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

pub struct Provider {
    runtime: tokio::runtime::Runtime,
    server: tokio::task::JoinHandle<()>,
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub tool_started: Arc<AtomicBool>,
}

impl Provider {
    pub fn new(root: PathBuf, v2: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let model = Model {
            root,
            v2,
            requests: Arc::default(),
            tool_started: Arc::default(),
        };
        let app = Router::new()
            .route("/{*path}", post(respond))
            .with_state(model.clone());
        let server = runtime.spawn(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
                .await
                .unwrap();
        });
        Self {
            runtime,
            server,
            port,
            requests: model.requests,
            tool_started: model.tool_started,
        }
    }

    pub fn api(
        &self,
        port: u16,
        token: &str,
        v2: bool,
        path: &str,
        body: Option<Value>,
        method: reqwest::Method,
    ) -> Value {
        let path = if v2 {
            if path == "/session/status" {
                "/api/session/active".to_owned()
            } else {
                format!("/api{path}")
            }
        } else {
            path.to_owned()
        };
        self.runtime.block_on(async {
            let mut request = reqwest::Client::new()
                .request(method, format!("http://127.0.0.1:{port}{path}"))
                .timeout(Duration::from_secs(5));
            if v2 {
                request = request.basic_auth("opencode", Some(token));
            }
            if let Some(body) = body {
                request = request.json(&body);
            }
            let response = request.send().await.unwrap().error_for_status().unwrap();
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                return Value::Null;
            }
            let result: Value = response.json().await.unwrap();
            if v2 { result["data"].clone() } else { result }
        })
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.server.abort();
    }
}
