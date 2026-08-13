//! Ratatui document: header, transcript, input, footer.
//!
//! The app owns no Engine. Every state change is either a local key edit or a
//! message from the `mow rpc` child (`rpc::Client`).

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;
use std::time::Instant;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde_json::Value;

use crate::render::{is_unified_diff, markdown_lines};
use crate::rpc::{
    Client, Error, Notification, PermissionRequest, SessionInfo, SlashCommand, TranscriptMessage,
    token_delta,
};
use crate::theme::Theme;

/// One painted transcript block.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Note(String),
    Tool {
        name: String,
        duration_ms: Option<u64>,
    },
}

/// Host and delegated token usage for the header chip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub peer_tokens: u64,
}

impl Usage {
    pub fn total(self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn chip(self) -> String {
        let host = format_tokens(self.total());
        if self.peer_tokens > 0 {
            format!("{host} tok (⇄ {})", format_tokens(self.peer_tokens))
        } else {
            format!("{host} tok")
        }
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// UI state. `draw` is pure over this struct so `TestBackend` can assert on it.
#[derive(Debug)]
pub struct App {
    pub session: SessionInfo,
    pub entries: Vec<Entry>,
    pub input: String,
    pub busy: bool,
    /// Live assistant text for the running turn (not yet an entry).
    pub live: String,
    pub status: String,
    pub scroll: u16,
    /// Follow the bottom until the operator scrolls up.
    pub follow: bool,
    pub quit: bool,
    pub pending_perm: Option<PermissionRequest>,
    pub usage: Usage,
    pub slash_commands: Vec<SlashCommand>,
    pub last_view_h: u16,
    pub theme: Theme,
    pub activity_started: Option<Instant>,
    pub peers: HashMap<String, String>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            session: SessionInfo::default(),
            entries: Vec::new(),
            input: String::new(),
            busy: false,
            live: String::new(),
            status: String::new(),
            scroll: 0,
            follow: false,
            quit: false,
            pending_perm: None,
            usage: Usage::default(),
            slash_commands: Vec::new(),
            last_view_h: 1,
            theme: Theme::detect(),
            activity_started: None,
            peers: HashMap::new(),
        }
    }
}

impl App {
    pub fn new(session: SessionInfo) -> App {
        App {
            session,
            follow: true,
            ..App::default()
        }
    }

    pub fn from_transcript(session: SessionInfo, messages: Vec<TranscriptMessage>) -> App {
        let mut app = App::new(session);
        app.entries = messages
            .into_iter()
            .map(|message| match message.role.as_str() {
                "user" => Entry::User(message.content),
                "assistant" => Entry::Assistant(message.content),
                _ => Entry::Note(format!("{}: {}", message.role, message.content)),
            })
            .collect();
        app
    }

    pub fn header(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.session.workspace.is_empty() {
            parts.push(self.session.workspace.as_str());
        }
        if !self.session.model.is_empty() {
            parts.push(self.session.model.as_str());
        }
        let short = self.session.short_id();
        let mut s = if parts.is_empty() {
            "mowi".to_string()
        } else {
            format!("mowi · {}", parts.join(" · "))
        };
        if !short.is_empty() {
            s.push_str(" · ");
            s.push_str(&short);
        }
        if self.usage.total() > 0 {
            s.push_str(" · ");
            s.push_str(&self.usage.chip());
        }
        s
    }

    pub fn footer(&self) -> String {
        let hints = if let Some(permission) = &self.pending_perm {
            return format!("y allow · n deny · a always · {}", permission.name);
        } else {
            "enter send · esc cancel · q quit"
        };
        if self.status.is_empty() {
            hints.to_string()
        } else {
            format!("{hints} — {}", self.status)
        }
    }

    /// Transcript as wrapped-source lines (live answer last).
    pub fn transcript_lines(&self) -> Vec<Line<'_>> {
        let mut out: Vec<Line<'_>> = Vec::new();
        for e in &self.entries {
            out.extend(self.entry_lines(e));
            out.push(Line::raw(""));
        }
        if !self.live.is_empty() {
            out.extend(markdown_lines(self.live.as_str(), self.theme));
        }
        out
    }

