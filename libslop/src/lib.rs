use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn socket_path() -> PathBuf {
    runtime_dir().join("slopd/slopd.sock")
}

pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir().expect("could not determine XDG runtime dir")
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("could not determine XDG config dir")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdConfig {
    #[serde(default)]
    pub tmux: SlopdTmuxConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopdTmuxConfig {
    pub socket: Option<PathBuf>,
}

impl SlopdConfig {
    pub fn load() -> Self {
        let path = config_dir().join("slopd/config.toml");
        load_config(path)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SlopctlConfig {}

impl SlopctlConfig {
    pub fn load() -> Self {
        let path = config_dir().join("slopctl/config.toml");
        load_config(path)
    }
}

fn load_config<T: Default + for<'de> Deserialize<'de>>(path: PathBuf) -> T {
    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
            eprintln!("warning: failed to parse {}: {}", path.display(), e);
            T::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            eprintln!("warning: failed to read {}: {}", path.display(), e);
            T::default()
        }
    }
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
