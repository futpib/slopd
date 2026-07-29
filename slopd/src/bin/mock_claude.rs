use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

mod mock_support;

use mock_support::{
    CLAUDE_HELP as MOCK_HELP, InputMode as NewlineMode, MockCommand, parse as parse_mock_command,
    reject_unknown_mock_option,
};

/// How long the terminal waits after an Escape for a follow-up byte before
/// deciding the Escape stood alone. An Escape immediately followed by another
/// byte is an escape sequence (e.g. `ESC` + a printable char = `Alt-<char>`),
/// whereas a lone Escape is an interrupt. Real terminals use ~25-50ms; slopd's
/// interrupt settle is deliberately far larger so a genuine interrupt never
/// falls inside this window.
const ESC_FOLLOWUP_WINDOW_MS: i32 = 50;

/// Poll stdin for a single byte, waiting up to `timeout_ms`. Returns the byte if
/// one arrives in time, else `None`. Used to model a terminal's Escape
/// disambiguation window (see [`ESC_FOLLOWUP_WINDOW_MS`]).
fn poll_byte(stdin_fd: i32, stdin: &mut std::io::Stdin, timeout_ms: i32) -> Option<u8> {
    let mut pfd = libc::pollfd {
        fd: stdin_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if ret <= 0 {
        return None;
    }
    let mut byte = [0u8; 1];
    match stdin.read(&mut byte) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(byte[0]),
    }
}

/// Result of reading input during a busy period.
enum BusyInput {
    /// One or more prompts were queued, then the busy period ended normally.
    Queued(Vec<String>),
    /// One or more prompts were queued, then the user interrupted.
    Interrupted(Vec<String>),
    /// The user interrupted before typing any prompt.
    Empty,
}

/// Read queued prompts during a busy period. Collects submitted lines until
/// either `busy_duration` elapses (returning Queued) or an interrupt byte
/// arrives (returning Interrupted with whatever was collected so far, or Empty
/// if nothing was queued yet).
///
/// Writes `queue-operation enqueue` transcript records immediately as each
/// prompt arrives, so external observers (slopd) see them in real time.
fn read_busy_input(
    stdin_fd: i32,
    stdin: &mut std::io::Stdin,
    newline_mode: &mut NewlineMode,
    newline_count: &mut u64,
    busy_duration: std::time::Duration,
    transcript_path: &PathBuf,
    session_id: &str,
) -> BusyInput {
    let deadline = std::time::Instant::now() + busy_duration;
    let mut queued: Vec<String> = Vec::new();
    let mut line_buf: Vec<u8> = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return if queued.is_empty() {
                BusyInput::Empty
            } else {
                BusyInput::Queued(queued)
            };
        }

        // Poll stdin with timeout.
        let mut pfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret <= 0 {
            return if queued.is_empty() {
                BusyInput::Empty
            } else {
                BusyInput::Queued(queued)
            };
        }

        // Data available — read one byte.
        let mut byte = [0u8; 1];
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => {
                return if queued.is_empty() {
                    BusyInput::Empty
                } else {
                    BusyInput::Interrupted(queued)
                };
            }
            Ok(_) => {}
        }
        let b = byte[0];
        match b {
            0x03 | 0x04 | 0x1b => {
                return if queued.is_empty() {
                    BusyInput::Empty
                } else {
                    BusyInput::Interrupted(queued)
                };
            }
            0x0d | 0x0a => {
                let is_submit = match newline_mode {
                    NewlineMode::AlwaysSubmit => true,
                    NewlineMode::Alternating => {
                        let n = *newline_count;
                        *newline_count += 1;
                        n % 2 == 1
                    }
                };
                if !is_submit {
                    line_buf.push(b'\n');
                    continue;
                }
                let raw = String::from_utf8_lossy(&line_buf).into_owned();
                line_buf.clear();
                let prompt = raw.trim_start_matches('\n').to_string();
                // Write enqueue immediately so slopd sees it in real time.
                write_transcript_record(
                    transcript_path,
                    &transcript_record(
                        "queue-operation",
                        session_id,
                        serde_json::json!({
                            "operation": "enqueue",
                            "content": &prompt,
                        }),
                    ),
                );
                queued.push(prompt);
            }
            0x15 => {
                // Ctrl-U kills the input line (any draft queued so far).
                line_buf.clear();
            }
            _ => {
                line_buf.push(b);
            }
        }
    }
}