    fn entry_lines<'a>(&self, entry: &'a Entry) -> Vec<Line<'a>> {
        match entry {
            Entry::User(t) => vec![Line::from(vec![
                Span::styled("> ", self.theme.user()),
                Span::styled(t.as_str(), self.theme.user()),
            ])],
            Entry::Assistant(t) => {
                if is_unified_diff(t) {
                    crate::render::diff_lines(t, self.theme)
                } else {
                    markdown_lines(t, self.theme)
                }
            }
            Entry::Note(t) => vec![Line::styled(t.as_str(), self.theme.note())],
            Entry::Tool { name, duration_ms } => {
                let suffix = duration_ms
                    .map(|ms| format!(" · {:.1}s", ms as f64 / 1000.0))
                    .unwrap_or_default();
                vec![Line::styled(format!("⚙ {name}{suffix}"), self.theme.note())]
            }
        }
    }

    /// Materialize only a bounded transcript window for painting large sessions.
    fn visible_transcript_lines(&self) -> (Vec<Line<'_>>, u16) {
        const OVERSCAN_ENTRIES: usize = 256;
        let total = self.entries.len();
        let start = if self.follow {
            total.saturating_sub(OVERSCAN_ENTRIES)
        } else {
            ((self.scroll as usize) / 2)
                .saturating_sub(OVERSCAN_ENTRIES / 4)
                .min(total)
        };
        let end = (start + OVERSCAN_ENTRIES).min(total);
        let mut lines = Vec::new();
        for entry in &self.entries[start..end] {
            lines.extend(self.entry_lines(entry));
            lines.push(Line::raw(""));
        }
        if end == total && !self.live.is_empty() {
            lines.extend(markdown_lines(self.live.as_str(), self.theme));
        }
        (lines, (start.saturating_mul(2)) as u16)
    }

    pub fn activity(&self) -> String {
        if !self.busy {
            return String::new();
        }
        let elapsed = self
            .activity_started
            .map(|start| start.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        format!("● {:.1}s · {}", elapsed, self.status_or_default())
    }

    fn status_or_default(&self) -> &str {
        if self.status.is_empty() {
            "thinking"
        } else {
            &self.status
        }
    }

    /// Apply an `event` / `perm.ask` notification.
    pub fn on_notification(&mut self, n: &Notification) {
        match n.method.as_str() {
            "event" => self.on_event(&n.params),
            "perm.ask" => {
                if let Some(permission) = n.permission_request() {
                    self.pending_perm = Some(permission);
                }
            }
            _ => {}
        }
    }

    fn on_event(&mut self, params: &Value) {
        let kind = params.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if kind.contains("delegate") && kind.contains("usage") {
            self.usage.peer_tokens += token_count(params, "input_tokens");
            self.usage.peer_tokens += token_count(params, "output_tokens");
        }
        match kind {
            "loop.run.start" | "run.start" => {
                self.busy = true;
                self.status = "running".into();
                self.activity_started = Some(Instant::now());
            }
            "loop.run.end" | "run.end" => {
                self.busy = false;
                self.status.clear();
                self.activity_started = None;
                self.finish_peers();
            }
            k if k.ends_with("tool.start") || k == "tool.start" => {
                if let Some(name) = params
                    .get("tool")
                    .or_else(|| params.get("name"))
                    .and_then(|v| v.as_str())
                {
                    self.status = format!("tool · {name}");
                    self.entries.push(Entry::Tool {
                        name: name.to_string(),
                        duration_ms: None,
                    });
                }
            }
            k if k.ends_with("tool.end") || k == "tool.end" => {
                if let Some(Entry::Tool { duration_ms, .. }) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| matches!(entry, Entry::Tool { .. }))
                {
                    *duration_ms = params.get("duration_ms").and_then(Value::as_u64);
                }
                if params.get("name").and_then(Value::as_str) == Some("acp_delegate") {
                    self.finish_peers();
                }
                self.status.clear();
            }
            _ => {}
        }
        if kind.contains("delegate") && kind.contains("chunk") {
            let agent = params
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("peer")
                .to_string();
            self.peers
                .entry(agent.clone())
                .or_default()
                .push_str(params.get("delta").and_then(Value::as_str).unwrap_or(""));
            self.status = format!("→ {agent} · receiving");
        } else if kind.contains("delegate") && kind.contains("progress") {
            let agent = params
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("peer");
            let phase = params
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or("working");
            self.status = format!("→ {agent} · {phase}");
        }
        if let Some(d) = token_delta(params) {
            self.live.push_str(d);
            if self.follow {
                self.scroll = u16::MAX;
            }
        }
    }

    fn finish_peers(&mut self) {
        for (agent, _) in self.peers.drain() {
            self.entries.push(Entry::Note(format!("→ {agent} · done")));
        }
    }

    /// Finish the turn: commit live text (or the final `prompt` result).
    pub fn finish_turn(&mut self, result: Result<Value, Error>) {
        self.busy = false;
        self.activity_started = None;
        match result {
            Ok(v) => {
                self.usage.input_tokens +=
                    token_count(v.get("usage").unwrap_or(&Value::Null), "input_tokens");
                self.usage.output_tokens +=
                    token_count(v.get("usage").unwrap_or(&Value::Null), "output_tokens");
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let body = if !text.is_empty() {
                    text.to_string()
                } else {
                    std::mem::take(&mut self.live)
                };
                self.live.clear();
                if !body.trim().is_empty() {
                    self.entries.push(Entry::Assistant(body));
                }
            }
            Err(e) => {
                if !self.live.trim().is_empty() {
                    let body = std::mem::take(&mut self.live);
                    self.entries.push(Entry::Assistant(body));
                }
                self.live.clear();
                self.entries.push(Entry::Note(format!("error: {e}")));
            }
        }
        self.status.clear();
        self.finish_peers();
        if self.follow {
            self.scroll = u16::MAX;
        }
    }

    pub fn permission_decision(
        &mut self,
        decision: &str,
        client: &mut Client,
    ) -> Result<(), Error> {
        if let Some(permission) = self.pending_perm.take() {
            client.perm_decide(&permission.id, decision, Duration::from_secs(20))?;
        }
        Ok(())
    }

    pub fn refuses_exclusive_slash(&self, name: &str) -> bool {
        self.busy
            && self
                .slash_commands
                .iter()
                .any(|command| command.exclusive && command.name.trim_start_matches('/') == name)
    }
}

