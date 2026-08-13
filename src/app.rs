//! Ratatui document: header, transcript, input, footer.
//!
//! The app owns no Engine. Every state change is either a local key edit or a
//! message from the `mow rpc` child (`rpc::Client`).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;
use std::time::Instant;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    execute,
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use serde_json::Value;

use crate::render::{diff_file, is_unified_diff, markdown_lines};
use crate::rpc::{
    Client, Error, Notification, PermissionRequest, SessionInfo, SessionSummary, SlashCommand,
    TranscriptMessage, token_delta,
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

/// Which modal overlay (if any) is painted over the document.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Help,
    Sessions(Vec<SessionSummary>),
    Peer,
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Overlay::None)
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
    /// Transcript pane width, so diff bands can fill it.
    pub last_view_w: u16,
    pub theme: Theme,
    pub activity_started: Option<Instant>,
    pub peers: HashMap<String, String>,
    pub queue: VecDeque<String>,
    pub select_mode: bool,
    pub search_term: String,
    pub search_hits: Vec<usize>,
    pub search_cursor: usize,
    pub last_copy: String,
    /// Ask mode (`perm.set`): true = ask before power tools.
    pub ask_mode: bool,
    pub allow_write: bool,
    pub allow_shell: bool,
    /// Splash for a fresh session; any key dismisses it.
    pub welcome: bool,
    pub overlay: Overlay,
    /// Instant the permission overlay was painted; keys are ignored briefly.
    pub perm_shown: Option<Instant>,
    /// Agent whose buffer `ctrl+p` expands.
    pub peer_focus: Option<String>,
    pub animate: bool,
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
            last_view_w: 80,
            theme: Theme::detect(),
            activity_started: None,
            peers: HashMap::new(),
            queue: VecDeque::new(),
            select_mode: false,
            search_term: String::new(),
            search_hits: Vec::new(),
            search_cursor: 0,
            last_copy: String::new(),
            ask_mode: true,
            allow_write: false,
            allow_shell: false,
            welcome: false,
            overlay: Overlay::None,
            perm_shown: None,
            peer_focus: None,
            animate: std::env::var_os("MOW_NO_ANIM").is_none(),
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

    /// Adopt capability / ask-mode chips from a `status` result.
    pub fn apply_status(&mut self, status: &Value) {
        if let Some(v) = status.get("allow_write").and_then(Value::as_bool) {
            self.allow_write = v;
        }
        if let Some(v) = status.get("allow_shell").and_then(Value::as_bool) {
            self.allow_shell = v;
        }
        match status.get("ask_mode") {
            Some(Value::Bool(v)) => self.ask_mode = *v,
            Some(Value::String(mode)) => self.ask_mode = mode == "ask",
            _ => {}
        }
    }

    /// Capability chip: what the Engine was allowed to do at spawn.
    pub fn capability_chip(&self) -> String {
        match (self.allow_write, self.allow_shell) {
            (true, true) => "write+shell".into(),
            (true, false) => "write".into(),
            (false, true) => "shell".into(),
            (false, false) => "read-only".into(),
        }
    }

    pub fn mode_chip(&self) -> &'static str {
        if self.ask_mode { "ask" } else { "auto" }
    }

    /// Prompt glyph: a plain `>` when colour is off, so it stays legible.
    pub fn prompt_glyph(&self) -> &'static str {
        if self.theme.colored { "❯ " } else { "> " }
    }

    /// Vanity chips, widest-first. These drop on a narrow terminal.
    fn vanity_chips(&self) -> Vec<String> {
        let mut chips = Vec::new();
        if !self.session.workspace.is_empty() {
            chips.push(self.session.workspace.clone());
        }
        if !self.session.model.is_empty() {
            chips.push(self.session.model.clone());
        }
        let short = self.session.short_id();
        if !short.is_empty() {
            chips.push(short);
        }
        if self.usage.total() > 0 || self.usage.peer_tokens > 0 {
            chips.push(self.usage.chip());
        }
        if self.select_mode {
            chips.push("select".into());
        }
        chips
    }

    /// Header as spans. Safety chips (capability, ask/auto) never drop; vanity
    /// chips fall off left-to-right until the row fits `width`.
    pub fn header_line(&self, width: u16) -> Line<'static> {
        let safety = format!("{} · {}", self.capability_chip(), self.mode_chip());
        let mut vanity = self.vanity_chips();
        let width = width as usize;
        loop {
            let left = if vanity.is_empty() {
                "mowi".to_string()
            } else {
                format!("mowi · {}", vanity.join(" · "))
            };
            if left.chars().count() + safety.chars().count() + 2 <= width || vanity.is_empty() {
                let pad = width
                    .saturating_sub(left.chars().count() + safety.chars().count())
                    .max(1);
                return Line::from(vec![
                    Span::styled(left, self.theme.header()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(safety, self.theme.chip()),
                ]);
            }
            vanity.remove(0);
        }
    }

    pub fn footer(&self) -> String {
        let hints = if let Some(permission) = &self.pending_perm {
            return format!("y allow · n deny · a always · {}", permission.name);
        } else {
            "enter send · esc cancel · ? help · ctrl+s select"
        };
        if self.status.is_empty() {
            hints.to_string()
        } else {
            format!("{hints} — {}", self.status)
        }
    }

    /// Transcript as wrapped-source lines (live answer last).
    #[allow(dead_code)]
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
                    self.diff_card(t)
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

    /// A diff entry as a review card: a titled rule, the washed hunk, a close
    /// rule. `last_view_w` is the transcript pane width, so bands are full
    /// rectangles rather than ragged stripes.
    fn diff_card(&self, text: &str) -> Vec<Line<'static>> {
        let width = self.last_view_w.max(8);
        let title = diff_file(text).unwrap_or_else(|| "diff".to_string());
        let head = format!("─ {title} ");
        let fill = (width as usize).saturating_sub(head.chars().count());
        let mut out = vec![Line::styled(
            format!("{head}{}", "─".repeat(fill)),
            self.theme.chrome(),
        )];
        out.extend(crate::render::diff_lines(text, self.theme, width));
        out.push(Line::styled(
            "─".repeat(width as usize),
            self.theme.chrome(),
        ));
        out
    }

    /// Materialize only a bounded transcript window for painting large sessions.
    fn visible_transcript_lines(&self) -> (Vec<Line<'_>>, u16) {
        const OVERSCAN_LINES: usize = 16;
        let total_lines = self.estimated_total_lines();
        let viewport = self.last_view_h.max(1) as usize;
        let target = if self.follow {
            total_lines.saturating_sub(viewport + OVERSCAN_LINES)
        } else {
            (self.scroll as usize).saturating_sub(OVERSCAN_LINES)
        };
        let window_end = target + viewport + (OVERSCAN_LINES * 2);
        let mut lines = Vec::new();
        let mut base = 0usize;
        let mut cursor = 0usize;
        for entry in &self.entries {
            let entry_height = self.estimated_entry_lines(entry);
            let entry_end = cursor + entry_height;
            if entry_end > target && cursor < window_end {
                if lines.is_empty() {
                    base = cursor;
                }
                lines.extend(self.entry_lines(entry));
                lines.push(Line::raw(""));
            }
            cursor = entry_end;
        }
        if !self.live.is_empty() && cursor >= target {
            lines.extend(markdown_lines(self.live.as_str(), self.theme));
        }
        lines.extend(self.peer_lines());
        (lines, base.min(u16::MAX as usize) as u16)
    }

    fn estimated_entry_lines(&self, entry: &Entry) -> usize {
        (match entry {
            Entry::Assistant(text) => text.lines().count().max(1),
            _ => 1,
        }) + 1
    }

    fn estimated_total_lines(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| self.estimated_entry_lines(entry))
            .sum::<usize>()
            + usize::from(!self.live.is_empty())
    }

    pub fn activity(&self) -> String {
        if !self.busy {
            return String::new();
        }
        let elapsed = self
            .activity_started
            .map(|start| start.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        format!(
            "{} {:.1}s · {}",
            self.spinner(elapsed),
            elapsed,
            self.status_or_default()
        )
    }

    /// Spinner frame. `MOW_NO_ANIM=1` pins a static `●`; elapsed still ticks.
    fn spinner(&self, elapsed: f32) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        if !self.animate {
            return "●";
        }
        FRAMES[((elapsed * 8.0) as usize) % FRAMES.len()]
    }

    /// Collapsed peer rows: one line per agent. `ctrl+p` opens the full
    /// buffer in an overlay — peer text never welds onto the host answer.
    fn peer_lines(&self) -> Vec<Line<'static>> {
        let mut agents: Vec<&String> = self.peers.keys().collect();
        agents.sort();
        agents
            .into_iter()
            .map(|agent| {
                let preview = self.peers[agent]
                    .lines()
                    .next_back()
                    .unwrap_or("")
                    .chars()
                    .take(48)
                    .collect::<String>();
                let preview = if preview.is_empty() {
                    "working".to_string()
                } else {
                    preview
                };
                Line::styled(format!("→ {agent} · {preview} (ctrl+p)"), self.theme.note())
            })
            .collect()
    }

    /// Toggle the expanded peer buffer (`ctrl+p`).
    pub fn toggle_peer_expand(&mut self) -> bool {
        if self.peer_focus.take().is_some() {
            return true;
        }
        let mut agents: Vec<&String> = self.peers.keys().collect();
        agents.sort();
        match agents.last() {
            Some(agent) => {
                self.peer_focus = Some((*agent).clone());
                true
            }
            None => {
                self.status = "no peer output".into();
                false
            }
        }
    }

    /// Drop painted entries. Engine history is untouched (`ctrl+l`).
    pub fn clear_transcript(&mut self) {
        self.entries.clear();
        self.live.clear();
        self.search_hits.clear();
        self.search_term.clear();
        self.scroll = 0;
        self.follow = true;
        self.status = "transcript cleared (engine history kept)".into();
    }

    /// Flip ask/auto locally; the caller pushes `perm.set`.
    pub fn toggle_ask_mode(&mut self) -> &'static str {
        self.ask_mode = !self.ask_mode;
        self.mode_chip()
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
                    self.perm_shown = Some(Instant::now());
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
                let tool_name = params
                    .get("tool")
                    .or_else(|| params.get("name"))
                    .and_then(Value::as_str);
                if let Some(Entry::Tool { duration_ms, .. }) =
                    self.entries
                        .iter_mut()
                        .rev()
                        .find(|entry| match (entry, tool_name) {
                            (Entry::Tool { name, .. }, Some(end_name)) => name == end_name,
                            (Entry::Tool { .. }, None) => true,
                            _ => false,
                        })
                {
                    *duration_ms = params.get("duration_ms").and_then(Value::as_u64);
                }
                if tool_name == Some("acp_delegate") {
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
        self.peer_focus = None;
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

    pub fn enqueue_prompt(&mut self, text: String) -> bool {
        if self.queue.len() >= 16 {
            self.status = "queue full".into();
            return false;
        }
        self.queue.push_back(text);
        self.status = format!("queued {}", self.queue.len());
        true
    }

    pub fn next_queued_prompt(&mut self) -> Option<String> {
        let next = self.queue.pop_front();
        if next.is_some() {
            self.status = if self.queue.is_empty() {
                "running".into()
            } else {
                format!("queued {}", self.queue.len())
            };
        }
        next
    }

    pub fn last_user_prompt(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::User(text) => Some(text.clone()),
            _ => None,
        })
    }

    pub fn edit_last_prompt(&mut self) -> bool {
        if let Some(prompt) = self.last_user_prompt() {
            self.input = prompt;
            true
        } else {
            false
        }
    }

    pub fn copy_last_assistant(&mut self) -> bool {
        let text = if !self.live.is_empty() {
            self.live.clone()
        } else {
            self.entries
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    Entry::Assistant(text) => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        if text.is_empty() {
            self.status = "nothing to copy".into();
            return false;
        }
        self.last_copy = text;
        self.status = "copied locally".into();
        true
    }

    pub fn search(&mut self, term: &str) -> Option<(usize, usize)> {
        let term = term.trim();
        if !term.is_empty() && term != self.search_term {
            self.search_term = term.to_string();
            self.search_hits = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry_text(entry)
                        .to_lowercase()
                        .contains(&term.to_lowercase())
                })
                .map(|(index, _)| index)
                .collect();
            self.search_cursor = 0;
        } else if !self.search_hits.is_empty() {
            self.search_cursor = (self.search_cursor + 1) % self.search_hits.len();
        }
        if self.search_hits.is_empty() {
            self.status = "0/0".into();
            return None;
        }
        let hit = self.search_hits[self.search_cursor];
        self.follow = false;
        self.scroll = self.entries[..hit]
            .iter()
            .map(|entry| self.estimated_entry_lines(entry))
            .sum::<usize>()
            .min(u16::MAX as usize) as u16;
        self.status = format!("{}/{}", self.search_cursor + 1, self.search_hits.len());
        Some((self.search_cursor + 1, self.search_hits.len()))
    }

    pub fn toggle_select_mode(&mut self) {
        self.select_mode = !self.select_mode;
    }

    pub fn permission_decision(
        &mut self,
        decision: &str,
        client: &mut Client,
    ) -> Result<(), Error> {
        if let Some(permission) = self.pending_perm.take() {
            self.perm_shown = None;
            self.status = format!("{} {}", permission.name, decision);
            client.perm_decide(&permission.id, decision, Duration::from_secs(20))?;
        }
        Ok(())
    }

    /// True while the freshly painted permission overlay swallows keys, so a
    /// stray keystroke cannot approve a power tool (Go mowi behavior).
    pub fn perm_guard_active(&self) -> bool {
        const GUARD: Duration = Duration::from_millis(200);
        self.perm_shown
            .map(|shown| shown.elapsed() < GUARD)
            .unwrap_or(false)
    }

    /// Local key table plus `slash.list` rows, for the help overlay.
    pub fn help_rows(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = [
            ("enter", "send (queue while busy)"),
            ("ctrl+j", "newline"),
            ("↑ (empty input)", "edit last prompt"),
            ("ctrl+u / ctrl+d", "scroll transcript"),
            ("ctrl+l", "clear transcript (engine history kept)"),
            ("shift+tab", "ask ↔ auto"),
            ("ctrl+p", "expand peer output"),
            ("ctrl+s", "select mode (native copy)"),
            ("ctrl+/ or ?", "this help"),
            ("esc", "dismiss overlay, else cancel turn"),
            ("ctrl+c", "quit"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        for command in &self.slash_commands {
            rows.push((
                format!("/{}", command.name.trim_start_matches('/')),
                command.summary.clone(),
            ));
        }
        rows
    }

    /// Close whichever overlay is open. Returns true if something closed.
    pub fn dismiss_overlay(&mut self) -> bool {
        if self.welcome {
            self.welcome = false;
            return true;
        }
        if self.overlay.is_open() {
            self.overlay = Overlay::None;
            return true;
        }
        false
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

fn entry_text(entry: &Entry) -> String {
    match entry {
        Entry::User(text) | Entry::Assistant(text) | Entry::Note(text) => text.clone(),
        Entry::Tool { name, .. } => name.clone(),
    }
}

fn max_scroll(app: &App) -> u16 {
    let n = app.estimated_total_lines().min(u16::MAX as usize) as u16;
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

/// Centered popup rect using constraint layout (no ad-hoc arithmetic).
fn centered(area: Rect, width: Constraint, height: Constraint) -> Rect {
    let [row] = Layout::vertical([height]).flex(Flex::Center).areas(area);
    let [cell] = Layout::horizontal([width]).flex(Flex::Center).areas(row);
    cell
}

/// Height of the input textarea: grows 1..6 lines with the typed text.
fn input_height(app: &App) -> u16 {
    (app.input.lines().count().max(1) as u16).clamp(1, 6)
}

fn overlay_block(app: &App, title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.chrome())
        .padding(Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), app.theme.accent()))
}

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, app, area);
        return;
    }

    // header · hairline · [activity] · transcript · input · footer
    let mut rows = vec![Constraint::Length(1), Constraint::Length(1)];
    if app.busy {
        rows.push(Constraint::Length(1));
    }
    rows.push(Constraint::Fill(1));
    rows.push(Constraint::Length(input_height(app) + 1));
    rows.push(Constraint::Length(1));
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(rows)
        .split(area);
    let rest = &areas[2..];
    let (activity, transcript_area, input_area, footer_area) = if app.busy {
        (Some(rest[0]), rest[1], rest[2], rest[3])
    } else {
        (None, rest[0], rest[1], rest[2])
    };

    // Header bar: chips on one row, closed by a hairline rule.
    frame.render_widget(
        Paragraph::new(app.header_line(area.width.saturating_sub(2)))
            .block(Block::new().padding(Padding::horizontal(1))),
        areas[0],
    );
    frame.render_widget(
        Block::new()
            .borders(Borders::TOP)
            .border_style(app.theme.chrome()),
        areas[1],
    );

    // The activity band exists only while a turn runs.
    if let Some(band) = activity {
        frame.render_widget(
            Paragraph::new(Line::styled(app.activity(), app.theme.note()))
                .block(Block::new().padding(Padding::horizontal(1))),
            band,
        );
    }

    draw_transcript(frame, app, transcript_area);

    let input_block = Block::new()
        .borders(Borders::TOP)
        .border_style(app.theme.chrome())
        .padding(Padding::horizontal(1));
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);
    frame.render_widget(
        Paragraph::new(prompt_text(app)).wrap(Wrap { trim: false }),
        input_inner,
    );

    // The permission overlay owns the decision hints; keep the footer quiet.
    if app.pending_perm.is_none() {
        frame.render_widget(
            Paragraph::new(app.footer())
                .style(app.theme.note())
                .block(Block::new().padding(Padding::horizontal(1))),
            footer_area,
        );
    }

    if app.welcome {
        draw_welcome(frame, app, area);
        return;
    }
    if app.pending_perm.is_some() {
        draw_permission(frame, app, area);
        return;
    }
    match &app.overlay {
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Sessions(sessions) => draw_sessions(frame, app, sessions, area),
        Overlay::Peer => draw_peer(frame, app, area),
        Overlay::None => {}
    }
}

