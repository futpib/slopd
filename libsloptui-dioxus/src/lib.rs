use std::time::{Duration, SystemTime};

use dioxus::prelude::*;
use dioxus_html::input_data::keyboard_types::{Key, Modifiers};
use dioxus_tui::use_keyboard_input;
use dioxus_tui::TuiContext;
use libsloptui_core::{
    App, AppEvent, MessageRole, TreeRow, View,
};
use tokio::net::UnixStream;

// ── Logging ────────────────────────────────────────────────────────────

pub fn setup_logging(verbose: u8) -> tracing_appender::non_blocking::WorkerGuard {
    let state_dir = dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".local/state"))
        .join("sloptui");
    std::fs::create_dir_all(&state_dir).ok();

    let file_appender = tracing_appender::rolling::never(&state_dir, "sloptui.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let level = libslop::verbosity_to_level(verbose);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level.as_str())),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    guard
}

// ── Colors ─────────────────────────────────────────────────────────────

const BG: &str = "#16161e";
const SURFACE: &str = "#1e1e2e";
const BORDER_COLOR: &str = "#45475a";
const TEXT_COLOR: &str = "#cdd6f4";
const TEXT_DIM: &str = "#6c7086";
const TEXT_MUTED: &str = "#585b70";
const HIGHLIGHT_BG: &str = "#313244";

fn state_color(state: &libslop::PaneState) -> &'static str {
    match state {
        libslop::PaneState::BootingUp => "#f9e2af",
        libslop::PaneState::Ready => "#a6e3a1",
        libslop::PaneState::Busy => "#89b4fa",
        libslop::PaneState::AwaitingInput => "#cba6f7",
    }
}

fn state_icon(state: &libslop::PaneState) -> &'static str {
    match state {
        libslop::PaneState::BootingUp => "◐",
        libslop::PaneState::Ready => "●",
        libslop::PaneState::Busy => "◉",
        libslop::PaneState::AwaitingInput => "◈",
    }
}

fn role_color(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "#89b4fa",
        MessageRole::Assistant => "#a6e3a1",
        MessageRole::Tool => "#6c7086",
        MessageRole::System => "#f9e2af",
    }
}

fn role_label(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "you",
        MessageRole::Assistant => "claude",
        MessageRole::Tool => "tool",
        MessageRole::System => "system",
    }
}

fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

fn format_time_ago(ts: u64) -> String {
    let now = SystemTime::now();
    let epoch = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let fmt = timeago::Formatter::new();
    fmt.convert(epoch.saturating_sub(Duration::from_secs(ts)))
}

// ── Tree prefix ────────────────────────────────────────────────────────

fn tree_prefix(rows: &[TreeRow], index: usize) -> String {
    let row = &rows[index];
    if row.depth == 0 {
        return String::new();
    }

    let mut depth_is_last = vec![false; row.depth];
    depth_is_last[row.depth - 1] = row.is_last_sibling;

    let mut target_depth = row.depth as i32 - 2;
    let mut i = index;
    while target_depth >= 0 && i > 0 {
        i -= 1;
        if rows[i].depth == target_depth as usize {
            depth_is_last[target_depth as usize] = rows[i].is_last_sibling;
            target_depth -= 1;
        }
    }

    let mut prefix = String::new();
    for d in 0..row.depth - 1 {
        if depth_is_last[d] {
            prefix.push_str("   ");
        } else {
            prefix.push_str(" │ ");
        }
    }
    if row.is_last_sibling {
        prefix.push_str(" └─");
    } else {
        prefix.push_str(" ├─");
    }
    prefix
}

// ── Client event loop (transport-agnostic) ─────────────────────────────

/// Run the event loop on an already-connected client.
/// Updates `app` and sends events through `tx`.
pub async fn run_event_loop<R, W>(
    mut client: libslopctl::Client<R, W>,
    mut app: Signal<App>,
    mut tx_signal: Signal<Option<tokio::sync::mpsc::UnboundedSender<AppEvent>>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    if let Ok(panes) = client.ps().await {
        app.write().update_panes(panes);
    }

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    tx_signal.set(Some(event_tx.clone()));

    if let Err(e) = libsloptui_core::subscribe_pane_events(&mut client, event_tx.clone()).await {
        tracing::error!("failed to subscribe: {}", e);
    }

    let mut transcript_sub_id: Option<u64> = None;

    loop {
        let event = match event_rx.recv().await {
            Some(e) => e,
            None => break,
        };
        let action = app.write().apply_event(&event);
        match action {
            libsloptui_core::AppAction::Quit => break,
            libsloptui_core::AppAction::Noop => {}
            libsloptui_core::AppAction::Redraw => {}
            libsloptui_core::AppAction::FetchAndRedraw => {
                if let Ok(panes) = client.ps().await {
                    app.write().update_panes(panes);
                }
            }
            libsloptui_core::AppAction::EnterRepl { pane_id } => {
                match libsloptui_core::subscribe_transcript(
                    &mut client,
                    pane_id,
                    100,
                    event_tx.clone(),
                )
                .await
                {
                    Ok(sub_id) => transcript_sub_id = Some(sub_id),
                    Err(e) => tracing::error!("transcript subscribe failed: {}", e),
                }
            }
            libsloptui_core::AppAction::LeaveRepl => {
                if let Some(sub_id) = transcript_sub_id.take() {
                    let _ = client.unsubscribe_by_id(sub_id).await;
                }
            }
            libsloptui_core::AppAction::SendPrompt { pane_id, prompt } => {
                if let Err(e) = client.send_prompt(pane_id, prompt, 60, false).await {
                    tracing::error!("send failed: {}", e);
                }
            }
            libsloptui_core::AppAction::InterruptPane { pane_id } => {
                if let Err(e) = client.interrupt(pane_id).await {
                    tracing::error!("interrupt failed: {}", e);
                }
            }
        }
    }
}

