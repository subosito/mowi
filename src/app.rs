//! Ratatui document: header, transcript, input, footer.
//!
//! The app owns no Engine. Every state change is either a local key edit or a
//! message from the `mow rpc` child (`rpc::Client`).

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use serde_json::Value;

use crate::rpc::{Client, Error, Notification, SessionInfo, token_delta};

/// One painted transcript block.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Note(String),
}

/// UI state. `draw` is pure over this struct so `TestBackend` can assert on it.
#[derive(Debug, Default)]
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
}

impl App {
    pub fn new(session: SessionInfo) -> App {
        App {
            session,
            follow: true,
            ..App::default()
        }
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
        s
    }

    pub fn footer(&self) -> String {
        let hints = "enter send · esc cancel · q quit";
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
            match e {
                Entry::User(t) => out.push(Line::from(vec![
                    Span::styled("> ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(t.as_str()),
                ])),
                Entry::Assistant(t) => out.push(Line::raw(t.as_str())),
                Entry::Note(t) => out.push(Line::styled(
                    t.as_str(),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            }
            out.push(Line::raw(""));
        }
        if !self.live.is_empty() {
            out.push(Line::raw(self.live.as_str()));
        }
        out
    }

    /// Apply an `event` / `perm.ask` notification.
    pub fn on_notification(&mut self, n: &Notification) {
        match n.method.as_str() {
            "event" => self.on_event(&n.params),
            "perm.ask" => {
                let name = n
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool");
                // Phase 1 owns the y/n/a strip; for now say so plainly.
                self.status = format!("{name} needs permission (not wired yet — esc to cancel)");
            }
            _ => {}
        }
    }

    fn on_event(&mut self, params: &Value) {
        let kind = params.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "run.start" => {
                self.busy = true;
                self.status = "running".into();
            }
            "run.end" => {
                self.busy = false;
                self.status.clear();
            }
            "tool.start" => {
                if let Some(name) = params.get("name").and_then(|v| v.as_str()) {
                    self.status = format!("tool · {name}");
                }
            }
            _ => {}
        }
        if let Some(d) = token_delta(params) {
            self.live.push_str(d);
            if self.follow {
                self.scroll = u16::MAX;
            }
        }
    }

    /// Finish the turn: commit live text (or the final `prompt` result).
    pub fn finish_turn(&mut self, result: Result<Value, Error>) {
        self.busy = false;
        match result {
            Ok(v) => {
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
        if self.follow {
            self.scroll = u16::MAX;
        }
    }
}

/// Paint header / transcript / input / footer.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(app.header()).style(Style::default().add_modifier(Modifier::BOLD)),
        areas[0],
    );

    let lines = app.transcript_lines();
    let height = areas[1].height.max(1) as usize;
    let scroll = if app.follow || app.scroll == u16::MAX {
        lines.len().saturating_sub(height) as u16
    } else {
        app.scroll
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        areas[1],
    );

    frame.render_widget(Paragraph::new(format!("› {}", app.input)), areas[2]);
    frame.render_widget(
        Paragraph::new(app.footer()).style(Style::default().add_modifier(Modifier::DIM)),
        areas[3],
    );
}

/// Terminal loop: keys in, RPC messages out, one repaint per tick.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &mut Client,
    app: &mut App,
) -> Result<(), Error> {
    let mut turn: Option<Receiver<Result<Value, Error>>> = None;

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

        if event::poll(Duration::from_millis(50)).map_err(Error::Io)? {
            if let Event::Key(key) = event::read().map_err(Error::Io)? {
                if key.kind == KeyEventKind::Press {
                    handle_key(key, client, app, &mut turn)?;
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
) -> Result<(), Error> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
            if !text.is_empty() && turn.is_none() {
                app.entries.push(Entry::User(text.clone()));
                app.input.clear();
                app.live.clear();
                app.busy = true;
                app.follow = true;
                app.scroll = u16::MAX;
                app.status = "running".into();
                *turn = Some(client.prompt(&text)?);
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Up => {
            app.follow = false;
            app.scroll = app.scroll.saturating_sub(1);
        }
        KeyCode::Down => {
            app.scroll = app.scroll.saturating_add(1);
        }
        KeyCode::Char(c) => app.input.push(c),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &App, w: u16, h: u16) -> String {
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

        let out = render(&app, 48, 6);
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
        assert_eq!(app.entries, vec![Entry::Assistant("hello".into())]);
        assert!(app.live.is_empty());
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
}