/// Run all command hooks registered for the given event, passing payload as JSON on stdin.
/// Mirrors real Claude's hook execution: each command is run via `sh -c` in a non-interactive
/// shell with the JSON payload on stdin.
fn fire_hooks(
    no_hooks: bool,
    settings: &serde_json::Value,
    event: &str,
    payload: &serde_json::Value,
) {
    if no_hooks {
        return;
    }
    let Some(entries) = settings["hooks"][event].as_array() else {
        return;
    };
    for entry in entries {
        let Some(hooks) = entry["hooks"].as_array() else {
            continue;
        };
        for hook in hooks {
            if hook["type"] != "command" {
                continue;
            }
            let Some(command) = hook["command"].as_str() else {
                continue;
            };
            let mut child = Command::new("sh")
                .args(["-c", command])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| {
                    let msg = format!("mock_claude: failed to spawn hook {:?}: {}", command, e);
                    eprintln!("{}", msg);
                    println!("{}", msg);
                    std::process::exit(1);
                });
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .expect("failed to write hook payload to stdin");
            let output = child.wait_with_output().expect("failed to wait for hook");
            if !output.status.success() {
                let msg = format!(
                    "mock_claude: hook {:?} exited with {:?}\nstdout: {}\nstderr: {}",
                    command,
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                eprintln!("{}", msg);
                println!("{}", msg);
            }
        }
    }
}

/// Append a JSON record to the transcript file.
fn write_transcript_record(transcript_path: &PathBuf, record: &serde_json::Value) {
    use std::fs::OpenOptions;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript_path)
        .expect("failed to open transcript file");
    let mut line = serde_json::to_string(record).expect("failed to serialize transcript record");
    line.push('\n');
    file.write_all(line.as_bytes())
        .expect("failed to write transcript record");
}

fn transcript_record(
    record_type: &str,
    session_id: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut record = serde_json::json!({
        "type": record_type,
        "uuid": format!("mock-uuid-{}", uuid_counter()),
        "timestamp": chrono_now(),
        "sessionId": session_id,
    });
    if let (Some(base), Some(extra_obj)) = (record.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            base.insert(k.clone(), v.clone());
        }
    }
    record
}

/// Write the transcript records a client-local slash command (/model, /effort,
/// /compact, /clear) produces in real Claude: a `user` record carrying the
/// `<command-name>` block, optionally followed by a `<local-command-stdout>`
/// `user` record. Crucially, NO hooks fire (no UserPromptSubmit/Stop) — this is
/// the real-Claude behaviour slopd must detect via the transcript tailer.
fn write_slash_command(
    transcript_path: &PathBuf,
    session_id: &str,
    name: &str,
    args: &str,
    stdout: Option<&str>,
) {
    let content = format!(
        "<command-name>/{n}</command-name>\n            <command-message>{n}</command-message>\n            <command-args>{a}</command-args>",
        n = name,
        a = args,
    );
    write_transcript_record(
        transcript_path,
        &transcript_record(
            "user",
            session_id,
            serde_json::json!({
                "message": { "role": "user", "content": content },
            }),
        ),
    );
    if let Some(out) = stdout {
        write_transcript_record(
            transcript_path,
            &transcript_record(
                "user",
                session_id,
                serde_json::json!({
                    "message": { "role": "user", "content": format!("<local-command-stdout>{}</local-command-stdout>", out) },
                }),
            ),
        );
    }
}

fn uuid_counter() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("1970-01-01T00:00:{:02}.000Z", d.as_secs() % 60)
}

fn hook_payload(
    event: &str,
    session_id: &str,
    cwd: &std::path::Path,
    transcript_path: &PathBuf,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "hook_event_name": event,
        "transcript_path": transcript_path,
        "cwd": cwd,
    })
}

/// Fire the Stop hook and write a `system/turn_duration` transcript record,
/// matching real Claude's end-of-turn behaviour.
fn fire_stop(
    no_hooks: bool,
    settings: &serde_json::Value,
    session_id: &str,
    cwd: &std::path::Path,
    transcript_path: &PathBuf,
) {
    fire_hooks(
        no_hooks,
        settings,
        "Stop",
        &hook_payload("Stop", session_id, cwd, transcript_path),
    );
    write_transcript_record(
        transcript_path,
        &transcript_record(
            "system",
            session_id,
            serde_json::json!({
                "subtype": "turn_duration",
                "durationMs": 0,
            }),
        ),
    );
}

/// Fire StopFailure instead of Stop, simulating a turn that failed to complete
/// (e.g., API error 500). Used to test slopd's auto-continue functionality.
fn fire_stop_failure(
    no_hooks: bool,
    settings: &serde_json::Value,
    session_id: &str,
    cwd: &std::path::Path,
    transcript_path: &PathBuf,
) {
    fire_hooks(
        no_hooks,
        settings,
        "StopFailure",
        &hook_payload("StopFailure", session_id, cwd, transcript_path),
    );
    write_transcript_record(
        transcript_path,
        &transcript_record(
            "system",
            session_id,
            serde_json::json!({
                "subtype": "turn_duration",
                "durationMs": 0,
            }),
        ),
    );
}