/// Input text with the prompt glyph on the first line.
fn prompt_text(app: &App) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, line) in app.input.split('\n').enumerate() {
        let glyph = if index == 0 { app.prompt_glyph() } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(glyph, app.theme.accent()),
            Span::styled(line.to_string(), app.theme.user()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            app.prompt_glyph(),
            app.theme.accent(),
        )));
    }
    lines
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Right padding of 2 leaves the last column free for the scrollbar, so a
    // full-width diff band never runs under the thumb.
    let block = Block::new().padding(Padding::new(1, 2, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.last_view_h = inner.height.max(1);
    app.last_view_w = inner.width.max(8);

    let (lines, base_scroll) = app.visible_transcript_lines();
    let height = app.last_view_h as usize;
    let overflow = lines.len().saturating_sub(height);
    let scroll = if app.follow {
        overflow as u16
    } else {
        app.scroll.saturating_sub(base_scroll).min(overflow as u16)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
    if overflow > 0 {
        let mut state = ScrollbarState::new(overflow).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(app.theme.chrome())
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut state,
        );
    }
}

fn draw_too_small(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.warn())
        .title(Span::styled(" mowi ", app.theme.warn()));
    let spot = centered(
        area,
        Constraint::Length(area.width.min(34)),
        Constraint::Length(area.height.min(5)),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("terminal too small", app.theme.warn()),
            Line::styled(
                format!(
                    "need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}",
                    area.width, area.height
                ),
                app.theme.note(),
            ),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(block),
        spot,
    );
}

fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(6).min(52)),
        Constraint::Length(6),
    );
    frame.render_widget(Clear, spot);
    let workspace = if app.session.workspace.is_empty() {
        "workspace".to_string()
    } else {
        app.session.workspace.clone()
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("◇ ", app.theme.accent()),
                Span::styled(workspace, app.theme.assistant()),
            ]),
            Line::from(vec![
                Span::styled(app.session.model.clone(), app.theme.note()),
                Span::raw("  "),
                Span::styled(
                    format!("{} · {}", app.capability_chip(), app.mode_chip()),
                    app.theme.chip(),
                ),
            ]),
            Line::styled("ask anything · ? help · any key to start", app.theme.note()),
        ])
        .wrap(Wrap { trim: true })
        .block(overlay_block(app, "mowi")),
        spot,
    );
}

fn draw_help(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(64)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let items: Vec<ListItem<'static>> = app
        .help_rows()
        .into_iter()
        .map(|(key, what)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{key:<18}"), app.theme.chip()),
                Span::styled(what, app.theme.note()),
            ]))
        })
        .collect();
    frame.render_widget(
        List::new(items).block(overlay_block(app, "help · esc to close")),
        spot,
    );
}

fn draw_sessions(frame: &mut Frame<'_>, app: &App, sessions: &[SessionSummary], area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(72)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let mut items: Vec<ListItem<'static>> = sessions
        .iter()
        .map(|session| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(session.id.clone(), app.theme.chip()),
                    Span::styled(format!("  {}", session.updated), app.theme.note()),
                ]),
                Line::styled(format!("  {}", session.preview), app.theme.context()),
            ])
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "no stored sessions",
            app.theme.note(),
        )));
    }
    items.push(ListItem::new(Line::styled(
        "resume with: mowi --session <id>",
        app.theme.accent(),
    )));
    frame.render_widget(
        List::new(items).block(overlay_block(app, "sessions · esc to close")),
        spot,
    );
}