fn token_count(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

/// Paint header / transcript / input / footer.

fn max_scroll(app: &App) -> u16 {
    let n = app.transcript_lines().len() as u16;
    let h = app.last_view_h.max(1);
    n.saturating_sub(h)
}

fn leave_follow(app: &mut App, n: u16) {
    if app.follow {
        app.follow = false;
        app.scroll = max_scroll(app).saturating_sub(n);
        return;
    }
    app.scroll = app.scroll.saturating_sub(n);
}

fn scroll_down(app: &mut App, n: u16) {
    if app.follow {
        return;
    }
    let max = max_scroll(app);
    app.scroll = app.scroll.saturating_add(n).min(max);
    if app.scroll >= max {
        app.follow = true;
        app.scroll = 0;
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let constraints = if app.busy {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(app.header()).style(app.theme.header()),
        areas[0],
    );

    let transcript_area = if app.busy {
        frame.render_widget(
            Paragraph::new(app.activity()).style(app.theme.note()),
            areas[1],
        );
        areas[2]
    } else {
        areas[1]
    };
    app.last_view_h = transcript_area.height.max(1);
    let (lines, base_scroll) = app.visible_transcript_lines();
    let height = app.last_view_h as usize;
    let scroll = if app.follow {
        lines.len().saturating_sub(height) as u16
    } else {
        app.scroll
            .saturating_sub(base_scroll)
            .min(lines.len().saturating_sub(height) as u16)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        transcript_area,
    );

    let input_area = if app.busy { areas[3] } else { areas[2] };
    let footer_area = if app.busy { areas[4] } else { areas[3] };
    frame.render_widget(
        Paragraph::new(format!("› {}", app.input)).style(app.theme.user()),
        input_area,
    );
    frame.render_widget(
        Paragraph::new(app.footer()).style(app.theme.note()),
        footer_area,
    );
}

/// Terminal loop: keys in, RPC messages out, one repaint per tick.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &mut Client,
    app: &mut App,
) -> Result<(), Error> {
    let mut turn: Option<Receiver<Result<Value, Error>>> = None;
    let mut slash_rx: Option<Receiver<Result<Value, Error>>> = None;

    while !app.quit {
        terminal.draw(|f| draw(f, app)).map_err(Error::Io)?;

        // Drain notifications without blocking the key loop.
        loop {
            match client.notifications().try_recv() {
                Ok(n) => app.on_notification(&n),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.entries.push(Entry::Note("mow rpc exited".into()));
                    app.quit = true;
                    break;
                }
            }
        }

        if let Some(rx) = turn.as_ref() {
            match rx.try_recv() {
                Ok(res) => {
                    app.finish_turn(res);
                    turn = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.finish_turn(Err(Error::Closed));
                    turn = None;
                }
            }
        }
        if let Some(rx) = slash_rx.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(value) => {
                            let title = value
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("slash");
                            let body = value.get("body").and_then(Value::as_str).unwrap_or("");
                            let error = value.get("error").and_then(Value::as_str);
                            app.entries.push(Entry::Note(if let Some(error) = error {
                                format!("{title}: {error}")
                            } else {
                                format!("{title}: {body}")
                            }));
                        }
                        Err(error) => app.entries.push(Entry::Note(format!("error: {error}"))),
                    }
                    slash_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    app.entries
                        .push(Entry::Note("slash connection closed".into()));
                    slash_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        if event::poll(Duration::from_millis(50)).map_err(Error::Io)? {
            if let Event::Key(key) = event::read().map_err(Error::Io)? {
                if key.kind == KeyEventKind::Press {
                    handle_key(key, client, app, &mut turn, &mut slash_rx)?;
                }
            }
        }
    }
    Ok(())
}