/// Fire UserPromptSubmit + Stop for an accepted prompt that produces no model
/// turn — the mock test-harness `::mock` commands. This is how `slopctl send`
/// of such a command is confirmed,
/// mirroring that the input was accepted. (Real client-local slash commands
/// like /model fire NO hooks and are confirmed via the transcript
/// `<command-name>` signal instead.)
fn fire_accepted_no_turn(
    no_hooks: bool,
    settings: &serde_json::Value,
    session_id: &str,
    cwd: &std::path::Path,
    transcript_path: &PathBuf,
    prompt: &str,
) {
    let mut payload = hook_payload("UserPromptSubmit", session_id, cwd, transcript_path);
    payload["prompt"] = serde_json::json!(prompt);
    fire_hooks(no_hooks, settings, "UserPromptSubmit", &payload);
    fire_stop(no_hooks, settings, session_id, cwd, transcript_path);
}

/// Session state needed by commands that fire hooks or write transcript records.
/// None in --print mode, Some in interactive mode.
struct SessionContext<'a> {
    no_hooks: bool,
    settings: &'a serde_json::Value,
    session_id: &'a str,
    cwd: &'a std::path::Path,
    transcript_path: &'a PathBuf,
}

enum PromptResult {
    /// Command handled; caller should skip further processing.
    Handled,
    /// Exit with the given code.
    Exit(i32),
}

/// Dispatch `::mock` commands that work in both --print and interactive mode.
/// When `ctx` is Some, commands that need hooks/transcript use it.
fn handle_simple_mock_command(
    command: &MockCommand<'_>,
    ctx: Option<&SessionContext>,
) -> Option<PromptResult> {
    if let MockCommand::Echo(text) = command {
        println!("{}", text);
        return Some(PromptResult::Handled);
    }
    if let MockCommand::Sleep(duration) = command {
        std::thread::sleep(*duration);
        return Some(PromptResult::Handled);
    }
    if let MockCommand::Env(key) = command {
        let val = std::env::var(key).unwrap_or_else(|_| "UNSET".to_string());
        println!("::mock env {}={}", key, val);
        return Some(PromptResult::Handled);
    }
    if matches!(command, MockCommand::Cwd) {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "UNKNOWN".to_string());
        println!("::mock cwd {}", cwd);
        return Some(PromptResult::Handled);
    }
    if let MockCommand::ProcessExit(code) = command {
        if let Some(ctx) = ctx {
            let mut payload = hook_payload(
                "UserPromptSubmit",
                ctx.session_id,
                ctx.cwd,
                ctx.transcript_path,
            );
            payload["prompt"] = serde_json::json!(format!("::mock process exit {code}"));
            fire_hooks(ctx.no_hooks, ctx.settings, "UserPromptSubmit", &payload);
            fire_stop(
                ctx.no_hooks,
                ctx.settings,
                ctx.session_id,
                ctx.cwd,
                ctx.transcript_path,
            );
        }
        return Some(PromptResult::Exit(*code));
    }
    None
}

