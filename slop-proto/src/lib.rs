use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn socket_path() -> PathBuf {
    runtime_dir().join("slopd/slopd.sock")
}

pub fn runtime_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(val);
    }
    dirs::runtime_dir().expect("could not determine XDG runtime dir")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub body: RequestBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestBody {
    Ping,
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseBody {
    Pong,
    Status { state: DaemonState },
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonState {
    pub uptime_secs: u64,
}