// ── Launch for Unix socket ─────────────────────────────────────────────

pub fn launch_unix() {
    let _ = dioxus_tui::launch(UnixApp);
}

#[component]
fn UnixApp() -> Element {
    let app = use_signal(App::new);
    let tx = use_signal(|| None::<tokio::sync::mpsc::UnboundedSender<AppEvent>>);

    use_future(move || async move {
        let socket_path = libslop::socket_path();
        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to connect to {}: {}", socket_path.display(), e);
                return;
            }
        };
        let (reader, writer) = stream.into_split();
        let client = libslopctl::Client::new(reader, writer);
        run_event_loop(client, app, tx).await;
    });

    rsx! { AppRoot { app, tx } }
}

// ── Root component (public for reuse by other binaries) ────────────────

#[component]
pub fn AppRoot(app: Signal<App>, tx: Signal<Option<tokio::sync::mpsc::UnboundedSender<AppEvent>>>) -> Element {
    let tui: TuiContext = consume_context();
    let key_input = use_keyboard_input();

    // Handle keyboard input.
    use_effect(move || {
        let Some(data) = key_input.read().clone() else {
            return;
        };
        let Some(ref tx) = *tx.read() else {
            return;
        };

        let view = app.read().view.clone();
        let event = match &view {
            View::PaneList => match data.key() {
                Key::Character(ref c) if c == "q" => {
                    tui.quit();
                    return;
                }
                Key::Escape => {
                    tui.quit();
                    return;
                }
                Key::Character(ref c) if c == "j" => Some(AppEvent::SelectNext),
                Key::Character(ref c) if c == "k" => Some(AppEvent::SelectPrev),
                Key::ArrowDown => Some(AppEvent::SelectNext),
                Key::ArrowUp => Some(AppEvent::SelectPrev),
                Key::Enter => Some(AppEvent::Enter),
                _ => None,
            },
            View::PaneRepl { .. } => {
                if data.modifiers().contains(Modifiers::CONTROL) {
                    match data.key() {
                        Key::Character(ref c) if c == "c" => Some(AppEvent::Interrupt),
                        _ => None,
                    }
                } else {
                    match data.key() {
                        Key::Escape => Some(AppEvent::Back),
                        Key::Enter => Some(AppEvent::InputSubmit),
                        Key::Backspace => Some(AppEvent::InputBackspace),
                        Key::Delete => Some(AppEvent::InputDelete),
                        Key::ArrowLeft => Some(AppEvent::InputLeft),
                        Key::ArrowRight => Some(AppEvent::InputRight),
                        Key::PageUp => Some(AppEvent::ScrollUp),
                        Key::PageDown => Some(AppEvent::ScrollDown),
                        Key::Character(ref c) => {
                            if let Some(ch) = c.chars().next() {
                                Some(AppEvent::InputChar(ch))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
            }
        };

        if let Some(event) = event {
            let _ = tx.send(event);
        }
    });

    let view = app.read().view.clone();

    rsx! {
        div {
            width: "100%",
            height: "100%",
            background_color: BG,
            color: TEXT_COLOR,
            tabindex: "0",

            match view {
                View::PaneList => rsx! { PaneListView { app } },
                View::PaneRepl { ref pane_id } => rsx! { ReplView { app, pane_id: pane_id.clone() } },
            }
        }
    }
}

// ── Pane list view ─────────────────────────────────────────────────────

#[component]
fn PaneListView(app: Signal<App>) -> Element {
    let app_ref = app.read();
    let count = app_ref.rows.len();

    rsx! {
        div {
            width: "100%",
            height: "100%",
            display: "flex",
            flex_direction: "column",

            // Header
            div {
                display: "flex",
                justify_content: "space-between",
                span { color: TEXT_COLOR, "Panes ({count})" }
                span { color: TEXT_DIM, "j/k navigate  Enter open  q quit" }
            }

            // Divider
            div { color: BORDER_COLOR, "─────────────────────────────────────────────────────────────────────────────────────────────────────────────" }

            // List
            div {
                width: "100%",
                display: "flex",
                flex_direction: "row",
                flex_grow: "1",
                min_height: "0px",

                // Pane list
                div {
                    flex_grow: "1",
                    display: "flex",
                    flex_direction: "column",

                    for (i, row) in app_ref.rows.iter().enumerate() {
                        {
                            let selected = app_ref.selected() == Some(i);
                            let prefix = tree_prefix(&app_ref.rows, i);
                            let color = state_color(&row.state);
                            let icon = state_icon(&row.state);
                            let bg = if selected { HIGHLIGHT_BG } else { SURFACE };
                            let tags_str = if row.tags.is_empty() {
                                String::new()
                            } else {
                                format!("  {}", row.tags.join(", "))
                            };

                            rsx! {
                                div {
                                    background_color: bg,
                                    display: "flex",

                                    if selected {
                                        span { color: color, "▌ " }
                                    } else {
                                        span { "  " }
                                    }
                                    span { color: BORDER_COLOR, "{prefix}" }
                                    span { color: color, "{icon} " }
                                    span { color: TEXT_COLOR, "{row.pane_id}" }
                                    span { color: color, "  {row.detailed_state.as_str()}" }
                                    span { color: TEXT_MUTED, "{tags_str}" }
                                }
                            }
                        }
                    }
                }

                // Detail panel
                div {
                    min_width: "40ch",
                    padding_left: "2ch",
                    display: "flex",
                    flex_direction: "column",

                    {
                        if let Some(row) = app_ref.selected().and_then(|i| app_ref.rows.get(i)) {
                            let color = state_color(&row.state);
                            let icon = state_icon(&row.state);
                            let working_dir = row.working_dir.as_deref().map(|d| shorten_home(d)).unwrap_or_else(|| "—".to_string());
                            let session = row.session_id.as_deref().unwrap_or("—").to_string();
                            let parent = row.parent_pane_id.as_deref().unwrap_or("—").to_string();
                            let tags = if row.tags.is_empty() { "—".to_string() } else { row.tags.join(", ") };
                            let created = format_time_ago(row.created_at);
                            let last_active = format_time_ago(row.last_active);

                            rsx! {
                                div {
                                    span { color: color, "{icon} " }
                                    span { color: TEXT_COLOR, "{row.pane_id}" }
                                }
                                div { "" }
                                DetailLine { label: "State", value: row.detailed_state.as_str().to_string(), color: color.to_string() }
                                DetailLine { label: "Working Dir", value: working_dir, color: TEXT_DIM.to_string() }
                                DetailLine { label: "Session", value: session, color: TEXT_DIM.to_string() }
                                DetailLine { label: "Parent", value: parent, color: TEXT_DIM.to_string() }
                                DetailLine { label: "Tags", value: tags, color: TEXT_DIM.to_string() }
                                div { "" }
                                DetailLine { label: "Created", value: created, color: TEXT_DIM.to_string() }
                                DetailLine { label: "Last Active", value: last_active, color: TEXT_DIM.to_string() }
                            }
                        } else {
                            rsx! { div { color: TEXT_MUTED, "No pane selected" } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DetailLine(label: String, value: String, color: String) -> Element {
    rsx! {
        div {
            span { color: TEXT_MUTED, "{label:<14}" }
            span { color: color, "{value}" }
        }
    }
}

// ── REPL view ──────────────────────────────────────────────────────────

#[component]
fn ReplView(app: Signal<App>, pane_id: String) -> Element {
    let app_ref = app.read();

    let pane_state = app_ref
        .rows
        .iter()
        .find(|r| r.pane_id == pane_id)
        .map(|r| (&r.state, &r.detailed_state));

    rsx! {
        div {
            width: "100%",
            height: "100%",
            display: "flex",
            flex_direction: "column",

            // Header
            div {
                display: "flex",
                justify_content: "space-between",

                div {
                    if let Some((state, detailed)) = pane_state {
                        span { color: state_color(state), "{state_icon(state)} " }
                        span { color: TEXT_COLOR, "{pane_id}" }
                        span { color: state_color(state), "  {detailed.as_str()}" }
                    } else {
                        span { color: TEXT_COLOR, "{pane_id}" }
                    }
                }
                span { color: TEXT_DIM, "Enter send  C-c interrupt  Esc back  PgUp/PgDn scroll" }
            }

            // Divider
            div { color: BORDER_COLOR, "─────────────────────────────────────────────────────────────────────────────────────────────────────────────" }

            // Transcript
            div {
                flex_grow: "1",
                min_height: "0px",
                display: "flex",
                flex_direction: "column",
                background_color: SURFACE,

                for msg in app_ref.transcript.iter() {
                    div {
                        div { color: role_color(msg.role), "{role_label(msg.role)}" }
                        for line in msg.text.lines() {
                            div {
                                color: if msg.role == MessageRole::Tool { TEXT_MUTED } else { TEXT_COLOR },
                                "{line}"
                            }
                        }
                        div { "" }
                    }
                }
            }

            // Divider
            div { color: BORDER_COLOR, "─────────────────────────────────────────────────────────────────────────────────────────────────────────────" }

            // Input
            div {
                display: "flex",
                span { color: "#fab387", "> " }
                span { color: TEXT_COLOR, "{app_ref.input}" }
            }
        }
    }
}