const FLAGS: &[&str] = &[
    "--print",
    "-p",
    "--mock-session-start=skip",
    "--mock-hooks=disabled",
    "--mock-exit=after-session-start",
    "--mock-exit=immediate",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    for arg in args.iter().skip(1).filter(|arg| arg.starts_with("--mock-")) {
        match arg.as_str() {
            "--mock-session-start=skip"
            | "--mock-hooks=disabled"
            | "--mock-exit=after-session-start"
            | "--mock-exit=immediate"
            | "--mock-crash-output" => {}
            _ => reject_unknown_mock_option("mock_claude", arg),
        }
    }
    let print_mode = args.iter().any(|a| a == "--print" || a == "-p");
    let no_session_start = args.iter().any(|a| a == "--mock-session-start=skip");
    let no_hooks = args.iter().any(|a| a == "--mock-hooks=disabled");
    // Failure-injection mode: fire SessionStart then SessionEnd and exit early,
    // simulating Claude bailing right after bootstrap (see the exit block below).
    let exit_after_start = args.iter().any(|a| a == "--mock-exit=after-session-start");
    // Failure-injection mode: exit before firing ANY hook, simulating a Claude
    // binary that dies on launch (or an executable tmux can't find). The pane
    // dies with no SessionStart/SessionEnd, so slopd only learns of it via the
    // reconciler and emits PaneDestroyed — the bare "died before becoming ready"
    // path with no session-ended reason. Distinct from --mock-exit=after-session-start, which
    // fires SessionStart→SessionEnd first.
    if args.iter().any(|a| a == "--mock-exit=immediate") {
        std::process::exit(1);
    }

    // Failure-injection mode: print a diagnostic line to the terminal (as Claude
    // does for a startup error) then exit non-zero before firing ANY hook —
    // simulating Claude choking on project-local config and dying with a visible
    // error. The pane lingers (slopd sets remain-on-exit) so the reconciler can
    // capture this text and the exit code and surface them through `slopctl run`.
    //
    // The brief pause before exiting mirrors real Claude, whose Node startup takes
    // ~100ms+ before it could reach a crash — comfortably longer than slopd's
    // set-option round-trip. Exiting in microseconds (as a bare Rust binary
    // otherwise would) is unrealistic and would race slopd's remain-on-exit,
    // making the pane vanish before it can be marked to linger.
    if let Some(pos) = args.iter().position(|a| a == "--mock-crash-output") {
        let Some(msg) = args.get(pos + 1).map(String::as_str) else {
            eprintln!("mock_claude: `--mock-crash-output` requires a message");
            std::process::exit(2);
        };
        println!("{}", msg);
        std::thread::sleep(std::time::Duration::from_millis(250));
        std::process::exit(37);
    }

    if print_mode {
        // In --print mode, treat the last non-flag argument as the prompt,
        // process it, and exit immediately (no interactive loop).
        let prompt = args
            .iter()
            .skip(1)
            .rfind(|a| !FLAGS.contains(&a.as_str()))
            .cloned()
            .unwrap_or_default();
        let command = match parse_mock_command(&prompt) {
            Ok(Some(command)) => command,
            Ok(None) => return,
            Err(error) => {
                eprintln!("mock_claude: {error}");
                std::process::exit(2);
            }
        };
        match handle_simple_mock_command(&command, None) {
            Some(PromptResult::Exit(code)) => std::process::exit(code),
            Some(PromptResult::Handled) => return,
            _ => {
                eprintln!("mock_claude: command is not supported in --print mode");
                std::process::exit(2);
            }
        }
    }

    // Real Claude reads $CLAUDE_CONFIG_DIR/settings.json (default: ~/.claude/settings.json).
    let settings_path = {
        let config_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{}/.claude", home)
        });
        format!("{}/settings.json", config_dir)
    };

    let settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    // Honor `--session-id <id>` like real Claude: it pins the id of the session
    // this process actually runs as — the one the transcript file is named for.
    // slopd's fork path passes the minted fork id here. Absent → default id.
    let session_id: &str = args
        .iter()
        .position(|a| a == "--session-id")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .unwrap_or("mock-session-id-1234");
    // The `--resume <id>` target, if any (the session being resumed / forked from).
    let resume_src: Option<&str> = args
        .iter()
        .position(|a| a == "--resume")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);
    // Fork mode: `--fork-session` copies the resumed session into a NEW one
    // (`--session-id`). CRUCIAL fidelity detail (verified against real Claude
    // v2.1.x): the SessionStart hook then fires with the RESUMED SOURCE id
    // (`--resume`), NOT the new fork id — even though `transcript_path` already
    // points at the fork's new file. slopd must not trust that id (it pins the
    // minted fork id instead). A fresh run or a plain `--resume` reports its own id.
    let forking = args.iter().any(|a| a == "--fork-session");
    let session_start_sid: &str = if forking {
        resume_src.unwrap_or(session_id)
    } else {
        session_id
    };
    let cwd = std::env::current_dir().unwrap_or_default();

    // Create a transcript .jsonl file, mirroring real Claude behaviour.
    // Use CLAUDE_CONFIG_DIR-relative path like real Claude does.
    let transcript_dir = {
        let config_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{}/.claude", home)
        });
        PathBuf::from(config_dir).join("projects").join("mock")
    };
    std::fs::create_dir_all(&transcript_dir).unwrap_or_default();
    // The transcript file this process actually writes is named for the session it
    // runs as (the fork id in fork mode).
    let transcript_path = transcript_dir.join(format!("{}.jsonl", session_id));
    // The path reported in the SessionStart hook. Faithful to real Claude: while
    // forking, the hook names the SOURCE session's file (the fork's own file is not
    // written until its first turn), even though this process will write the fork's
    // file. slopd must rewrite this to the fork's file. Non-fork: identical to
    // transcript_path.
    let session_start_transcript_path = transcript_dir.join(format!("{}.jsonl", session_start_sid));

    // Put the terminal in raw mode so we receive key bytes directly (Ctrl+C = 0x03,
    // Ctrl+D = 0x04, Escape = 0x1b) rather than having the terminal driver intercept them.
    // This mirrors real Claude's interactive terminal behaviour.
    let stdin_fd = 0i32; // STDIN_FILENO
    let orig_termios = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(stdin_fd, &mut t);
        let orig = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &t);
        orig
    };

    if !no_session_start {
        // In fork mode both diverge from the running session (see above): the hook
        // carries the resumed SOURCE id and the SOURCE transcript file; slopd must
        // rewrite both to the minted fork id / file. Non-fork: identical.
        let mut payload = hook_payload(
            "SessionStart",
            session_start_sid,
            &cwd,
            &session_start_transcript_path,
        );
        payload["source"] = serde_json::json!("startup");
        payload["model"] = serde_json::json!("mock");
        fire_hooks(no_hooks, &settings, "SessionStart", &payload);
    }

    // Failure-injection mode: simulate Claude bailing right after startup, as it
    // does when given a bad `--resume` target — it writes only bootstrap
    // metadata, fires SessionStart, then exits ~1-2s later with
    // reason=prompt_input_exit. We fire SessionEnd with that reason and exit
    // non-zero so slopd observes SessionStart → SessionEnd → (pane close) →
    // PaneDestroyed, the exact sequence `slopctl run`'s readiness wait must catch.
    if exit_after_start {
        let mut payload = hook_payload("SessionEnd", session_id, &cwd, &transcript_path);
        payload["reason"] = serde_json::json!("prompt_input_exit");
        fire_hooks(no_hooks, &settings, "SessionEnd", &payload);
        unsafe {
            libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
        }
        std::process::exit(1);
    }

    // Read raw bytes from stdin, accumulating lines.
    // Mirrors real Claude terminal behaviour:
    //   - Single Esc, C-c, or C-d: interrupt (drop current work, back to prompt)
    //   - Two consecutive C-c or two consecutive C-d: exit
    //   - Two consecutive Esc: rewind mode (ignored here, not an exit)
    let mut line_buf: Vec<u8> = Vec::new();
    let mut last_interrupt: Option<u8> = None;
    let mut stdin = std::io::stdin();
    let mut byte = [0u8; 1];
    // A byte read while disambiguating an Escape that turned out not to belong to
    // the escape sequence; processed on the next iteration before reading stdin.
    let mut pending: Option<u8> = None;
    let mut newline_mode = NewlineMode::Alternating;
    let mut newline_count: u64 = 0;
    // When set (via `::mock fail always`), every subsequent submitted prompt — including
    // slopd's own injected "continue" — ends in StopFailure, simulating a
    // persistent API outage. Used to test the auto-continue retry cap.
    let mut always_fail = false;
    // When set (via `::mock fail-then-busy <duration>`), the NEXT prompt — slopd's
    // injected "continue" — runs a busy turn this long (deliberately longer than the
    // retry backoff) before finishing with a clean Stop, instead of failing. Used
    // to test that slopd does NOT fire a second "continue" into a turn that
    // outlasts the backoff delay.
    let mut fail_then_busy: Option<std::time::Duration> = None;

    loop {
        let b = match pending.take() {
            Some(p) => p,
            None => {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                byte[0]
            }
        };
        match b {
            0x03 | 0x04 => {
                if last_interrupt == Some(b) {
                    // Two consecutive C-c or two consecutive C-d: exit.
                    break;
                }
                last_interrupt = Some(b);
            }
            0x15 => {
                // Ctrl-U kills the input line. Real Claude clears the editable
                // buffer; slopd relies on this to submit a prompt verbatim rather
                // than concatenated onto a stale draft or ghosted suggestion.
                line_buf.clear();
                last_interrupt = None;
            }
            0x1b => {
                // Escape disambiguation, mirroring a real terminal. An Escape
                // immediately followed by a printable byte is `Alt-<char>`: the
                // terminal swallows that byte (it never reaches the input buffer).
                // A lone Escape — nothing arrives within the window — is an
                // interrupt and does NOT clear the input line.
                match poll_byte(stdin_fd, &mut stdin, ESC_FOLLOWUP_WINDOW_MS) {
                    Some(nb) if (0x20..=0x7e).contains(&nb) => {
                        // Esc + printable = Alt-<char>: swallow the char, leave
                        // the existing buffer untouched.
                        last_interrupt = None;
                    }
                    Some(nb) => {
                        // Lone Escape, then a distinct control key: process that
                        // key normally on the next iteration.
                        last_interrupt = Some(0x1b);
                        pending = Some(nb);
                    }
                    None => {
                        last_interrupt = Some(0x1b);
                    }
                }
            }
            0x0d | 0x0a => {
                last_interrupt = None;

                // In Alternating mode, even-numbered newlines are literal (appended
                // to the buffer like Ctrl+J) and odd-numbered newlines submit.
                let is_submit = match newline_mode {
                    NewlineMode::AlwaysSubmit => true,
                    NewlineMode::Alternating => {
                        let n = newline_count;
                        newline_count += 1;
                        n % 2 == 1
                    }
                };

                if !is_submit {
                    line_buf.push(b'\n');
                    continue;
                }

                let raw_prompt = String::from_utf8_lossy(&line_buf).into_owned();
                line_buf.clear();
                // Trim leading newlines that were inserted as literals by alternating mode.
                let prompt = raw_prompt.trim_start_matches('\n').to_string();

                // Real Claude ignores an empty submission (pressing Enter on an
                // empty prompt does nothing — no UserPromptSubmit, no turn).
                // Without this, slopd's Enter-retry loop (which spams Enter
                // until UserPromptSubmit) would submit an empty line after a
                // client-local slash command and spuriously fire the hook.
                if prompt.trim().is_empty() {
                    continue;
                }

                let ctx = SessionContext {
                    no_hooks,
                    settings: &settings,
                    session_id,
                    cwd: &cwd,
                    transcript_path: &transcript_path,
                };
                let mock_command = match parse_mock_command(&prompt) {
                    Ok(command) => command,
                    Err(error) => {
                        eprintln!("mock_claude: {error}");
                        unsafe {
                            libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
                        }
                        std::process::exit(2);
                    }
                };
                if let Some(command) = &mock_command {
                    match handle_simple_mock_command(command, Some(&ctx)) {
                        Some(PromptResult::Handled) => {
                            fire_accepted_no_turn(
                                no_hooks,
                                &settings,
                                session_id,
                                &cwd,
                                &transcript_path,
                                &prompt,
                            );
                            continue;
                        }
                        Some(PromptResult::Exit(code)) => {
                            unsafe {
                                libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
                            }
                            std::process::exit(code);
                        }
                        None => {}
                    }
                }
                if let Some(MockCommand::InputMode(mode)) = mock_command {
                    newline_mode = mode;
                    if mode == NewlineMode::Alternating {
                        newline_count = 0;
                    }
                    fire_accepted_no_turn(
                        no_hooks,
                        &settings,
                        session_id,
                        &cwd,
                        &transcript_path,
                        &prompt,
                    );
                    continue;
                }
                if matches!(mock_command, Some(MockCommand::Help)) {
                    println!("{MOCK_HELP}");
                    fire_accepted_no_turn(
                        no_hooks,
                        &settings,
                        session_id,
                        &cwd,
                        &transcript_path,
                        &prompt,
                    );
                    continue;
                }

                // Client-local slash commands: write the transcript records
                // real Claude writes, fire NO hooks (no UserPromptSubmit/Stop).
                // slopd must detect these via the transcript tailer.
                if let Some(id) = prompt.strip_prefix("/model ") {
                    let id = id.trim();
                    write_slash_command(
                        &transcript_path,
                        session_id,
                        "model",
                        id,
                        Some(&format!("Set model to {}", id)),
                    );
                    continue;
                }
                if let Some(level) = prompt.strip_prefix("/effort ") {
                    let level = level.trim();
                    write_slash_command(
                        &transcript_path,
                        session_id,
                        "effort",
                        level,
                        Some(&format!("Set effort level to {}: mock", level)),
                    );
                    continue;
                }
                if prompt.trim() == "/compact" {
                    write_slash_command(
                        &transcript_path,
                        session_id,
                        "compact",
                        "",
                        Some("Compacted."),
                    );
                    continue;
                }
                if prompt.trim() == "/clear" {
                    write_slash_command(&transcript_path, session_id, "clear", "", None);
                    continue;
                }

                if let Some(command) = mock_command {
                    match command {
                        MockCommand::Permission(Some(duration)) => {
                            // Handled below.
                            let _ = duration;
                        }
                        MockCommand::Permission(None) => {
                            eprintln!("mock_claude: `::mock permission` requires a duration");
                            unsafe {
                                libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
                            }
                            std::process::exit(2);
                        }
                        MockCommand::Busy(_)
                        | MockCommand::Hook(_)
                        | MockCommand::FailOnce
                        | MockCommand::FailAlways
                        | MockCommand::FailThenBusy(_)
                        | MockCommand::TransportDisconnect
                        | MockCommand::TransportStallHooks
                        | MockCommand::SpawnPane => {}
                        other => {
                            eprintln!("mock_claude: unsupported command {other:?}");
                            unsafe {
                                libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
                            }
                            std::process::exit(2);
                        }
                    }
                }

                if let Some(MockCommand::Permission(Some(duration))) = mock_command {
                    // Simulate Claude processing a tool use then awaiting permission.
                    // Like `::mock busy`, but after the busy period fires PermissionRequest
                    // instead of finishing. When interrupted in the permission dialog,
                    // real Claude writes transcript `user` events but does NOT fire
                    // any hooks — so slopd never learns the state changed.

                    // Fire hooks for the prompt submission and tool use.
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "user",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "user", "content": &prompt },
                            }),
                        ),
                    );
                    let mut submit_payload =
                        hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    submit_payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &submit_payload);
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "assistant",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                            }),
                        ),
                    );

                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PreToolUse",
                        &hook_payload("PreToolUse", session_id, &cwd, &transcript_path),
                    );

                    // Busy period (tool use running), like `::mock busy`.
                    std::thread::sleep(duration);

                    // Now the tool needs permission — fire PermissionRequest.
                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PermissionRequest",
                        &hook_payload("PermissionRequest", session_id, &cwd, &transcript_path),
                    );

                    // Block waiting for interrupt (like the real permission dialog).
                    // When interrupted, write transcript events but NO hooks, just like real Claude.
                    let mut interrupt_byte = [0u8; 1];
                    loop {
                        match stdin.read(&mut interrupt_byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        match interrupt_byte[0] {
                            0x03 | 0x04 | 0x1b => {
                                // Interrupted — write transcript user events like real Claude.
                                // First: tool_result rejection.
                                write_transcript_record(
                                    &transcript_path,
                                    &transcript_record(
                                        "user",
                                        session_id,
                                        serde_json::json!({
                                            "message": {
                                                "role": "user",
                                                "content": [{
                                                    "type": "tool_result",
                                                    "tool_use_id": format!("mock-tool-use-{}", uuid_counter()),
                                                    "content": "The user doesn't want to proceed with this tool use. The tool use was rejected.",
                                                    "is_error": true,
                                                }],
                                            },
                                        }),
                                    ),
                                );
                                // Second: interrupt message.
                                write_transcript_record(
                                    &transcript_path,
                                    &transcript_record(
                                        "user",
                                        session_id,
                                        serde_json::json!({
                                            "message": {
                                                "role": "user",
                                                "content": [{
                                                    "type": "text",
                                                    "text": "[Request interrupted by user for tool use]",
                                                }],
                                            },
                                        }),
                                    ),
                                );
                                // NO hooks fired — this is the real Claude behaviour that
                                // slopd must handle via transcript detection.
                                break;
                            }
                            _ => {
                                // Ignore other input while in permission dialog.
                            }
                        }
                    }
                    continue;
                }
                if let Some(MockCommand::Busy(duration)) = mock_command {
                    // Simulate Claude running a tool use for the requested duration.
                    // During this time the real Claude still accepts terminal input and
                    // queues it; once the tool finishes, the queued prompt is submitted.
                    // Supports multiple queued prompts, interrupts (cancel), and the
                    // corresponding queue-operation transcript records.
                    // Fire UserPromptSubmit for the mock command itself (the user submitted it),
                    // and write user + assistant transcript records like real Claude does.
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "user",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "user", "content": &prompt },
                            }),
                        ),
                    );
                    let mut busy_payload =
                        hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    busy_payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &busy_payload);
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "assistant",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                            }),
                        ),
                    );

                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PreToolUse",
                        &hook_payload("PreToolUse", session_id, &cwd, &transcript_path),
                    );

                    let busy_input = read_busy_input(
                        stdin_fd,
                        &mut stdin,
                        &mut newline_mode,
                        &mut newline_count,
                        duration,
                        &transcript_path,
                        session_id,
                    );

                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PostToolUse",
                        &hook_payload("PostToolUse", session_id, &cwd, &transcript_path),
                    );

                    match busy_input {
                        BusyInput::Empty => {
                            // Interrupted before any prompt was queued — tool finished, back to ready.
                            fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                        }
                        BusyInput::Interrupted(prompts) => {
                            // Prompts were queued then user interrupted — enqueue
                            // records were already written in read_busy_input;
                            // write remove for each (cancelled).
                            for _ in &prompts {
                                write_transcript_record(
                                    &transcript_path,
                                    &transcript_record(
                                        "queue-operation",
                                        session_id,
                                        serde_json::json!({
                                            "operation": "remove",
                                        }),
                                    ),
                                );
                            }
                            fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                        }
                        BusyInput::Queued(prompts) => {
                            // Enqueue records were already written in read_busy_input.
                            // Write dequeue for each (consumed).
                            for _ in &prompts {
                                write_transcript_record(
                                    &transcript_path,
                                    &transcript_record(
                                        "queue-operation",
                                        session_id,
                                        serde_json::json!({
                                            "operation": "dequeue",
                                        }),
                                    ),
                                );
                            }
                            // Process the last queued prompt (like real Claude — last wins).
                            let last = prompts.last().unwrap();
                            write_transcript_record(
                                &transcript_path,
                                &transcript_record(
                                    "user",
                                    session_id,
                                    serde_json::json!({
                                        "message": { "role": "user", "content": last },
                                    }),
                                ),
                            );
                            write_transcript_record(
                                &transcript_path,
                                &transcript_record(
                                    "assistant",
                                    session_id,
                                    serde_json::json!({
                                        "message": { "role": "assistant", "content": format!("mock response to: {}", last) },
                                    }),
                                ),
                            );
                            let mut payload = hook_payload(
                                "UserPromptSubmit",
                                session_id,
                                &cwd,
                                &transcript_path,
                            );
                            payload["prompt"] = serde_json::json!(last);
                            fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                            fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                        }
                    }
                    continue;
                }
                if let Some(MockCommand::Hook(event)) = mock_command {
                    fire_hooks(
                        no_hooks,
                        &settings,
                        event,
                        &hook_payload(event, session_id, &cwd, &transcript_path),
                    );
                    // Fall through to fire UserPromptSubmit so slopctl send unblocks.
                }

                // Toggle persistent-failure mode: every subsequent prompt fails.
                if matches!(mock_command, Some(MockCommand::FailAlways)) {
                    always_fail = true;
                    fire_accepted_no_turn(
                        no_hooks,
                        &settings,
                        session_id,
                        &cwd,
                        &transcript_path,
                        &prompt,
                    );
                    continue;
                }

                // Arm "fail this turn, then run a long busy turn on the next
                // prompt": the mock command itself fails (first
                // StopFailure), and the following prompt (slopd's injected
                // "continue") runs busy for the requested duration before a clean Stop.
                if let Some(MockCommand::FailThenBusy(duration)) = mock_command {
                    fail_then_busy = Some(duration);
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "user",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "user", "content": &prompt },
                            }),
                        ),
                    );
                    let mut payload =
                        hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "assistant",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "assistant", "content": "API Error: 500 Internal server error" },
                            }),
                        ),
                    );
                    fire_stop_failure(no_hooks, &settings, session_id, &cwd, &transcript_path);
                    continue;
                }

                // The prompt after `::mock fail-then-busy` (slopd's injected "continue")
                // runs a busy turn longer than the backoff, then finishes cleanly.
                if let Some(busy_duration) = fail_then_busy.take() {
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "user",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "user", "content": &prompt },
                            }),
                        ),
                    );
                    let mut payload =
                        hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PreToolUse",
                        &hook_payload("PreToolUse", session_id, &cwd, &transcript_path),
                    );
                    std::thread::sleep(busy_duration);
                    fire_hooks(
                        no_hooks,
                        &settings,
                        "PostToolUse",
                        &hook_payload("PostToolUse", session_id, &cwd, &transcript_path),
                    );
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "assistant",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                            }),
                        ),
                    );
                    fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                    continue;
                }

                // Simulate a turn that failed with StopFailure (e.g., API error 500).
                // Used to test slopd's auto-continue functionality. In always_fail
                // mode any prompt (including slopd's injected "continue") fails the
                // same way, simulating a persistent outage.
                if matches!(mock_command, Some(MockCommand::FailOnce)) || always_fail {
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "user",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "user", "content": &prompt },
                            }),
                        ),
                    );
                    let mut payload =
                        hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                    write_transcript_record(
                        &transcript_path,
                        &transcript_record(
                            "assistant",
                            session_id,
                            serde_json::json!({
                                "message": { "role": "assistant", "content": "API Error: 500 Internal server error" },
                            }),
                        ),
                    );
                    fire_stop_failure(no_hooks, &settings, session_id, &cwd, &transcript_path);
                    continue;
                }

                if matches!(mock_command, Some(MockCommand::TransportDisconnect)) {
                    break;
                }
                if matches!(mock_command, Some(MockCommand::TransportStallHooks)) {
                    let mut buf = [0u8; 256];
                    while stdin.read(&mut buf).unwrap_or(0) > 0 {}
                    break;
                }
                if matches!(mock_command, Some(MockCommand::SpawnPane)) {
                    // Spawn a child pane via slopctl run. TMUX_PANE is set automatically
                    // by tmux in our environment, so the child will have @slopd_ancestor_panes
                    // pointing at us without any manual wiring.
                    let slopctl =
                        std::env::var("SLOPCTL").unwrap_or_else(|_| "slopctl".to_string());
                    // --no-wait: keep this child-spawn fire-and-forget (the test
                    // only needs the child pane id), independent of run's new
                    // wait-for-ready default.
                    let output = Command::new(&slopctl)
                        .args(["run", "--no-wait"])
                        .stdout(Stdio::piped())
                        .spawn()
                        .and_then(|c| c.wait_with_output());
                    match output {
                        Ok(out) if out.status.success() => {
                            let child_pane =
                                String::from_utf8_lossy(&out.stdout).trim().to_string();
                            // Print child pane ID so the test can read it from tmux pane content.
                            println!("::mock spawned-pane {}", child_pane);
                        }
                        Ok(out) => {
                            eprintln!("mock_claude: slopctl run failed: {:?}", out.status);
                        }
                        Err(e) => {
                            eprintln!("mock_claude: failed to spawn slopctl run: {}", e);
                        }
                    }
                    // Fall through to fire UserPromptSubmit so slopctl send unblocks.
                }

                // Write transcript records like real Claude does.
                write_transcript_record(
                    &transcript_path,
                    &transcript_record(
                        "user",
                        session_id,
                        serde_json::json!({
                            "message": { "role": "user", "content": &prompt },
                        }),
                    ),
                );
                write_transcript_record(
                    &transcript_path,
                    &transcript_record(
                        "assistant",
                        session_id,
                        serde_json::json!({
                            "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                        }),
                    ),
                );

                let mut payload =
                    hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                payload["prompt"] = serde_json::json!(prompt);
                fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
            }
            _ => {
                last_interrupt = None;
                line_buf.push(b);
            }
        }
    }

    unsafe {
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios);
    }
}