fn handle_key(
    key: KeyEvent,
    client: &mut Client,
    app: &mut App,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<(), Error> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if app.pending_perm.is_some() {
        match key.code {
            KeyCode::Char('y') => return app.permission_decision("allow", client),
            KeyCode::Char('n') => return app.permission_decision("deny", client),
            KeyCode::Char('a') => return app.permission_decision("always", client),
            _ => return Ok(()),
        }
    }
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if app.busy || turn.is_some() {
                let _ = client.cancel();
            }
            app.quit = true;
        }
        KeyCode::Char('q') if app.input.is_empty() && !app.busy => app.quit = true,
        KeyCode::Esc => {
            if app.busy || turn.is_some() {
                client.cancel()?;
                app.status = "cancelling".into();
            }
        }
        KeyCode::Enter => {
            let text = app.input.trim().to_string();
            if text.starts_with("/steer") {
                let steer_text = text.strip_prefix("/steer").unwrap_or("").trim();
                if app.busy || turn.is_some() {
                    if steer_text.is_empty() {
                        app.status = "steer text must not be empty".into();
                    } else {
                        client.steer(steer_text, Duration::from_secs(20))?;
                        app.status = "steered".into();
                    }
                    app.input.clear();
                }
            } else if text.starts_with('/') {
                handle_slash(&text, client, app, slash_rx)?;
                app.input.clear();
            } else if app.busy || turn.is_some() {
                app.status = "turn in flight (esc cancel · /steer)".into();
                app.input.clear();
            } else if !text.is_empty() {
                app.entries.push(Entry::User(text.clone()));
                app.input.clear();
                app.live.clear();
                app.busy = true;
                app.follow = true;
                app.scroll = u16::MAX;
                app.status = "running".into();
                app.activity_started = Some(Instant::now());
                *turn = Some(client.prompt(&text)?);
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char('u') if ctrl => {
            leave_follow(app, 5);
        }
        KeyCode::Char('d') if ctrl => {
            scroll_down(app, 5);
        }
        KeyCode::Up => {
            leave_follow(app, 1);
        }
        KeyCode::Down => {
            scroll_down(app, 1);
        }
        KeyCode::Char(c) => app.input.push(c),
        _ => {}
    }
    Ok(())
}

