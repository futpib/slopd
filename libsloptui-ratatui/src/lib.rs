use std::io;
use std::time::{Duration, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use libsloptui_core::{App, AppAction, AppEvent, MessageRole, TreeRow, View};
use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::mpsc;
use tracing::debug;

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

// ── Color palette (Catppuccin Mocha) ───────────────────────────────────

const BG: Color = Color::Rgb(22, 22, 30);
const SURFACE: Color = Color::Rgb(30, 30, 46);
const BORDER: Color = Color::Rgb(69, 71, 90);
const TEXT: Color = Color::Rgb(205, 214, 244);
const TEXT_DIM: Color = Color::Rgb(108, 112, 134);
const TEXT_MUTED: Color = Color::Rgb(88, 91, 112);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const BLUE: Color = Color::Rgb(137, 180, 250);
const MAUVE: Color = Color::Rgb(203, 166, 247);
const PEACH: Color = Color::Rgb(250, 179, 135);
const TREE_LINE: Color = Color::Rgb(69, 71, 90);
const HIGHLIGHT_BG: Color = Color::Rgb(49, 50, 68);
const USER_COLOR: Color = Color::Rgb(137, 180, 250);
const ASSISTANT_COLOR: Color = Color::Rgb(166, 227, 161);
const TOOL_COLOR: Color = Color::Rgb(108, 112, 134);
const SYSTEM_COLOR: Color = Color::Rgb(249, 226, 175);

// ── Helpers ────────────────────────────────────────────────────────────

fn state_color(state: &libslop::PaneState) -> Color {
    match state {
        libslop::PaneState::BootingUp => YELLOW,
        libslop::PaneState::Ready => GREEN,
        libslop::PaneState::Busy => BLUE,
        libslop::PaneState::AwaitingInput => MAUVE,
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

fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

fn format_time_ago(epoch: Duration, ts: u64) -> String {
    let fmt = timeago::Formatter::new();
    fmt.convert(epoch.saturating_sub(Duration::from_secs(ts)))
}

fn role_label(role: MessageRole) -> (&'static str, Color) {
    match role {
        MessageRole::User => ("you", USER_COLOR),
        MessageRole::Assistant => ("claude", ASSISTANT_COLOR),
        MessageRole::Tool => ("tool", TOOL_COLOR),
        MessageRole::System => ("system", SYSTEM_COLOR),
    }
}

fn horizontal_rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(BORDER),
    ))
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

// ── Pane list view ─────────────────────────────────────────────────────

fn render_pane_list_view(frame: &mut Frame, area: Rect, app: &App, list_state: &mut ListState) {
    let wide = area.width >= 80;

    let count = app.rows.len();
    let header = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    // Header: title left, key hints right.
    let hints = key_hints(&[("j/k", "navigate"), ("Enter", "open"), ("q", "quit")]);
    let title_spans: Vec<Span> = vec![
        Span::styled("Panes ", Style::default().fg(TEXT).bold()),
        Span::styled(format!("({count})"), Style::default().fg(TEXT_DIM)),
    ];
    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Min(1)])
        .split(header[0]);
    frame.render_widget(Paragraph::new(Line::from(title_spans)).bg(BG), header_layout[0]);
    frame.render_widget(
        Paragraph::new(Line::from(hints)).alignment(Alignment::Right).bg(BG),
        header_layout[1],
    );
    frame.render_widget(Paragraph::new(horizontal_rule(area.width)).bg(BG), header[1]);

    let body = header[2];

    if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body);

        render_pane_list(frame, cols[0], app, list_state);
        render_detail(frame, cols[1], app);
    } else {
        render_pane_list(frame, body, app, list_state);
    }
}

