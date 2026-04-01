use std::collections::{HashMap, HashSet};

pub struct TreeRow {
    pub pane_id: String,
    pub depth: usize,
    pub state: libslop::PaneState,
    pub detailed_state: libslop::PaneDetailedState,
    pub tags: Vec<String>,
    pub working_dir: Option<String>,
    pub created_at: u64,
    pub last_active: u64,
    pub session_id: Option<String>,
    pub parent_pane_id: Option<String>,
    pub is_last_sibling: bool,
}

/// A single message in the transcript, distilled for display.
pub struct TranscriptMessage {
    pub role: MessageRole,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone)]
pub enum View {
    PaneList,
    PaneRepl { pane_id: String },
}

pub struct App {
    pub rows: Vec<TreeRow>,
    selected: Option<usize>,
    pub view: View,
    pub transcript: Vec<TranscriptMessage>,
    pub transcript_scroll: usize,
    pub input: String,
    pub input_cursor: usize,
}

pub enum AppEvent {
    SelectNext,
    SelectPrev,
    Quit,
    Refresh,
    /// Enter the REPL for the currently selected pane.
    Enter,
    /// Go back to the pane list.
    Back,
    /// A transcript record arrived.
    TranscriptRecord(libslop::Record),
    /// User typed a character in the input field.
    InputChar(char),
    /// User pressed backspace.
    InputBackspace,
    /// User pressed delete.
    InputDelete,
    /// Move input cursor left.
    InputLeft,
    /// Move input cursor right.
    InputRight,
    /// Submit the current input as a prompt.
    InputSubmit,
    /// Interrupt the current pane (Ctrl+C).
    Interrupt,
    /// Scroll transcript up.
    ScrollUp,
    /// Scroll transcript down.
    ScrollDown,
}

pub enum AppAction {
    Redraw,
    FetchAndRedraw,
    Quit,
    /// Enter pane REPL — caller should subscribe to transcript.
    EnterRepl { pane_id: String },
    /// Leave pane REPL — caller should unsubscribe from transcript.
    LeaveRepl,
    /// Send a prompt to the pane.
    SendPrompt { pane_id: String, prompt: String },
    /// Interrupt the pane.
    InterruptPane { pane_id: String },
    /// No action needed.
    Noop,
}

