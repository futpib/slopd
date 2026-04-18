use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
enum NewlineMode {
    /// Every newline submits the line (original behaviour).
    AlwaysSubmit,
    /// Alternating: even-numbered newlines (0, 2, …) are literal, odd (1, 3, …) submit.
    Alternating,
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
            return if queued.is_empty() { BusyInput::Empty } else { BusyInput::Queued(queued) };
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
            return if queued.is_empty() { BusyInput::Empty } else { BusyInput::Queued(queued) };
        }

        // Data available — read one byte.
        let mut byte = [0u8; 1];
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => {
                return if queued.is_empty() { BusyInput::Empty } else { BusyInput::Interrupted(queued) };
            }
            Ok(_) => {}
        }
        let b = byte[0];
        match b {
            0x03 | 0x04 | 0x1b => {
                return if queued.is_empty() { BusyInput::Empty } else { BusyInput::Interrupted(queued) };
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
                write_transcript_record(transcript_path, &transcript_record(
                    "queue-operation", session_id, serde_json::json!({
                        "operation": "enqueue",
                        "content": &prompt,
                    }),
                ));
                queued.push(prompt);
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
fn fire_hooks(no_hooks: bool, settings: &serde_json::Value, event: &str, payload: &serde_json::Value) {
    if no_hooks { return; }
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

fn transcript_record(record_type: &str, session_id: &str, extra: serde_json::Value) -> serde_json::Value {
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

fn hook_payload(event: &str, session_id: &str, cwd: &std::path::Path, transcript_path: &PathBuf) -> serde_json::Value {
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
    fire_hooks(no_hooks, settings, "Stop", &hook_payload("Stop", session_id, cwd, transcript_path));
    write_transcript_record(transcript_path, &transcript_record("system", session_id, serde_json::json!({
        "subtype": "turn_duration",
        "durationMs": 0,
    })));
}

fn handle_prompt(prompt: &str) {
    if let Some(text) = prompt.strip_prefix("/echo ") {
        println!("{}", text.trim());
    }
    if let Some(code) = prompt.strip_prefix("/exit ") {
        let code: i32 = code.trim().parse().unwrap_or(0);
        std::process::exit(code);
    }
}

const FLAGS: &[&str] = &["--print", "-p", "--no-session-start", "--break-hooks"];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let print_mode = args.iter().any(|a| a == "--print" || a == "-p");
    let no_session_start = args.iter().any(|a| a == "--no-session-start");
    let no_hooks = args.iter().any(|a| a == "--break-hooks");

    if print_mode {
        // In --print mode, treat the last non-flag argument as the prompt,
        // process it, and exit immediately (no interactive loop).
        let prompt = args.iter()
            .skip(1)
            .filter(|a| !FLAGS.contains(&a.as_str()))
            .last()
            .cloned()
            .unwrap_or_default();
        handle_prompt(&prompt);
        return;
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

    let session_id = "mock-session-id-1234";
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
    let transcript_path = transcript_dir.join(format!("{}.jsonl", session_id));

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
        let mut payload = hook_payload("SessionStart", session_id, &cwd, &transcript_path);
        payload["source"] = serde_json::json!("startup");
        payload["model"] = serde_json::json!("mock");
        fire_hooks(no_hooks, &settings, "SessionStart", &payload);
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
    let mut newline_mode = NewlineMode::Alternating;
    let mut newline_count: u64 = 0;

    loop {
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let b = byte[0];
        match b {
            0x03 | 0x04 => {
                if last_interrupt == Some(b) {
                    // Two consecutive C-c or two consecutive C-d: exit.
                    break;
                }
                last_interrupt = Some(b);
            }
            0x1b => {
                // Single or double Esc: interrupt / rewind mode — never exit.
                last_interrupt = Some(b);
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

                if let Some(mode) = prompt.strip_prefix("/newline-mode ") {
                    match mode.trim() {
                        "always-submit" => newline_mode = NewlineMode::AlwaysSubmit,
                        "alternating" => {
                            newline_mode = NewlineMode::Alternating;
                            newline_count = 0;
                        }
                        other => eprintln!("mock_claude: unknown newline mode {:?}", other),
                    }
                    continue;
                }

                if let Some(text) = prompt.strip_prefix("/echo ") {
                    println!("{}", text.trim());
                    continue;
                }

                if let Some(secs) = prompt.strip_prefix("/sleep ") {
                    let secs: u64 = secs.trim().parse().unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    continue;
                }
                if let Some(secs) = prompt.strip_prefix("/permission ") {
                    // Simulate Claude processing a tool use then awaiting permission.
                    // Like /busy, but after the busy period fires PermissionRequest
                    // instead of finishing. When interrupted in the permission dialog,
                    // real Claude writes transcript `user` events but does NOT fire
                    // any hooks — so slopd never learns the state changed.
                    let secs: u64 = secs.trim().parse().unwrap_or(0);

                    // Fire hooks for the prompt submission and tool use.
                    write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                        "message": { "role": "user", "content": &prompt },
                    })));
                    let mut submit_payload = hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    submit_payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &submit_payload);
                    write_transcript_record(&transcript_path, &transcript_record("assistant", session_id, serde_json::json!({
                        "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                    })));

                    fire_hooks(no_hooks, &settings, "PreToolUse", &hook_payload("PreToolUse", session_id, &cwd, &transcript_path));

                    // Busy period (tool use running), like /busy.
                    std::thread::sleep(std::time::Duration::from_secs(secs));

                    // Now the tool needs permission — fire PermissionRequest.
                    fire_hooks(no_hooks, &settings, "PermissionRequest", &hook_payload("PermissionRequest", session_id, &cwd, &transcript_path));

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
                                write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                                    "message": {
                                        "role": "user",
                                        "content": [{
                                            "type": "tool_result",
                                            "tool_use_id": format!("mock-tool-use-{}", uuid_counter()),
                                            "content": "The user doesn't want to proceed with this tool use. The tool use was rejected.",
                                            "is_error": true,
                                        }],
                                    },
                                })));
                                // Second: interrupt message.
                                write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                                    "message": {
                                        "role": "user",
                                        "content": [{
                                            "type": "text",
                                            "text": "[Request interrupted by user for tool use]",
                                        }],
                                    },
                                })));
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
                if let Some(secs) = prompt.strip_prefix("/busy ") {
                    // Simulate Claude running a tool use for `secs` seconds.
                    // During this time the real Claude still accepts terminal input and
                    // queues it; once the tool finishes, the queued prompt is submitted.
                    // Supports multiple queued prompts, interrupts (cancel), and the
                    // corresponding queue-operation transcript records.
                    let secs: u64 = secs.trim().parse().unwrap_or(0);

                    // Fire UserPromptSubmit for the /busy command itself (the user submitted it),
                    // and write user + assistant transcript records like real Claude does.
                    write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                        "message": { "role": "user", "content": &prompt },
                    })));
                    let mut busy_payload = hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    busy_payload["prompt"] = serde_json::json!(&prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &busy_payload);
                    write_transcript_record(&transcript_path, &transcript_record("assistant", session_id, serde_json::json!({
                        "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                    })));

                    fire_hooks(no_hooks, &settings, "PreToolUse", &hook_payload("PreToolUse", session_id, &cwd, &transcript_path));

                    let busy_input = read_busy_input(
                        stdin_fd,
                        &mut stdin,
                        &mut newline_mode,
                        &mut newline_count,
                        std::time::Duration::from_secs(secs),
                        &transcript_path,
                        session_id,
                    );

                    fire_hooks(no_hooks, &settings, "PostToolUse", &hook_payload("PostToolUse", session_id, &cwd, &transcript_path));

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
                                write_transcript_record(&transcript_path, &transcript_record(
                                    "queue-operation", session_id, serde_json::json!({
                                        "operation": "remove",
                                    }),
                                ));
                            }
                            fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                        }
                        BusyInput::Queued(prompts) => {
                            // Enqueue records were already written in read_busy_input.
                            // Write dequeue for each (consumed).
                            for _ in &prompts {
                                write_transcript_record(&transcript_path, &transcript_record(
                                    "queue-operation", session_id, serde_json::json!({
                                        "operation": "dequeue",
                                    }),
                                ));
                            }
                            // Process the last queued prompt (like real Claude — last wins).
                            let last = prompts.last().unwrap();
                            write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                                "message": { "role": "user", "content": last },
                            })));
                            write_transcript_record(&transcript_path, &transcript_record("assistant", session_id, serde_json::json!({
                                "message": { "role": "assistant", "content": format!("mock response to: {}", last) },
                            })));
                            let mut payload = hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                            payload["prompt"] = serde_json::json!(last);
                            fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                            fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                        }
                    }
                    continue;
                }
                if let Some(code) = prompt.strip_prefix("/exit ") {
                    let code: i32 = code.trim().parse().unwrap_or(0);
                    let mut payload = hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
                    payload["prompt"] = serde_json::json!(prompt);
                    fire_hooks(no_hooks, &settings, "UserPromptSubmit", &payload);
                    fire_stop(no_hooks, &settings, session_id, &cwd, &transcript_path);
                    unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios); }
                    std::process::exit(code);
                }
                if let Some(key) = prompt.strip_prefix("/env ") {
                    let val = std::env::var(key.trim())
                        .unwrap_or_else(|_| "UNSET".to_string());
                    println!("/env:{}={}", key.trim(), val);
                    continue;
                }
                if let Some(event) = prompt.strip_prefix("/hook ") {
                    fire_hooks(no_hooks, &settings, event.trim(), &hook_payload(event.trim(), session_id, &cwd, &transcript_path));
                    // Fall through to fire UserPromptSubmit so slopctl send unblocks.
                }

                if prompt == "/break-stdin" {
                    break;
                }
                if prompt == "/break-hooks" {
                    let mut buf = [0u8; 256];
                    while stdin.read(&mut buf).unwrap_or(0) > 0 {}
                    break;
                }
                if prompt == "/run" {
                    // Spawn a child pane via slopctl run. TMUX_PANE is set automatically
                    // by tmux in our environment, so the child will have @slopd_ancestor_panes
                    // pointing at us without any manual wiring.
                    let slopctl = std::env::var("SLOPCTL").unwrap_or_else(|_| "slopctl".to_string());
                    let output = Command::new(&slopctl)
                        .arg("run")
                        .stdout(Stdio::piped())
                        .spawn()
                        .and_then(|c| c.wait_with_output());
                    match output {
                        Ok(out) if out.status.success() => {
                            let child_pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
                            // Print child pane ID so the test can read it from tmux pane content.
                            println!("/run:{}", child_pane);
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
                write_transcript_record(&transcript_path, &transcript_record("user", session_id, serde_json::json!({
                    "message": { "role": "user", "content": &prompt },
                })));
                write_transcript_record(&transcript_path, &transcript_record("assistant", session_id, serde_json::json!({
                    "message": { "role": "assistant", "content": format!("mock response to: {}", &prompt) },
                })));

                let mut payload = hook_payload("UserPromptSubmit", session_id, &cwd, &transcript_path);
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

    unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig_termios); }
}