fn draw_peer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(72)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let agent = app.peer_focus.clone().unwrap_or_else(|| "peer".to_string());
    let body = app.peers.get(&agent).cloned().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(body)
            .style(app.theme.context())
            .wrap(Wrap { trim: false })
            .block(overlay_block(app, &format!("⇄ {agent} · esc to close"))),
        spot,
    );
}

fn draw_permission(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(permission) = &app.pending_perm else {
        return;
    };
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(70)),
        Constraint::Length(area.height.saturating_sub(2).min(14)),
    );
    frame.render_widget(Clear, spot);
    let mut lines = vec![Line::styled(
        format!("▲ {} wants to run", permission.name),
        app.theme.warn(),
    )];
    for line in permission_preview(permission).lines() {
        lines.push(Line::styled(line.to_string(), app.theme.context()));
    }
    // Tool name on the left, the decision keys on the right of the border.
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.warn())
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {} ", permission.name),
            app.theme.warn(),
        ))
        .title(
            Line::styled(" y allow · n deny · a always ", app.theme.chip())
                .alignment(Alignment::Right),
        );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        spot,
    );
}

/// Command string when the tool has one, else pretty-printed args JSON.
fn permission_preview(permission: &PermissionRequest) -> String {
    for field in ["command", "cmd", "path", "file_path"] {
        if let Some(text) = permission.args.get(field).and_then(Value::as_str) {
            return format!("{field}: {text}");
        }
    }
    match &permission.args {
        Value::Null => String::new(),
        args => serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string()),
    }
}