fn render_pane_list(frame: &mut Frame, area: Rect, app: &App, list_state: &mut ListState) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let prefix = tree_prefix(&app.rows, i);
            let color = state_color(&row.state);
            let icon = state_icon(&row.state);

            let line = Line::from(vec![
                Span::styled(prefix, Style::default().fg(TREE_LINE)),
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(&row.pane_id, Style::default().fg(TEXT).bold()),
                Span::styled(
                    format!("  {}", row.detailed_state.as_str()),
                    Style::default().fg(color),
                ),
                if !row.tags.is_empty() {
                    Span::styled(
                        format!("  {}", row.tags.join(", ")),
                        Style::default().fg(TEXT_MUTED),
                    )
                } else {
                    Span::raw("")
                },
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().bg(SURFACE))
        .highlight_style(
            Style::default()
                .bg(HIGHLIGHT_BG)
                .fg(TEXT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ")
        .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, list_state);
}

// ── Detail panel ───────────────────────────────────────────────────────

fn render_detail(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.selected().and_then(|i| app.rows.get(i));

    let content = if let Some(row) = selected {
        let now = SystemTime::now();
        let epoch = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let created = format_time_ago(epoch, row.created_at);
        let last_active = format_time_ago(epoch, row.last_active);
        let color = state_color(&row.state);
        let icon = state_icon(&row.state);

        let working_dir = row
            .working_dir
            .as_deref()
            .map(|d| shorten_home(d))
            .unwrap_or_else(|| "—".to_string());

        let session = row.session_id.as_deref().unwrap_or("—");
        let parent = row.parent_pane_id.as_deref().unwrap_or("—");
        let tags = if row.tags.is_empty() {
            "—".to_string()
        } else {
            row.tags.join(", ")
        };

        vec![
            Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(&row.pane_id, Style::default().fg(TEXT).bold()),
            ]),
            Line::from(""),
            detail_line("State", row.detailed_state.as_str(), color),
            detail_line("Working Dir", &working_dir, TEXT_DIM),
            detail_line("Session", session, TEXT_DIM),
            detail_line("Parent", parent, TEXT_DIM),
            detail_line("Tags", &tags, TEXT_DIM),
            Line::from(""),
            detail_line("Created", &created, TEXT_DIM),
            detail_line("Last Active", &last_active, TEXT_DIM),
        ]
    } else {
        vec![Line::from(Span::styled(
            "No pane selected",
            Style::default().fg(TEXT_MUTED).italic(),
        ))]
    };

    let paragraph = Paragraph::new(content)
        .block(Block::default().bg(SURFACE))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn detail_line(label: &str, value: &str, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::default().fg(TEXT_MUTED)),
        Span::styled(value.to_string(), Style::default().fg(value_color)),
    ])
}

// ── REPL view ──────────────────────────────────────────────────────────

fn render_repl(frame: &mut Frame, area: Rect, app: &App) {
    let pane_id = match &app.view {
        View::PaneRepl { pane_id } => pane_id.as_str(),
        _ => return,
    };

    let pane_state = app
        .rows
        .iter()
        .find(|r| r.pane_id == pane_id)
        .map(|r| (&r.state, &r.detailed_state));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // divider
            Constraint::Min(3),   // transcript
            Constraint::Length(1), // divider
            Constraint::Length(1), // input
        ])
        .split(area);

    // ── Header: pane info left, key hints right ──
    let title_spans: Vec<Span> = if let Some((state, detailed)) = pane_state {
        let color = state_color(state);
        let icon = state_icon(state);
        vec![
            Span::styled(format!("{icon} "), Style::default().fg(color)),
            Span::styled(pane_id, Style::default().fg(TEXT).bold()),
            Span::styled(
                format!("  {}", detailed.as_str()),
                Style::default().fg(color),
            ),
        ]
    } else {
        vec![Span::styled(pane_id, Style::default().fg(TEXT).bold())]
    };
    let hints = key_hints(&[
        ("Enter", "send"),
        ("C-c", "interrupt"),
        ("Esc", "back"),
        ("PgUp/PgDn", "scroll"),
    ]);
    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Min(1)])
        .split(layout[0]);
    frame.render_widget(Paragraph::new(Line::from(title_spans)).bg(BG), header_layout[0]);
    frame.render_widget(
        Paragraph::new(Line::from(hints)).alignment(Alignment::Right).bg(BG),
        header_layout[1],
    );

    // ── Divider ──
    frame.render_widget(Paragraph::new(horizontal_rule(area.width)).bg(BG), layout[1]);

    // ── Transcript ──
    let transcript_area = layout[2];
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.transcript {
        let (label, color) = role_label(msg.role);
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).bold(),
        )));
        for text_line in msg.text.lines() {
            lines.push(Line::from(Span::styled(
                text_line.to_string(),
                Style::default().fg(if msg.role == MessageRole::Tool {
                    TEXT_MUTED
                } else {
                    TEXT
                }),
            )));
        }
        lines.push(Line::from(""));
    }

    let visible_height = transcript_area.height as usize;
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll_offset = if app.transcript_scroll == 0 {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.transcript_scroll)
    };

    let paragraph = Paragraph::new(lines)
        .bg(SURFACE)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset as u16, 0));
    frame.render_widget(paragraph, transcript_area);

    // ── Divider ──
    frame.render_widget(Paragraph::new(horizontal_rule(area.width)).bg(BG), layout[3]);

    // ── Input ──
    let input_area = layout[4];
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(PEACH).bold()),
        Span::styled(&app.input, Style::default().fg(TEXT)),
    ]);
    frame.render_widget(Paragraph::new(input_line).bg(BG), input_area);

    let cursor_x = input_area.x + 2 + app.input[..app.input_cursor].chars().count() as u16;
    frame.set_cursor_position(Position::new(cursor_x, input_area.y));
}

fn key_hints(keys: &[(&str, &str)]) -> Vec<Span<'static>> {
    keys.iter()
        .enumerate()
        .flat_map(|(i, (key, desc))| {
            let mut v = vec![
                Span::styled(
                    format!(" {key} "),
                    Style::default().fg(SURFACE).bg(TEXT_DIM).bold(),
                ),
                Span::styled(format!(" {desc}"), Style::default().fg(TEXT_DIM)),
            ];
            if i < keys.len() - 1 {
                v.push(Span::styled("   ", Style::default()));
            }
            v
        })
        .collect()
}