fn handle_slash(
    text: &str,
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<(), Error> {
    let mut words = text[1..].split_whitespace();
    let Some(name) = words.next() else {
        return Ok(());
    };
    let args: Vec<String> = words.map(ToString::to_string).collect();
    match name {
        "help" => {
            app.slash_commands = client.slash_list(Duration::from_secs(20))?;
            let body = app
                .slash_commands
                .iter()
                .map(|command| format!("/{} — {}", command.name, command.summary))
                .collect::<Vec<_>>()
                .join(" · ");
            app.entries.push(Entry::Note(body));
        }
        "sessions" => {
            let sessions = client.sessions(Duration::from_secs(20))?;
            let body = sessions
                .iter()
                .map(|session| {
                    format!("{} · {} · {}", session.id, session.updated, session.preview)
                })
                .collect::<Vec<_>>()
                .join(" | ");
            app.entries.push(Entry::Note(if body.is_empty() {
                "no sessions".into()
            } else {
                body
            }));
        }
        "status" => {
            let status = client.status(Duration::from_secs(20))?;
            app.entries.push(Entry::Note(format!("status: {status}")));
        }
        _ => {
            if app.refuses_exclusive_slash(name) {
                app.status = format!("/{name} is unavailable while busy");
            } else {
                *slash_rx = Some(client.slash(name, &args, false)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_header_transcript_input_footer() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.entries.push(Entry::User("hi".into()));
        app.live.push_str("hello");

        let out = render(&mut app, 48, 6);
        assert!(out.contains("mowi"), "{out}");
        assert!(out.contains("gpt-5-mini"), "{out}");
        assert!(out.contains("abcdef01"), "{out}");
        assert!(out.contains("> hi"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("enter send"), "{out}");
    }

    #[test]
    fn token_events_append_live_text() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type":"run.start"}),
        });
        assert!(app.busy);
        for d in ["he", "llo"] {
            app.on_notification(&Notification {
                method: "event".into(),
                params: serde_json::json!({"type":"loop.token","delta":d}),
            });
        }
        assert_eq!(app.live, "hello");

        // Peer chunks never weld onto the host answer.
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type":"harness.delegate.chunk","delta":"peer"}),
        });
        assert_eq!(app.live, "hello");

        app.finish_turn(Ok(serde_json::json!({"text":"hello"})));
        assert!(!app.busy);
        assert_eq!(
            app.entries,
            vec![
                Entry::Assistant("hello".into()),
                Entry::Note("→ peer · done".into())
            ]
        );
        assert!(app.live.is_empty());
    }

    #[test]
    fn tool_events_add_and_complete_tool_line() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "loop.tool.start", "tool": "grep"
            }),
        });
        assert_eq!(
            app.entries,
            vec![Entry::Tool {
                name: "grep".into(),
                duration_ms: None
            }]
        );
        assert_eq!(app.status, "tool · grep");
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "loop.tool.end", "duration_ms": 400
            }),
        });
        assert_eq!(
            app.entries,
            vec![Entry::Tool {
                name: "grep".into(),
                duration_ms: Some(400)
            }]
        );
    }

    #[test]
    fn markdown_and_diff_entries_render() {
        let mut app = App::new(SessionInfo::default());
        app.entries
            .push(Entry::Assistant("**bold** and `code`".into()));
        assert!(
            app.transcript_lines()
                .iter()
                .any(|line| line.spans.len() > 1)
        );
        assert!(is_unified_diff("@@ -1 +1 @@\n+foo\n-bar"));
        app.theme = Theme { colored: false };
        let _ = app.transcript_lines();
    }

    #[test]
    fn failed_turn_keeps_partial_text_and_notes_error() {
        let mut app = App::new(SessionInfo::default());
        app.live.push_str("partial");
        app.finish_turn(Err(Error::Closed));
        assert_eq!(
            app.entries,
            vec![
                Entry::Assistant("partial".into()),
                Entry::Note("error: mow rpc connection closed".into()),
            ]
        );
    }

    #[test]
    fn transcript_seed_maps_roles() {
        let app = App::from_transcript(
            SessionInfo::default(),
            vec![
                TranscriptMessage {
                    role: "user".into(),
                    content: "hi".into(),
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    content: "hello".into(),
                },
                TranscriptMessage {
                    role: "tool".into(),
                    content: "grep".into(),
                },
            ],
        );
        assert_eq!(
            app.entries,
            vec![
                Entry::User("hi".into()),
                Entry::Assistant("hello".into()),
                Entry::Note("tool: grep".into()),
            ]
        );
    }

    #[test]
    fn permission_strip_and_exclusive_slash() {
        let mut app = App::new(SessionInfo::default());
        app.slash_commands.push(SlashCommand {
            name: "review".into(),
            summary: "review changes".into(),
            exclusive: true,
            aliases: vec![],
        });
        app.busy = true;
        assert!(app.refuses_exclusive_slash("review"));
        app.on_notification(&Notification {
            method: "perm.ask".into(),
            params: serde_json::json!({
                "id": "perm-1", "name": "write", "args": {}, "tool_call_id": "call-1"
            }),
        });
        assert_eq!(app.footer(), "y allow · n deny · a always · write");
    }

    #[test]
    fn token_chip_includes_peer_usage() {
        let mut app = App::new(SessionInfo::default());
        app.usage = Usage {
            input_tokens: 10_000,
            output_tokens: 2_300,
            peer_tokens: 1_200,
        };
        assert_eq!(app.usage.chip(), "12.3k tok (⇄ 1.2k)");
        assert!(app.header().contains("12.3k tok"));
    }
}