/// Terminal loop: keys in, RPC messages out, one repaint per tick.
pub fn run<B: Backend + Write>(
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
                    if let Some(next) = app.next_queued_prompt() {
                        turn = Some(start_prompt(client, app, &next)?);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.finish_turn(Err(Error::Closed));
                    turn = None;
                    if let Some(next) = app.next_queued_prompt() {
                        turn = Some(start_prompt(client, app, &next)?);
                    }
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
            match event::read().map_err(Error::Io)? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let select_before = app.select_mode;
                    handle_key(key, client, app, &mut turn, &mut slash_rx)?;
                    if select_before != app.select_mode {
                        if app.select_mode {
                            execute!(terminal.backend_mut(), DisableMouseCapture)
                                .map_err(Error::Io)?;
                        } else {
                            execute!(terminal.backend_mut(), EnableMouseCapture)
                                .map_err(Error::Io)?;
                        }
                    }
                }
                Event::Mouse(mouse) if !app.select_mode => match mouse.kind {
                    MouseEventKind::ScrollUp => leave_follow(app, 3),
                    MouseEventKind::ScrollDown => scroll_down(app, 3),
                    _ => {}
                },
                _ => {}
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

    // The permission overlay owns the keyboard, with a short guard window so a
    // stray keystroke in flight cannot approve a power tool.
    if app.pending_perm.is_some() {
        if app.perm_guard_active() {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('y') => return app.permission_decision("allow", client),
            KeyCode::Char('n') | KeyCode::Esc => return app.permission_decision("deny", client),
            KeyCode::Char('a') => return app.permission_decision("always", client),
            _ => return Ok(()),
        }
    }

    // The welcome splash dismisses on any key and swallows it.
    if app.welcome {
        app.welcome = false;
        return Ok(());
    }

    // Overlays: esc (or the toggle key) closes; everything else is inert.
    if app.overlay.is_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.overlay = Overlay::None,
            KeyCode::Char('?') => app.overlay = Overlay::None,
            KeyCode::Char('c') if ctrl => app.quit = true,
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('s') if ctrl => {
            app.toggle_select_mode();
        }
        KeyCode::Char('c') if ctrl => {
            if app.busy || turn.is_some() {
                let _ = client.cancel();
            }
            app.quit = true;
        }
        KeyCode::Char('j') if ctrl => app.input.push('\n'),
        KeyCode::Char('l') if ctrl => app.clear_transcript(),
        KeyCode::Char('p') if ctrl => {
            if app.toggle_peer_expand() {
                app.overlay = if app.peer_focus.is_some() {
                    Overlay::Peer
                } else {
                    Overlay::None
                };
            }
        }
        KeyCode::BackTab => {
            let mode = app.toggle_ask_mode();
            client.perm_set(mode, Duration::from_secs(20))?;
            app.status = format!("mode: {mode}");
        }
        // ctrl+/ arrives as Char('/') with CONTROL on most terminals.
        KeyCode::Char('/') if ctrl => app.overlay = Overlay::Help,
        KeyCode::Char('?') if app.input.is_empty() => app.overlay = Overlay::Help,
        KeyCode::Char('q') if app.input.is_empty() && !app.busy => app.quit = true,
        KeyCode::Esc => {
            if app.dismiss_overlay() {
            } else if app.busy || turn.is_some() {
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
                } else {
                    app.status = "no turn in flight".into();
                }
                app.input.clear();
            } else if text.starts_with('/') {
                handle_slash(&text, client, app, turn, slash_rx)?;
                app.input.clear();
            } else if app.busy || turn.is_some() {
                if !text.is_empty() {
                    app.enqueue_prompt(text);
                }
                app.input.clear();
            } else if !text.is_empty() {
                *turn = Some(start_prompt(client, app, &text)?);
                app.input.clear();
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
            if app.input.is_empty() && app.edit_last_prompt() {
                return Ok(());
            }
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

/// Where a typed `/name` is handled.
///
/// The router exists so local commands can never reach the wire: `/quit` was
/// once forwarded to `mow` as an unknown slash command instead of quitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashRoute {
    /// Quit the UI (cancelling an in-flight turn first).
    Quit,
    /// Handled by the UI; never sent to the host.
    Local,
    /// Forwarded to the host as an RPC `slash` call.
    Rpc,
}

/// Route a slash command name (without the leading `/`).
pub fn slash_route(name: &str) -> SlashRoute {
    match name {
        "quit" | "exit" | "q" => SlashRoute::Quit,
        "search" | "copy" | "edit" | "retry" | "help" | "sessions" | "status" | "steer" => {
            SlashRoute::Local
        }
        _ => SlashRoute::Rpc,
    }
}

fn handle_slash(
    text: &str,
    client: &mut Client,
    app: &mut App,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<(), Error> {
    let mut words = text[1..].split_whitespace();
    let Some(name) = words.next() else {
        return Ok(());
    };
    let args: Vec<String> = words.map(ToString::to_string).collect();
    if slash_route(name) == SlashRoute::Quit {
        // Same shape as ctrl+c: cancel an in-flight turn, then leave.
        if app.busy || turn.is_some() {
            let _ = client.cancel();
        }
        app.quit = true;
        return Ok(());
    }
    match name {
        "search" => {
            app.search(&args.join(" "));
        }
        "copy" => {
            app.copy_last_assistant();
        }
        "edit" => {
            if !app.edit_last_prompt() {
                app.status = "no user prompt to edit".into();
            }
        }
        "retry" => {
            if let Some(prompt) = app.last_user_prompt() {
                if app.busy || turn.is_some() {
                    app.enqueue_prompt(prompt);
                } else {
                    *turn = Some(start_prompt(client, app, &prompt)?);
                }
            } else {
                app.status = "no user prompt to retry".into();
            }
        }
        "help" => {
            app.slash_commands = client.slash_list(Duration::from_secs(20))?;
            app.overlay = Overlay::Help;
        }
        "sessions" => {
            let sessions = client.sessions(Duration::from_secs(20))?;
            app.overlay = Overlay::Sessions(sessions);
        }
        "status" => {
            let status = client.status(Duration::from_secs(20))?;
            app.apply_status(&status);
            app.entries.push(Entry::Note(format!("status: {status}")));
        }
        _ => {
            if slash_route(name) != SlashRoute::Rpc {
                // A local command with no handler here (e.g. `/steer`, taken
                // by the key path) must still never reach the wire.
                app.status = format!("/{name} is handled locally");
            } else if app.refuses_exclusive_slash(name) {
                app.status = format!("/{name} is unavailable while busy");
            } else {
                *slash_rx = Some(client.slash(name, &args, false)?);
            }
        }
    }
    Ok(())
}

fn start_prompt(
    client: &mut Client,
    app: &mut App,
    text: &str,
) -> Result<Receiver<Result<Value, Error>>, Error> {
    app.entries.push(Entry::User(text.to_string()));
    app.live.clear();
    app.busy = true;
    app.follow = true;
    app.scroll = u16::MAX;
    app.status = "running".into();
    app.activity_started = Some(Instant::now());
    client.prompt(text)
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

        let out = render(&mut app, 80, 14);
        assert!(out.contains("mowi"), "{out}");
        assert!(out.contains("gpt-5-mini"), "{out}");
        assert!(out.contains("abcdef01"), "{out}");
        assert!(out.contains("> hi"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("enter send"), "{out}");
        // Safety chips are always painted.
        assert!(out.contains("read-only"), "{out}");
        assert!(out.contains("ask"), "{out}");
    }

    #[test]
    fn narrow_header_drops_vanity_but_keeps_safety_chips() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/very/long/workspace/path".into(),
            model: "claude-sonnet-4".into(),
            wire: "anthropic-messages".into(),
        });
        app.allow_write = true;
        app.allow_shell = true;
        let wide: String = app
            .header_line(120)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(wide.contains("/very/long/workspace/path"), "{wide}");

        let narrow: String = app
            .header_line(42)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(!narrow.contains("/very/long/workspace/path"), "{narrow}");
        assert!(narrow.contains("write+shell"), "{narrow}");
        assert!(narrow.contains("ask"), "{narrow}");
    }

    #[test]
    fn tiny_terminal_paints_a_warning_not_a_broken_frame() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::User("hi".into()));
        let out = render(&mut app, 30, 8);
        assert!(out.contains("too small"), "{out}");
        assert!(!out.contains("> hi"), "{out}");
    }

    #[test]
    fn welcome_splash_paints_and_any_key_dismisses_it() {
        let mut app = App::new(SessionInfo::default());
        app.welcome = true;
        let out = render(&mut app, 60, 16);
        assert!(out.contains("any key to start"), "{out}");

        // handle_key needs a Client; the splash branch is the state machine.
        assert!(app.dismiss_overlay());
        assert!(!app.welcome);
        let out = render(&mut app, 60, 16);
        assert!(!out.contains("any key to start"), "{out}");
    }

    #[test]
    fn help_overlay_lists_local_keys_and_slash_commands() {
        let mut app = App::new(SessionInfo::default());
        app.slash_commands.push(SlashCommand {
            name: "review".into(),
            summary: "review changes".into(),
            exclusive: true,
            aliases: vec![],
        });
        app.overlay = Overlay::Help;
        let out = render(&mut app, 70, 20);
        assert!(out.contains("help"), "{out}");
        assert!(out.contains("ctrl+j"), "{out}");
        assert!(out.contains("/review"), "{out}");

        assert!(app.dismiss_overlay());
        assert_eq!(app.overlay, Overlay::None);
        let out = render(&mut app, 70, 20);
        assert!(!out.contains("ctrl+j"), "{out}");
    }

    #[test]
    fn sessions_overlay_lists_rows_and_resume_hint() {
        let mut app = App::new(SessionInfo::default());
        app.overlay = Overlay::Sessions(vec![SessionSummary {
            id: "s-42".into(),
            updated: "today".into(),
            preview: "port the header".into(),
        }]);
        let out = render(&mut app, 72, 16);
        assert!(out.contains("s-42"), "{out}");
        assert!(out.contains("port the header"), "{out}");
        assert!(out.contains("mowi --session <id>"), "{out}");
    }

    #[test]
    fn permission_overlay_shows_args_and_guards_early_keys() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "perm.ask".into(),
            params: serde_json::json!({
                "id": "perm-1",
                "name": "bash",
                "args": {"command": "rm -rf build"},
                "tool_call_id": "call-1"
            }),
        });
        assert!(app.perm_guard_active(), "guard should swallow stray keys");
        let out = render(&mut app, 70, 16);
        // Tool name titles the block; decisions sit on the title-right.
        assert!(out.contains("bash"), "{out}");
        assert!(out.contains("build"), "{out}");
        assert!(out.contains("y allow"), "{out}");
        // The footer stays quiet while the overlay owns the decision.
        assert!(!out.contains("enter send"), "{out}");
    }

    #[test]
    fn ctrl_j_grows_the_input_area() {
        let mut app = App::new(SessionInfo::default());
        assert_eq!(input_height(&app), 1);
        app.input.push_str("one");
        app.input.push('\n');
        app.input.push_str("two");
        assert_eq!(input_height(&app), 2);
        let out = render(&mut app, 60, 16);
        assert!(out.contains("❯ one"), "{out}");
        assert!(out.contains("  two"), "{out}");
    }

    #[test]
    fn ask_mode_flips_and_clear_keeps_engine_history() {
        let mut app = App::new(SessionInfo::default());
        assert_eq!(app.mode_chip(), "ask");
        assert_eq!(app.toggle_ask_mode(), "auto");
        assert!(!app.ask_mode);
        assert_eq!(app.toggle_ask_mode(), "ask");

        app.entries.push(Entry::User("hi".into()));
        app.live.push_str("partial");
        app.clear_transcript();
        assert!(app.entries.is_empty());
        assert!(app.live.is_empty());
        assert!(app.status.contains("engine history"), "{}", app.status);
    }

    #[test]
    fn peer_output_collapses_and_expands() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "harness.delegate.chunk", "agent": "peer-agent", "delta": "scanning\n"
            }),
        });
        let out = render(&mut app, 70, 16);
        assert!(out.contains("→ peer-agent"), "{out}");

        assert!(app.toggle_peer_expand());
        assert_eq!(app.peer_focus.as_deref(), Some("peer-agent"));
        assert!(app.toggle_peer_expand());
        assert!(app.peer_focus.is_none());
    }

    #[test]
    fn status_result_updates_capability_chips() {
        let mut app = App::new(SessionInfo::default());
        app.apply_status(&serde_json::json!({
            "allow_write": true, "allow_shell": false, "ask_mode": "auto"
        }));
        assert_eq!(app.capability_chip(), "write");
        assert_eq!(app.mode_chip(), "auto");
    }

    #[test]
    fn static_spinner_when_animation_is_off() {
        let mut app = App::new(SessionInfo::default());
        app.animate = false;
        app.busy = true;
        app.activity_started = Some(Instant::now());
        assert!(app.activity().starts_with('●'), "{}", app.activity());
    }

    #[test]
    fn quit_commands_are_local_and_never_reach_the_wire() {
        for name in ["quit", "exit", "q"] {
            assert_eq!(slash_route(name), SlashRoute::Quit, "/{name}");
        }
        // Everything the UI answers itself stays off the wire too.
        for name in [
            "help", "search", "copy", "retry", "edit", "steer", "sessions", "status",
        ] {
            assert_eq!(slash_route(name), SlashRoute::Local, "/{name}");
        }
        // Pack commands are forwarded to the host.
        for name in ["review", "sec", "goal"] {
            assert_eq!(slash_route(name), SlashRoute::Rpc, "/{name}");
        }
    }

    #[test]
    fn diff_card_bands_fill_the_transcript_width() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::Assistant(
            "--- a/src/app.rs\n+++ b/src/app.rs\n@@ -1 +1 @@\n-let x = 1;\n+let x = 2;".into(),
        ));
        let out = render(&mut app, 60, 16);
        // The card is titled with the parsed file path.
        assert!(out.contains("─ src/app.rs"), "{out}");
        // Signs use U+2212 minus, not hyphen-minus.
        assert!(out.contains('−'), "{out}");
        // Bands reach the pane width: the row after the sign is not ragged.
        let band_row = out
            .lines()
            .find(|row| row.contains("let x = 2;"))
            .expect("add band");
        assert!(band_row.trim_end().ends_with(';') || band_row.ends_with(' '));
    }

    #[test]
    fn busy_activity_row_does_not_shrink_transcript_height() {
        let mut app = App::new(SessionInfo::default());
        app.busy = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.last_view_h > 10, "height was {}", app.last_view_h);
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
        assert!(
            app.header_line(80)
                .spans
                .iter()
                .any(|span| span.content.contains("12.3k tok"))
        );
    }

    #[test]
    fn queue_is_capped_and_drains_in_order() {
        let mut app = App::new(SessionInfo::default());
        for index in 0..16 {
            assert!(app.enqueue_prompt(format!("prompt-{index}")));
        }
        assert!(!app.enqueue_prompt("overflow".into()));
        assert_eq!(app.queue.len(), 16);
        assert_eq!(app.next_queued_prompt().as_deref(), Some("prompt-0"));
        assert_eq!(app.queue.len(), 15);
    }

    #[test]
    fn search_cycles_and_edit_remembers_last_user() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::User("find this".into()));
        app.entries.push(Entry::Assistant("find that".into()));
        app.entries.push(Entry::Note("other".into()));
        assert_eq!(app.search("find"), Some((1, 2)));
        assert_eq!(app.search(""), Some((2, 2)));
        assert!(app.edit_last_prompt());
        assert_eq!(app.input, "find this");
        assert_eq!(app.last_user_prompt().as_deref(), Some("find this"));
    }

    #[test]
    fn select_mode_and_copy_state_are_local() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::Assistant("answer".into()));
        app.toggle_select_mode();
        assert!(app.select_mode);
        assert!(app.copy_last_assistant());
        assert_eq!(app.last_copy, "answer");
    }
}