// ── Main layout ────────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &App, list_state: &mut ListState) {
    frame.render_widget(Block::default().bg(BG), frame.area());

    match &app.view {
        View::PaneList => render_pane_list_view(frame, frame.area(), app, list_state),
        View::PaneRepl { .. } => render_repl(frame, frame.area(), app),
    }
}

// ── Input bridging ─────────────────────────────────────────────────────

fn map_key_event(key: crossterm::event::KeyEvent, view: &View) -> Option<AppEvent> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match view {
        View::PaneList => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Some(AppEvent::Quit),
            KeyCode::Down | KeyCode::Char('j') => Some(AppEvent::SelectNext),
            KeyCode::Up | KeyCode::Char('k') => Some(AppEvent::SelectPrev),
            KeyCode::Enter => Some(AppEvent::Enter),
            _ => None,
        },
        View::PaneRepl { .. } => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Some(AppEvent::Interrupt);
            }
            match key.code {
                KeyCode::Esc => Some(AppEvent::Back),
                KeyCode::Enter => Some(AppEvent::InputSubmit),
                KeyCode::Backspace => Some(AppEvent::InputBackspace),
                KeyCode::Delete => Some(AppEvent::InputDelete),
                KeyCode::Left => Some(AppEvent::InputLeft),
                KeyCode::Right => Some(AppEvent::InputRight),
                KeyCode::PageUp => Some(AppEvent::ScrollUp),
                KeyCode::PageDown => Some(AppEvent::ScrollDown),
                KeyCode::Char(c) => Some(AppEvent::InputChar(c)),
                _ => None,
            }
        }
    }
}

fn spawn_crossterm_input(
    tx: mpsc::UnboundedSender<AppEvent>,
    view_rx: std::sync::Arc<std::sync::Mutex<View>>,
) {
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                let view = view_rx.lock().unwrap();
                if let Some(app_event) = map_key_event(key, &view) {
                    drop(view);
                    if tx.send(app_event).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

// ── Public entry point ─────────────────────────────────────────────────

pub async fn run<R, W>(client: &mut libslopctl::Client<R, W>) -> Result<(), libslopctl::Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let panes = client.ps().await?;

    let mut app = App::new();
    app.update_panes(panes);

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    libsloptui_core::subscribe_pane_events(client, tx.clone()).await?;

    let view_state = std::sync::Arc::new(std::sync::Mutex::new(View::PaneList));
    spawn_crossterm_input(tx.clone(), std::sync::Arc::clone(&view_state));

    terminal::enable_raw_mode().expect("failed to enable raw mode");
    io::stdout()
        .execute(EnterAlternateScreen)
        .expect("failed to enter alternate screen");
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    let mut list_state = ListState::default();
    list_state.select(app.selected());

    let mut transcript_sub_id: Option<u64> = None;

    terminal
        .draw(|frame| render(frame, &app, &mut list_state))
        .expect("failed to draw");

    loop {
        let event = match rx.recv().await {
            Some(e) => e,
            None => break,
        };
        match app.apply_event(&event) {
            AppAction::Quit => break,
            AppAction::Noop => continue,
            AppAction::Redraw => {}
            AppAction::FetchAndRedraw => match client.ps().await {
                Ok(panes) => app.update_panes(panes),
                Err(e) => {
                    debug!("refresh failed: {}", e);
                    continue;
                }
            },
            AppAction::EnterRepl { pane_id } => {
                match libsloptui_core::subscribe_transcript(client, pane_id, 100, tx.clone()).await
                {
                    Ok(sub_id) => transcript_sub_id = Some(sub_id),
                    Err(e) => debug!("failed to subscribe to transcript: {}", e),
                }
            }
            AppAction::LeaveRepl => {
                if let Some(sub_id) = transcript_sub_id.take() {
                    let _ = client.unsubscribe_by_id(sub_id).await;
                }
            }
            AppAction::SendPrompt { pane_id, prompt } => {
                match client.send_prompt(pane_id, prompt, 60, false).await {
                    Ok(_) => {}
                    Err(e) => debug!("send failed: {}", e),
                }
            }
            AppAction::InterruptPane { pane_id } => {
                match client.interrupt(pane_id).await {
                    Ok(_) => {}
                    Err(e) => debug!("interrupt failed: {}", e),
                }
            }
        }

        *view_state.lock().unwrap() = app.view.clone();

        list_state.select(app.selected());
        terminal
            .draw(|frame| render(frame, &app, &mut list_state))
            .expect("failed to draw");
    }

    terminal::disable_raw_mode().expect("failed to disable raw mode");
    io::stdout()
        .execute(LeaveAlternateScreen)
        .expect("failed to leave alternate screen");

    Ok(())
}