impl App {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            selected: None,
            view: View::PaneList,
            transcript: Vec::new(),
            transcript_scroll: 0,
            input: String::new(),
            input_cursor: 0,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn update_panes(&mut self, panes: Vec<libslop::PaneInfo>) {
        let old_selected_pane = self
            .selected
            .and_then(|i| self.rows.get(i))
            .map(|r| r.pane_id.clone());

        self.rows = build_tree(panes);

        let new_index =
            old_selected_pane.and_then(|id| self.rows.iter().position(|r| r.pane_id == id));
        if let Some(idx) = new_index {
            self.selected = Some(idx);
        } else if !self.rows.is_empty() {
            let sel = self.selected.unwrap_or(0);
            self.selected = Some(sel.min(self.rows.len() - 1));
        } else {
            self.selected = None;
        }
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.selected {
            Some(i) => (i + 1).min(self.rows.len() - 1),
            None => 0,
        };
        self.selected = Some(i);
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = match self.selected {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.selected = Some(i);
    }

    pub fn push_transcript_record(&mut self, record: &libslop::Record) {
        if let Some(msg) = parse_transcript_record(record) {
            self.transcript.push(msg);
        }
    }

    pub fn apply_event(&mut self, event: &AppEvent) -> AppAction {
        match (&self.view, event) {
            // ── Pane list view ──
            (View::PaneList, AppEvent::SelectNext) => {
                self.select_next();
                AppAction::Redraw
            }
            (View::PaneList, AppEvent::SelectPrev) => {
                self.select_prev();
                AppAction::Redraw
            }
            (View::PaneList, AppEvent::Quit) => AppAction::Quit,
            (View::PaneList, AppEvent::Refresh) => AppAction::FetchAndRedraw,
            (View::PaneList, AppEvent::Enter) => {
                if let Some(row) = self.selected.and_then(|i| self.rows.get(i)) {
                    let pane_id = row.pane_id.clone();
                    self.view = View::PaneRepl {
                        pane_id: pane_id.clone(),
                    };
                    self.transcript.clear();
                    self.transcript_scroll = 0;
                    self.input.clear();
                    self.input_cursor = 0;
                    AppAction::EnterRepl { pane_id }
                } else {
                    AppAction::Noop
                }
            }

            // ── Pane REPL view ──
            (View::PaneRepl { .. }, AppEvent::Back | AppEvent::Quit) => {
                self.view = View::PaneList;
                AppAction::LeaveRepl
            }
            (View::PaneRepl { .. }, AppEvent::Refresh) => AppAction::FetchAndRedraw,
            (View::PaneRepl { .. }, AppEvent::TranscriptRecord(record)) => {
                self.push_transcript_record(record);
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::InputChar(c)) => {
                self.input.insert(self.input_cursor, *c);
                self.input_cursor += c.len_utf8();
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::InputBackspace) => {
                if self.input_cursor > 0 {
                    let prev = self.input[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.drain(prev..self.input_cursor);
                    self.input_cursor = prev;
                }
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::InputDelete) => {
                if self.input_cursor < self.input.len() {
                    let next = self.input[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input.len());
                    self.input.drain(self.input_cursor..next);
                }
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::InputLeft) => {
                if self.input_cursor > 0 {
                    self.input_cursor = self.input[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::InputRight) => {
                if self.input_cursor < self.input.len() {
                    self.input_cursor = self.input[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input.len());
                }
                AppAction::Redraw
            }
            (View::PaneRepl { pane_id }, AppEvent::InputSubmit) => {
                let prompt = self.input.clone();
                if prompt.is_empty() {
                    return AppAction::Noop;
                }
                let pane_id = pane_id.clone();
                self.input.clear();
                self.input_cursor = 0;
                AppAction::SendPrompt { pane_id, prompt }
            }
            (View::PaneRepl { pane_id }, AppEvent::Interrupt) => {
                AppAction::InterruptPane {
                    pane_id: pane_id.clone(),
                }
            }
            (View::PaneRepl { .. }, AppEvent::ScrollUp) => {
                self.transcript_scroll = self.transcript_scroll.saturating_add(3);
                AppAction::Redraw
            }
            (View::PaneRepl { .. }, AppEvent::ScrollDown) => {
                self.transcript_scroll = self.transcript_scroll.saturating_sub(3);
                AppAction::Redraw
            }

            _ => AppAction::Noop,
        }
    }
}

/// Parse a transcript Record into a displayable message, or None if it should be hidden.
fn parse_transcript_record(record: &libslop::Record) -> Option<TranscriptMessage> {
    if record.source != "transcript" {
        return None;
    }

    let role = match record.event_type.as_str() {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        "tool_use" | "tool_result" => MessageRole::Tool,
        _ => return None,
    };

    // Extract text content from the payload.
    let text = extract_message_text(&record.payload, &record.event_type)?;
    if text.is_empty() {
        return None;
    }

    Some(TranscriptMessage { role, text })
}

/// Extract displayable text from a transcript record payload.
fn extract_message_text(payload: &serde_json::Value, event_type: &str) -> Option<String> {
    let obj = payload.as_object()?;

    match event_type {
        "user" | "assistant" => {
            if let Some(message) = obj.get("message") {
                if let Some(s) = message.as_str() {
                    return Some(s.to_string());
                }
                if let Some(content) = message.get("content").and_then(serde_json::Value::as_array) {
                    let texts: Vec<&str> = content
                        .iter()
                        .filter_map(|block: &serde_json::Value| {
                            if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                                block.get("text").and_then(serde_json::Value::as_str)
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !texts.is_empty() {
                        return Some(texts.join("\n"));
                    }
                }
            }
            if let Some(content) = obj.get("content") {
                if let Some(s) = content.as_str() {
                    return Some(s.to_string());
                }
            }
            None
        }
        "tool_use" => {
            let name = obj
                .get("name")
                .or_else(|| obj.get("tool_name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            Some(format!("[tool: {name}]"))
        }
        "tool_result" => {
            let content = obj
                .get("content")
                .or_else(|| obj.get("output"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(result)");
            let truncated = if content.len() > 200 {
                format!("{}…", &content[..200])
            } else {
                content.to_string()
            };
            Some(format!("[result: {truncated}]"))
        }
        "system" => {
            obj.get("content")
                .or_else(|| obj.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(|s| s.to_string())
        }
        _ => None,
    }
}

pub fn build_tree(panes: Vec<libslop::PaneInfo>) -> Vec<TreeRow> {
    let mut children: HashMap<Option<String>, Vec<&libslop::PaneInfo>> = HashMap::new();
    let pane_ids: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();

    for pane in &panes {
        let parent_key = pane
            .parent_pane_id
            .as_ref()
            .filter(|pid| pane_ids.contains(pid.as_str()))
            .cloned();
        children.entry(parent_key).or_default().push(pane);
    }

    for group in children.values_mut() {
        group.sort_by_key(|p| p.created_at);
    }

    let mut result = Vec::new();
    fn walk<'a>(
        parent: Option<String>,
        depth: usize,
        children: &HashMap<Option<String>, Vec<&'a libslop::PaneInfo>>,
        result: &mut Vec<TreeRow>,
    ) {
        if let Some(group) = children.get(&parent) {
            let len = group.len();
            for (i, pane) in group.iter().enumerate() {
                result.push(TreeRow {
                    pane_id: pane.pane_id.clone(),
                    depth,
                    state: pane.state.clone(),
                    detailed_state: pane.detailed_state.clone(),
                    tags: pane.tags.clone(),
                    working_dir: pane.working_dir.clone(),
                    created_at: pane.created_at,
                    last_active: pane.last_active,
                    session_id: pane.session_id.clone(),
                    parent_pane_id: pane.parent_pane_id.clone(),
                    is_last_sibling: i == len - 1,
                });
                walk(Some(pane.pane_id.clone()), depth + 1, children, result);
            }
        }
    }
    walk(None, 0, &children, &mut result);
    result
}

/// Subscribe to pane lifecycle events and forward them as `AppEvent::Refresh`.
/// Spawns a background task; the caller should merge other events into the same `tx`.
pub async fn subscribe_pane_events<R, W>(
    client: &mut libslopctl::Client<R, W>,
    tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) -> Result<(), libslopctl::Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut subscription = client
        .subscribe(vec![
            libslop::EventFilter {
                source: Some("slopd".to_string()),
                event_type: Some("StateChange".to_string()),
                pane_id: None,
                session_id: None,
                payload_match: Default::default(),
            },
            libslop::EventFilter {
                source: Some("slopd".to_string()),
                event_type: Some("PaneCreated".to_string()),
                pane_id: None,
                session_id: None,
                payload_match: Default::default(),
            },
            libslop::EventFilter {
                source: Some("slopd".to_string()),
                event_type: Some("PaneDestroyed".to_string()),
                pane_id: None,
                session_id: None,
                payload_match: Default::default(),
            },
        ])
        .await?;

    tokio::spawn(async move {
        loop {
            match subscription.next().await {
                Ok(Some(_)) => {
                    if tx.send(AppEvent::Refresh).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("subscription error: {}", e);
                    break;
                }
            }
        }
    });

    Ok(())
}

/// Subscribe to a pane's transcript and forward records as `AppEvent::TranscriptRecord`.
/// Replays the last `replay_n` records, then streams live.
/// Returns the subscription ID so the caller can unsubscribe later.
pub async fn subscribe_transcript<R, W>(
    client: &mut libslopctl::Client<R, W>,
    pane_id: String,
    replay_n: u64,
    tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) -> Result<u64, libslopctl::Error>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut subscription = client
        .subscribe_transcript(pane_id, replay_n)
        .await?;
    let sub_id = subscription.id();

    tokio::spawn(async move {
        loop {
            match subscription.next().await {
                Ok(Some(libslopctl::SubscriptionItem::Record(record))) => {
                    if tx.send(AppEvent::TranscriptRecord(record)).is_err() {
                        break;
                    }
                }
                Ok(Some(libslopctl::SubscriptionItem::Subscribed)) => {}
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("transcript subscription error: {}", e);
                    break;
                }
            }
        }
    });

    Ok(sub_id)
}
