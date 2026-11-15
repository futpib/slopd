#![allow(dead_code)]

use libsloptest::tempfile::{self, TempDir};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn output(command: &mut Command) -> Output {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Command timed out: {command:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut result = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.read_to_end(&mut result.stdout).unwrap();
    stderr.read_to_end(&mut result.stderr).unwrap();
    result
}

pub fn run(command: &mut Command) -> String {
    let result = output(command);
    assert!(
        result.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

pub fn wait_for<T>(label: &str, seconds: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn wait(label: &str, seconds: u64, mut check: impl FnMut() -> bool) {
    wait_for(label, seconds, || check().then_some(()));
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub struct Process(pub Child);

impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub struct Sandbox {
    pub root: TempDir,
    pub env: BTreeMap<OsString, OsString>,
    pub real_tmux: PathBuf,
}

impl Sandbox {
    pub fn new(prefix: &str, keep_runtime: bool) -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let root = if keep_runtime {
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(runtime.unwrap())
                .unwrap()
        } else {
            tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
        };
        let mut env: BTreeMap<_, _> = std::env::vars_os()
            .filter(|(key, _)| {
                let key = key.to_string_lossy();
                !["OPENCODE_", "TMUX", "SLOP"]
                    .iter()
                    .any(|prefix| key.starts_with(prefix))
                    && key != "CODEX_HOME"
                    && key != "CLAUDE_CONFIG_DIR"
            })
            .collect();
        for (key, directory) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            if key == "XDG_RUNTIME_DIR" && keep_runtime {
                continue;
            }
            let path = root.path().join(directory);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            env.insert(key.into(), path.into_os_string());
        }
        let real_tmux = std::env::split_paths(&env[&OsString::from("PATH")])
            .map(|path| path.join("tmux"))
            .find(|path| path.is_file())
            .expect("tmux required");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let guard = bin.join("tmux");
        let socket = root.path().join("tmux.sock");
        // Reject accidental access to the user's default tmux server, including
        // commands issued by the daemon and injected hooks.
        fs::write(&guard, format!("#!/bin/sh\nif [ \"$1\" != -S ] || [ \"$2\" != {} ]; then\n  echo 'Rejected tmux command outside private test socket' >&2\n  exit 99\nfi\nexec {} \"$@\"\n",
            shell_quote(socket.to_str().unwrap()), shell_quote(real_tmux.to_str().unwrap()))).unwrap();
        fs::set_permissions(&guard, fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&env[&OsString::from("PATH")])),
        )
        .unwrap();
        env.insert("PATH".into(), path);
        let sandbox = Self {
            root,
            env,
            real_tmux,
        };
        assert_eq!(
            output(sandbox.command(&guard).arg("list-sessions"))
                .status
                .code(),
            Some(99)
        );
        sandbox
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    pub fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        command
            .env_clear()
            .envs(&self.env)
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        run(self
            .command("tmux")
            .arg("-S")
            .arg(self.path("tmux.sock"))
            .args(args))
    }

    pub fn cli(&self) -> Command {
        let mut command = self.command(libsloptest::cargo_bin("slopctl"));
        command
            .arg("--config")
            .arg(self.path("slopd.toml"))
            .arg("--socket")
            .arg(self.path("slopd.sock"));
        command
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = output(
            self.command("tmux")
                .arg("-S")
                .arg(self.path("tmux.sock"))
                .arg("kill-server"),
        );
    }
}
