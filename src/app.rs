//! Ratatui document: header, transcript, input, footer.
//!
//! The app owns no Engine. Every state change is either a local key edit or a
//! message from the `mow rpc` child (`rpc::Client`).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;
use std::time::Instant;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Flex, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row,
        Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
    },
};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::render::{Segment, diff_title, markdown_lines, split_markdown_and_diffs};
use crate::rpc::{
    Client, ContextUsage, EffortList, Error, ModelList, Notification, PermissionRequest,
    SessionInfo, SessionSummary, SlashCommand, TranscriptMessage, token_delta,
};
use crate::slash::{
    SlashRoute, canonical_slash, slash_completions, slash_route, unknown_slash_message,
};
use crate::theme::{SPINNER, SPINNER_STATIC, TYPING, Theme, Tone};

/// Display columns allowed for a collapsed peer preview.
const PEER_PREVIEW: usize = 48;
/// Display columns allowed for the argument half of a tool row label.
const TOOL_ARG_COLS: usize = 56;
/// Cells in the header context meter.
const CTX_CELLS: usize = 5;
/// Below this context percentage the footer stays quiet — the header gauge is
/// enough, and a number that is always on screen stops being read.
const CTX_FOOTER_PCT: f64 = 60.0;
/// Terminals narrower than this drop the header gauge before they drop the
/// session identity: knowing *which* model you are talking to outranks knowing
/// how full its window is.
const GAUGE_MIN_COLS: u16 = 100;

/// Compact token count for status text: 950 -> "950", 12_300 -> "12.3k".
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let k = n as f64 / 1000.0;
    if k < 100.0 {
        format!("{k:.1}k")
    } else {
        format!("{k:.0}k")
    }
}

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
    /// One turn's worth of tool calls, collapsed to a single summary line
    /// unless expanded (`t` on empty input). A long agent turn that runs
    /// `bash`/`write`/`read` a dozen times is one row, not a flood.
    Tools {
        tools: Vec<(String, Option<u64>)>,
        expanded: bool,
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
    Help(TableState),
    Sessions {
        items: Vec<SessionSummary>,
        state: ListState,
    },
    Models {
        list: ModelList,
        state: ListState,
    },
    Efforts {
        list: EffortList,
        state: ListState,
    },
    Completions {
        items: Vec<String>,
        state: ListState,
    },
    Peer,
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Overlay::None)
    }

    pub(crate) fn help() -> Self {
        let mut state = TableState::default();
        state.select(Some(0));
        Overlay::Help(state)
    }

    fn sessions(items: Vec<SessionSummary>) -> Self {
        Overlay::Sessions {
            state: select_list(items.len(), 0),
            items,
        }
    }

    fn models(list: ModelList) -> Self {
        let idx = list
            .models
            .iter()
            .position(|model| model.current || model.id == list.current)
            .unwrap_or(0);
        Overlay::Models {
            state: select_list(list.models.len(), idx),
            list,
        }
    }

    fn efforts(list: EffortList) -> Self {
        let idx = list
            .efforts
            .iter()
            .position(|effort| effort.current || effort.id == list.current)
            .unwrap_or(0);
        Overlay::Efforts {
            state: select_list(list.efforts.len(), idx),
            list,
        }
    }

    fn completions(items: Vec<String>) -> Self {
        Overlay::Completions {
            state: select_list(items.len(), 0),
            items,
        }
    }
}

fn select_list(len: usize, idx: usize) -> ListState {
    let mut state = ListState::default();
    if len > 0 {
        state.select(Some(idx.min(len - 1)));
    }
    state
}

fn step_list(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0).min(len - 1) as i32;
    let next = (cur + delta).clamp(0, (len as i32) - 1) as usize;
    state.select(Some(next));
}

fn step_table(state: &mut TableState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0).min(len - 1) as i32;
    let next = (cur + delta).clamp(0, (len as i32) - 1) as usize;
    state.select(Some(next));
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
    pub search_term: String,
    pub search_hits: Vec<usize>,
    pub search_cursor: usize,
    pub last_copy: String,
    /// Tool calls of the turn currently running, grouped so a busy turn does
    /// not paint one transcript row per `bash`/`write`/`read` call. Committed
    /// to `entries` as `Entry::Tools` when the turn's loop ends (or the turn
    /// finishes), then cleared.
    pub live_tools: Vec<(String, Option<u64>)>,
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
    /// Reasoning effort from `effort.list` / `effort.set`.
    pub effort: String,
    /// Latest `context` result, for the gauge and /context.
    pub ctx: Option<ContextUsage>,
    /// Methods the connected server advertises (empty = assume everything).
    pub caps: Vec<String>,
    /// Character index into `input`.
    pub cursor: usize,
    /// Ratatui frame counter, used for the busy spinner.
    pub tick: u64,
    pub scrollbar_state: ScrollbarState,
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
            search_term: String::new(),
            search_hits: Vec::new(),
            search_cursor: 0,
            last_copy: String::new(),
            live_tools: Vec::new(),
            ask_mode: true,
            allow_write: false,
            allow_shell: false,
            welcome: false,
            overlay: Overlay::None,
            perm_shown: None,
            peer_focus: None,
            animate: std::env::var_os("MOW_NO_ANIM").is_none(),
            effort: String::new(),
            ctx: None,
            caps: Vec::new(),
            cursor: 0,
            tick: 0,
            scrollbar_state: ScrollbarState::default(),
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
        if let Some(model) = status.get("model").and_then(Value::as_str)
            && !model.is_empty()
        {
            self.session.model = model.to_string();
        }
        if let Some(effort) = status.get("effort").and_then(Value::as_str) {
            self.effort = effort.to_string();
        }
    }

    /// Show the model catalog overlay and keep the header chip in sync.
    pub fn apply_model_list(&mut self, list: ModelList) {
        if !list.current.is_empty() {
            self.session.model = list.current.clone();
        }
        self.overlay = Overlay::models(list);
    }

    /// Apply a `model.set` result to the header chip.
    pub fn apply_model_set(&mut self, model: &str) {
        if model.is_empty() {
            return;
        }
        self.session.model = model.to_string();
        self.status = format!("model: {model}");
        self.entries.push(Entry::Note(format!("model: {model}")));
        self.overlay = Overlay::None;
    }

    /// Show the effort picker overlay.
    pub fn apply_effort_list(&mut self, list: EffortList) {
        if !list.current.is_empty() {
            self.effort = list.current.clone();
        }
        self.overlay = Overlay::efforts(list);
    }

    /// Apply an `effort.set` result to the status bar.
    pub fn apply_effort_set(&mut self, effort: &str) {
        if effort.is_empty() {
            return;
        }
        self.effort = effort.to_string();
        self.status = format!("effort: {effort}");
        self.entries.push(Entry::Note(format!("effort: {effort}")));
        self.overlay = Overlay::None;
    }

    /// Record the server's advertised method surface for feature detection.
    pub fn set_capabilities(&mut self, methods: &[String]) {
        self.caps = methods.to_vec();
    }

    /// Feature-detect a method; an empty list (older server) means assume yes.
    pub fn supports(&self, method: &str) -> bool {
        self.caps.is_empty() || self.caps.iter().any(|m| m == method)
    }

    /// Store a `context` result for the gauge.
    pub fn apply_context(&mut self, usage: &ContextUsage) {
        self.ctx = Some(usage.clone());
    }

    /// One-line context summary, e.g. "context: 12.3k / 200k (6%)".
    pub fn context_summary(&self) -> String {
        let Some(ctx) = &self.ctx else {
            return "context: unknown".into();
        };
        match (ctx.context_window, ctx.percent) {
            (Some(window), Some(pct)) => format!(
                "context: {} / {} ({:.0}%)",
                human_tokens(ctx.tokens),
                human_tokens(window),
                pct
            ),
            _ => format!("context: {} tokens", human_tokens(ctx.tokens)),
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

    /// Compact context meter for the header, e.g. `▰▰▰▱▱ 58%`.
    ///
    /// Returns `None` until the Engine reports a window, so the chip never
    /// shows a fake zero.
    pub fn context_chip(&self) -> Option<String> {
        let ctx = self.ctx.as_ref()?;
        let pct = match (ctx.context_window, ctx.percent) {
            (Some(_), Some(pct)) => pct,
            (Some(window), None) if window > 0 => (ctx.tokens as f64 / window as f64) * 100.0,
            _ => return None,
        }
        .clamp(0.0, 100.0);
        let filled = ((pct / 100.0) * CTX_CELLS as f64).round() as usize;
        Some(format!(
            "{}{} {:.0}%",
            "▰".repeat(filled),
            "▱".repeat(CTX_CELLS.saturating_sub(filled)),
            pct
        ))
    }

    /// Severity of context pressure, so the meter warns before it truncates.
    pub fn context_tone(&self) -> Tone {
        let pct = self.ctx.as_ref().and_then(|c| c.percent).unwrap_or(0.0);
        if pct >= 90.0 {
            Tone::Error
        } else if pct >= 75.0 {
            Tone::Warn
        } else {
            Tone::Muted
        }
    }

    /// Vanity chips in **drop order**: least important first.
    ///
    /// The drop loop peels from the front, so this list is ordered by what a
    /// developer can most afford to lose. Token usage and the session id are
    /// trivia; the model is identity — "which brain am I talking to" is the
    /// last thing to go, because answering it wrong is how you ship a patch
    /// written by the wrong model.
    ///
    /// `width` also decides whether the context gauge is offered at all: below
    /// `GAUGE_MIN_COLS` it is suppressed up front, so the remaining columns go
    /// to identity rather than a meter.
    fn vanity_chips(&self, width: u16) -> Vec<String> {
        let mut chips = Vec::new();
        if self.usage.total() > 0 || self.usage.peer_tokens > 0 {
            chips.push(self.usage.chip());
        }
        let short = self.session.short_id();
        if !short.is_empty() {
            chips.push(short);
        }
        if !self.session.workspace.is_empty() {
            chips.push(self.session.workspace.clone());
        }
        if !self.effort.is_empty() {
            chips.push(self.effort.clone());
        }
        if !self.session.model.is_empty() {
            chips.push(self.session.model.clone());
        }
        // Context pressure sits closest to the safety chips: it is the last
        // vanity chip to drop, because it is the one that predicts trouble.
        if width >= GAUGE_MIN_COLS
            && let Some(ctx) = self.context_chip()
        {
            chips.push(ctx);
        }
        chips
    }

    /// Header as spans. Safety chips (capability, ask/auto) never drop; vanity
    /// chips fall off left-to-right until the row fits `width`.
    pub fn header_line(&self, width: u16) -> Line<'static> {
        let safety = format!("{} · {}", self.capability_chip(), self.mode_chip());
        let mut vanity = self.vanity_chips(width);
        let width = width as usize;
        // The context meter is styled by pressure, not by the vanity style.
        let ctx_chip = self.context_chip();
        loop {
            let left = if vanity.is_empty() {
                "mowi".to_string()
            } else {
                format!("mowi · {}", vanity.join(" · "))
            };
            let left_w = Span::raw(left.as_str()).width();
            let safety_w = Span::raw(safety.as_str()).width();
            if left_w + safety_w < width || vanity.is_empty() {
                let pad = width.saturating_sub(left_w + safety_w);
                // Split the meter off the tail so it can carry its own tone.
                let ctx_tail = ctx_chip
                    .as_ref()
                    .filter(|chip| left.ends_with(chip.as_str()))
                    .map(|chip| left.len() - chip.len());
                let mut spans = Vec::new();
                match ctx_tail {
                    Some(at) => {
                        spans.push(Span::styled(left[..at].to_string(), self.theme.header()));
                        spans.push(Span::styled(
                            left[at..].to_string(),
                            self.theme
                                .badge(self.context_tone())
                                .patch(self.theme.header_bg()),
                        ));
                    }
                    None => spans.push(Span::styled(left, self.theme.header())),
                }
                spans.push(Span::styled(" ".repeat(pad), self.theme.header_bg()));
                spans.push(Span::styled(
                    safety,
                    self.theme.chip().patch(self.theme.header_bg()),
                ));
                return Line::from(spans);
            }
            vanity.remove(0);
        }
    }

    /// Plain-text footer (styling dropped) — used by tests to assert content
    /// without pinning spans.
    #[allow(dead_code)]
    pub fn footer(&self) -> String {
        self.footer_line(120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Status footer, laid out as a real status bar: live state on the left,
    /// key hints flushed right, and the gap between them filled so the row is
    /// one continuous surface instead of a ragged sentence.
    ///
    /// The hints are the first thing to go when the terminal is narrow — a
    /// developer who has run out of columns still needs to see whether the
    /// turn is running, not to be reminded that `enter` sends.
    pub fn footer_line(&self, width: u16) -> Line<'static> {
        if let Some(permission) = &self.pending_perm {
            return decision_line(self.theme, width, Some(&permission.name));
        }

        let mut left: Vec<Span<'static>> = Vec::new();
        let sep = |spans: &mut Vec<Span<'static>>| {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", self.theme.chrome()));
            }
        };

        // The activity band above the composer owns the live turn readout
        // (spinner, elapsed, current tool). Repeating it here would give the
        // operator two clocks that tick out of step, so the footer carries
        // only the coarse state word.
        if self.busy {
            left.push(Span::styled("● ", self.theme.badge(Tone::Active)));
            left.push(Span::styled("busy", self.theme.badge(Tone::Active)));
        } else {
            left.push(Span::styled("● ", self.theme.badge(Tone::Ok)));
            left.push(Span::styled("idle", self.theme.note()));
        }

        if !self.queue.is_empty() {
            sep(&mut left);
            left.push(Span::styled(
                format!("{} queued", self.queue.len()),
                self.theme.badge(Tone::Warn),
            ));
        }
        // Status text is only news when the band is not already showing it.
        if !self.status.is_empty() && !self.busy {
            sep(&mut left);
            let tone = if self.peers.is_empty() {
                self.theme.note()
            } else {
                self.theme.peer()
            };
            left.push(Span::styled(clip_display(&self.status, 40), tone));
        }
        // Context pressure earns a footer slot only once it starts to matter:
        // the header gauge covers the normal case, and a percentage that is
        // always on screen stops being read.
        if let Some(ctx) = &self.ctx
            && let Some(pct) = ctx.percent
            && pct >= CTX_FOOTER_PCT
        {
            sep(&mut left);
            let style = if pct >= 85.0 {
                self.theme.warn()
            } else {
                self.theme.note()
            };
            left.push(Span::styled(format!("ctx {pct:.0}%"), style));
        }

        let hints = self.footer_hints();
        let left_w: usize = left.iter().map(Span::width).sum();
        let width = width as usize;
        let mut spans = left;
        for hint in hints {
            let hint_w = Span::raw(hint.as_str()).width();
            // +2 keeps a breathing gap between state and hints.
            if left_w + hint_w + 2 > width {
                continue;
            }
            let pad = width.saturating_sub(left_w + hint_w);
            spans.push(Span::styled(" ".repeat(pad), self.theme.chrome()));
            spans.push(Span::styled(hint, self.theme.note()));
            break;
        }
        Line::from(spans)
    }

    /// Key hints, widest first: the widest one that still fits is painted.
    fn footer_hints(&self) -> Vec<String> {
        vec![
            "enter send · esc cancel · ctrl+u/d scroll · ? help".to_string(),
            "enter send · esc cancel · ? help".to_string(),
            "enter · esc · ?".to_string(),
            "?".to_string(),
        ]
    }

    /// Wall-clock for the running turn, e.g. `4.2s` / `1m03s`.
    pub fn elapsed(&self) -> Option<String> {
        let secs = self.activity_started?.elapsed().as_secs_f64();
        Some(if secs >= 60.0 {
            format!("{}m{:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
        } else {
            format!("{secs:.1}s")
        })
    }

    /// Typing-indicator frame, frozen when animation is off.
    pub fn typing_frame(&self) -> &'static str {
        if self.animate {
            TYPING[(self.tick as usize / 3) % TYPING.len()]
        } else {
            TYPING[2]
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
            out.extend(self.assistant_lines(self.live.as_str()));
        }
        out
    }

    fn entry_lines<'a>(&self, entry: &'a Entry) -> Vec<Line<'a>> {
        let width = self.last_view_w.max(8) as usize;
        match entry {
            Entry::User(t) => self.user_lines(t),
            Entry::Assistant(t) => self.assistant_lines(t),
            Entry::Note(t) => {
                wrap_styled_line(Line::styled(t.to_string(), self.theme.note()), width)
            }
            Entry::Tool { name, duration_ms } => {
                // One glyph, not two: state is carried by colour and shape.
                // A running tool gets the spinner so a stall is visible.
                let (glyph, glyph_style) = match duration_ms {
                    Some(_) => ("✓", self.theme.badge(Tone::Ok).patch(self.theme.base())),
                    None => (self.spinner_frame(), self.theme.spinner()),
                };
                // Label as `verb · argument`, never a raw shell blob: a
                // chained command must not become four lines of transcript.
                let (verb, rest) = tool_label(name);
                let mut spans = vec![
                    Span::styled(format!("{glyph} "), glyph_style),
                    Span::styled(verb, self.theme.tool()),
                ];
                if !rest.is_empty() {
                    spans.push(Span::styled(format!(" {rest}"), self.theme.note()));
                }
                match duration_ms {
                    // The spec keeps sub-second timings: "0.4s" is how an
                    // operator tells a cached read from a real one.
                    Some(ms) => spans.push(Span::styled(
                        format!("  {:.1}s", *ms as f64 / 1000.0),
                        self.theme.timing(),
                    )),
                    None => spans.push(Span::styled("  running", self.theme.timing())),
                }
                wrap_styled_line(Line::from(spans), width)
            }
            Entry::Tools { tools, expanded } => {
                // Collapsed (the default): one summary row so a busy turn
                // stays readable. Expanded: a header plus one row per call,
                // same visual language as a plain Tool entry.
                if tools.len() <= 1 || *expanded {
                    let mut out = wrap_styled_line(
                        Line::styled(format!("⚙ {} tool calls", tools.len()), self.theme.tool()),
                        width,
                    );
                    for (name, duration_ms) in tools {
                        let (verb, rest) = tool_label(name);
                        let mut spans = vec![
                            Span::styled("✓ ", self.theme.badge(Tone::Ok).patch(self.theme.base())),
                            Span::styled(verb, self.theme.tool()),
                        ];
                        if !rest.is_empty() {
                            spans.push(Span::styled(format!(" {rest}"), self.theme.note()));
                        }
                        if let Some(ms) = duration_ms {
                            spans.push(Span::styled(
                                format!("  {:.1}s", *ms as f64 / 1000.0),
                                self.theme.timing(),
                            ));
                        }
                        out.extend(wrap_styled_line(Line::from(spans), width));
                    }
                    out
                } else {
                    let total_ms: u64 = tools.iter().filter_map(|(_, d)| *d).sum();
                    let mut text = format!("⚙ {} tool calls", tools.len());
                    if total_ms > 0 {
                        text.push_str(&format!(" · {:.1}s", total_ms as f64 / 1000.0));
                    }
                    wrap_styled_line(Line::styled(text, self.theme.note()), width)
                }
            }
        }
    }

    /// User prompt as a full-width filled band: accent bar, padded body, blank
    /// rows above and below so it reads as a message, not a leftover prompt.
    fn user_lines(&self, text: &str) -> Vec<Line<'static>> {
        let width = self.last_view_w.max(8) as usize;
        let blank = self.user_band_row("", width);
        let mut out = vec![blank.clone()];
        for raw in text.split('\n') {
            let inner = width.saturating_sub(3).max(1); // accent + gutters
            for chunk in wrap_cols(raw, inner) {
                out.push(self.user_band_row(&chunk, width));
            }
        }
        out.push(blank);
        out
    }

    fn user_band_row(&self, body: &str, width: usize) -> Line<'static> {
        let mut spans = Vec::new();
        // A saturated one-column rail on the left edge. This is the only
        // full-height accent in the transcript, so scanning up the pane the
        // eye can find "where did I last speak" without reading a word.
        if self.theme.colored {
            spans.push(Span::styled("▎", self.theme.user_rail()));
        }
        let used: usize = spans.iter().map(Span::width).sum();
        let room = width.saturating_sub(used);
        let text = if body.is_empty() {
            String::new()
        } else {
            format!(" {body}")
        };
        let text: String = text.chars().take(room).collect();
        spans.push(Span::styled(text, self.theme.user()));
        let painted: usize = spans.iter().map(Span::width).sum();
        let pad = width.saturating_sub(painted);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), self.theme.user()));
        }
        Line::from(spans).style(self.theme.user())
    }

    /// Markdown by default; only fenced or bare unified-diff bodies become cards.
    fn assistant_lines(&self, text: &str) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for segment in split_markdown_and_diffs(text) {
            if !out.is_empty() {
                out.push(Line::raw(""));
            }
            match segment {
                Segment::Md(md) => {
                    let width = self.last_view_w.max(8) as usize;
                    for line in markdown_lines(&md, self.theme) {
                        out.extend(wrap_styled_line(line, width));
                    }
                }
                Segment::Diff(diff) => out.extend(self.diff_card(&diff)),
            }
        }
        if out.is_empty() {
            out.push(Line::raw(""));
        }
        out
    }

    /// A diff entry as a review card: a titled rule, the washed hunk, a close
    /// rule. `last_view_w` is the transcript pane width, so bands are full
    /// rectangles rather than ragged stripes.
    fn diff_card(&self, text: &str) -> Vec<Line<'static>> {
        let width = self.last_view_w.max(8);
        let title = diff_title(text);
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
            lines.extend(self.assistant_lines(self.live.as_str()));
        }
        lines.extend(self.peer_lines());
        (lines, base.min(u16::MAX as usize) as u16)
    }

    /// Height an entry will occupy once painted, in transcript rows.
    ///
    /// This must never *under*-report. `visible_transcript_lines` slices the
    /// document with these numbers, and the scrollbar derives its extent from
    /// them; if an entry paints taller than it claims, the window slides and
    /// content the operator was reading — typically their own prompt — is
    /// pushed off the top of the pane.
    ///
    /// So the count is by wrapped rows, not logical lines. A tool row whose
    /// name is a whole shell blob (`bash echo ---; cat …; ls -la`) is the case
    /// that made this matter: it reported 1 row and painted four.
    fn estimated_entry_lines(&self, entry: &Entry) -> usize {
        let width = self.last_view_w.max(8) as usize;
        // Rows a block of text needs once hard-wrapped into `inner` columns.
        let wrapped = |text: &str, inner: usize| -> usize {
            let inner = inner.max(1);
            text.split('\n')
                .map(|line| {
                    let cols = line.width();
                    if cols == 0 { 1 } else { cols.div_ceil(inner) }
                })
                .sum::<usize>()
                .max(1)
        };
        (match entry {
            // The user band reserves the accent rail plus its gutters, and
            // pads with a blank row above and below.
            Entry::User(text) => wrapped(text, width.saturating_sub(3)) + 2,
            Entry::Assistant(text) => wrapped(text, width),
            Entry::Note(text) => wrapped(text, width),
            Entry::Tool { name, duration_ms } => {
                // Estimate against the *label*, not the raw name: the label is
                // what gets painted, and it is bounded by TOOL_ARG_COLS.
                let (verb, rest) = tool_label(name);
                let suffix = match duration_ms {
                    Some(_) => 8,
                    None => 10,
                };
                let cols = verb.width() + rest.width() + suffix + 3;
                cols.div_ceil(width.max(1))
            }
            Entry::Tools { tools, expanded } => {
                if tools.len() <= 1 || *expanded {
                    // Header row plus one label row per call; each label is
                    // bounded by TOOL_ARG_COLS and carries the same chrome as
                    // a plain Tool entry (glyph + verb + rest + timing).
                    1 + tools
                        .iter()
                        .map(|(name, duration_ms)| {
                            let (verb, rest) = tool_label(name);
                            let cols = verb.width()
                                + rest.width()
                                + 3
                                + usize::from(duration_ms.is_some()) * 8;
                            cols.div_ceil(width.max(1))
                        })
                        .sum::<usize>()
                } else {
                    // The collapsed summary is one line; count it exactly like
                    // the painter lays it out so the estimate never reports
                    // fewer rows than the group paints.
                    let total_ms: u64 = tools.iter().filter_map(|(_, d)| *d).sum();
                    let mut text = format!("⚙ {} tool calls", tools.len());
                    if total_ms > 0 {
                        text.push_str(&format!(" · {:.1}s", total_ms as f64 / 1000.0));
                    }
                    wrapped(&text, width)
                }
            }
        }) + 1
    }

    fn estimated_total_lines(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| self.estimated_entry_lines(entry))
            .sum::<usize>()
            + usize::from(!self.live.is_empty())
    }

    /// The activity band: the single live readout for a running turn.
    ///
    /// This is the only place spinner, elapsed time and the typing pulse are
    /// painted. The footer deliberately carries just the coarse state word, so
    /// there is never a second clock ticking out of step with this one.
    pub fn activity(&self) -> String {
        if !self.busy {
            return String::new();
        }
        let elapsed = self.elapsed().unwrap_or_else(|| "0.0s".into());
        let mut out = format!(
            "{} {} · {}",
            self.spinner_frame(),
            elapsed,
            self.status_or_default()
        );
        // The pulse only runs while tokens are actually landing, so a stalled
        // turn looks different from a streaming one.
        if !self.live.is_empty() {
            out.push_str(&format!(" {}", self.typing_frame()));
        }
        out
    }

    /// Spinner frame. `MOW_NO_ANIM=1` pins a static `●`; elapsed still ticks.
    /// Collapsed peer rows: one line per agent. `ctrl+p` opens the full
    /// buffer in an overlay — peer text never welds onto the host answer.
    ///
    /// Peer deltas are *foreign bytes*: ACP agents emit ANSI colour, cursor
    /// moves, carriage returns and tabs. Painting those raw is what garbles
    /// the frame, so every preview goes through `sanitize_preview` first.
    fn peer_lines(&self) -> Vec<Line<'static>> {
        let mut agents: Vec<&String> = self.peers.keys().collect();
        agents.sort();
        let frame = self.spinner_frame();
        agents
            .into_iter()
            .map(|agent| {
                let preview = last_visible_line(&self.peers[agent]);
                let preview = if preview.is_empty() {
                    "working".to_string()
                } else {
                    preview
                };
                Line::from(vec![
                    Span::styled(format!("{frame} "), self.theme.spinner()),
                    Span::styled("⇄ ", self.theme.peer()),
                    Span::styled(agent.clone(), self.theme.peer()),
                    Span::styled(" · ", self.theme.chrome()),
                    Span::styled(preview, self.theme.note()),
                    Span::styled("  (ctrl+p)", self.theme.timing()),
                ])
            })
            .collect()
    }

    /// Current spinner glyph, frozen to a stable frame when animation is off.
    /// Spinner frame for the running turn.
    ///
    /// With animation disabled (`MOW_NO_ANIM=1`, or a recording/CI terminal) a
    /// frozen braille glyph reads as a stuck spinner. A solid `●` reads as a
    /// deliberate state light instead, so the pause is not mistaken for a hang.
    pub fn spinner_frame(&self) -> &'static str {
        if self.animate {
            SPINNER[(self.tick as usize / 2) % SPINNER.len()]
        } else {
            SPINNER_STATIC
        }
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
                // A fresh loop owns a fresh tool group (a no-op if the
                // previous turn already committed its group at run.end).
                self.live_tools.clear();
            }
            "loop.run.end" | "run.end" => {
                self.busy = false;
                self.status.clear();
                self.activity_started = None;
                self.finish_peers();
                // Commit the turn's tool calls as one entry. Idempotent: the
                // group is consumed, so a later finish_turn has nothing left.
                self.commit_tool_group();
            }
            k if k.ends_with("tool.start") || k == "tool.start" => {
                if let Some(name) = params
                    .get("tool")
                    .or_else(|| params.get("name"))
                    .and_then(|v| v.as_str())
                {
                    self.status = format!("tool · {name}");
                    self.live_tools.push((name.to_string(), None));
                }
            }
            k if k.ends_with("tool.end") || k == "tool.end" => {
                let tool_name = params
                    .get("tool")
                    .or_else(|| params.get("name"))
                    .and_then(Value::as_str);
                self.note_tool_end(tool_name, params.get("duration_ms").and_then(Value::as_u64));
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
            self.status = format!("⇄ {} · receiving", sanitize_preview(&agent));
        } else if kind.contains("delegate") && kind.contains("progress") {
            let agent = params
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("peer");
            let phase = params
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or("working");
            self.status = format!(
                "⇄ {} · {}",
                sanitize_preview(agent),
                sanitize_preview(phase)
            );
        }
        if let Some(d) = token_delta(params) {
            self.live.push_str(d);
            if self.follow {
                self.scroll = u16::MAX;
            }
        }
    }

    /// Record a tool's end: stamp duration onto the matching open call, or the
    /// most recent still-running one when the engine omits the name.
    fn note_tool_end(&mut self, name: Option<&str>, duration: Option<u64>) {
        if let Some(entry) = self.live_tools.iter_mut().rev().find(|(n, d)| match name {
            Some(end_name) => n == end_name,
            None => d.is_none(),
        }) {
            entry.1 = duration;
        }
    }

    /// Fold the current turn's tool calls into one transcript entry.
    /// Consumes `live_tools`, so calling it twice is a safe no-op — it runs at
    /// `run.end` and again defensively at `finish_turn` for engines that skip
    /// the end notification.
    fn commit_tool_group(&mut self) {
        if self.live_tools.is_empty() {
            return;
        }
        if self.live_tools.len() == 1 {
            // A single call stays a plain row: a summary of one tool is a
            // worse transcript, not a better one.
            let (name, duration_ms) = self.live_tools.pop().unwrap();
            self.entries.push(Entry::Tool { name, duration_ms });
        } else {
            let tools = std::mem::take(&mut self.live_tools);
            self.entries.push(Entry::Tools {
                tools,
                expanded: false,
            });
        }
        if self.follow {
            self.scroll = u16::MAX;
        }
    }

    /// `t` on empty input: expand/collapse the most recent tool group.
    /// Returns false when there is nothing to toggle, so the key can fall
    /// through to typing.
    pub fn toggle_tool_group(&mut self) -> bool {
        let Some(group) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            Entry::Tools { expanded, .. } => Some(expanded),
            _ => None,
        }) else {
            return false;
        };
        *group = !*group;
        if self.follow {
            self.scroll = u16::MAX;
        }
        true
    }

    /// Esc: collapse an expanded tool group before falling through to the
    /// destructive cancel. Returns true when one was collapsed.
    fn collapse_tool_group(&mut self) -> bool {
        let Some(expanded_ref) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            Entry::Tools { expanded, .. } if *expanded => Some(expanded),
            _ => None,
        }) else {
            return false;
        };
        *expanded_ref = false;
        if self.follow {
            self.scroll = u16::MAX;
        }
        true
    }

    fn finish_peers(&mut self) {
        self.peer_focus = None;
        for (agent, _) in self.peers.drain() {
            self.entries.push(Entry::Note(format!(
                "⇄ {} · done",
                sanitize_preview(&agent)
            )));
        }
    }

    /// Finish the turn: commit live text (or the final `prompt` result).
    pub fn finish_turn(&mut self, result: Result<Value, Error>) {
        self.busy = false;
        self.activity_started = None;
        // Fallback commit: engines that never emit run.end still get their
        // tool group. No-op when run.end already committed it.
        self.commit_tool_group();
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
            self.set_input(prompt);
            true
        } else {
            false
        }
    }

    fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    fn insert_char(&mut self, c: char) {
        let byte = self.cursor_byte();
        self.input.insert(byte, c);
        self.cursor += 1;
    }

    fn backspace_char(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let start = self.cursor_byte();
        let ch_len = self.input[start..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        if ch_len > 0 {
            self.input.replace_range(start..start + ch_len, "");
        }
    }

    fn delete_char(&mut self) {
        let start = self.cursor_byte();
        let ch_len = self.input[start..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        if ch_len > 0 {
            self.input.replace_range(start..start + ch_len, "");
        }
    }

    fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn cursor_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    /// Insert text (bracketed paste) at the cursor. Multi-line paste lands as
    /// real newlines — the composer is already multi-line.
    pub fn insert_text(&mut self, text: &str) {
        let byte = self.cursor_byte();
        self.input.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    fn cursor_byte(&self) -> usize {
        self.input
            .chars()
            .take(self.cursor)
            .map(char::len_utf8)
            .sum()
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.input.chars().count() as i32;
        self.cursor = (self.cursor as i32 + delta).clamp(0, len) as usize;
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
            ("t (empty input)", "expand / collapse tool calls"),
            ("home / end", "cursor to start / end"),
            ("pgup / pgdn", "scroll transcript"),
            ("ctrl+/ or ?", "this help"),
            ("esc", "dismiss overlay, else cancel turn"),
            ("ctrl+c", "quit (cancel first if busy)"),
            ("tab (on /)", "slash autocomplete"),
            ("/model", "list models, or /model <id> to set"),
            ("/effort", "list efforts, or /effort high to set"),
            ("/clear", "clear transcript (engine history kept)"),
            ("/quit", "quit"),
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

    fn overlay_move(&mut self, delta: i32) {
        let help_len = self.help_rows().len();
        match &mut self.overlay {
            Overlay::Help(state) => step_table(state, help_len, delta),
            Overlay::Sessions { items, state } => step_list(state, items.len(), delta),
            Overlay::Models { list, state } => step_list(state, list.models.len(), delta),
            Overlay::Efforts { list, state } => step_list(state, list.efforts.len(), delta),
            Overlay::Completions { items, state } => step_list(state, items.len(), delta),
            Overlay::Peer | Overlay::None => {}
        }
    }

    /// Selected id in a picker overlay, if any.
    pub fn overlay_selection(&self) -> Option<String> {
        match &self.overlay {
            Overlay::Sessions { items, state } => {
                items.get(state.selected()?).map(|s| s.id.clone())
            }
            Overlay::Models { list, state } => {
                list.models.get(state.selected()?).map(|m| m.id.clone())
            }
            Overlay::Efforts { list, state } => {
                list.efforts.get(state.selected()?).map(|e| e.id.clone())
            }
            Overlay::Completions { items, state } => items.get(state.selected()?).cloned(),
            _ => None,
        }
    }

    fn complete_slash(&mut self) {
        if !self.input.starts_with('/') {
            return;
        }
        let rest = self.input[1..].split_whitespace().next().unwrap_or("");
        let matches = slash_completions(rest, &self.slash_commands);
        match matches.len() {
            0 => self.status = "no matching command".into(),
            1 => self.set_input(format!("/{} ", matches[0])),
            _ => self.overlay = Overlay::completions(matches),
        }
    }

    fn note_unknown_slash(&mut self, name: &str) {
        let msg = unknown_slash_message(name, &self.slash_commands);
        self.status = msg.clone();
        self.entries.push(Entry::Note(msg));
    }

    fn load_transcript(&mut self, messages: Vec<TranscriptMessage>) {
        self.entries = messages
            .into_iter()
            .map(|message| match message.role.as_str() {
                "user" => Entry::User(message.content),
                "assistant" => Entry::Assistant(message.content),
                _ => Entry::Note(format!("{}: {}", message.role, message.content)),
            })
            .collect();
        self.follow = true;
        self.scroll = u16::MAX;
        self.status = "transcript reloaded".into();
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

/// Strip terminal control sequences from foreign (ACP peer) text.
///
/// Peers stream whatever their CLI writes: SGR colour, cursor moves, erase
/// codes, OSC title sets, backspaces, bare CR progress bars. Those bytes must
/// never reach the backend — they move the real cursor and shear the frame.
/// This keeps printable text (and tabs, widened to spaces) and drops the rest.
pub fn sanitize_preview(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // ESC: swallow the whole escape sequence.
            '\u{1b}' => match chars.next() {
                // CSI ... final byte in @..~
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC ... terminated by BEL or ST (ESC \).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-char escape: already consumed.
                _ => {}
            },
            '\t' => out.push_str("    "),
            // Backspace / CR: treat as "redraw this line" like a terminal would.
            '\r' => out.clear(),
            '\u{8}' => {
                out.pop();
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The last non-empty sanitized line of a peer buffer, clipped to `PEER_PREVIEW`
/// display columns (not chars — CJK and emoji are double-width).
fn last_visible_line(buffer: &str) -> String {
    let line = buffer
        .lines()
        .map(sanitize_preview)
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default();
    clip_display(line.trim(), PEER_PREVIEW)
}

/// Truncate to `max` display columns, appending `…` when clipped.
fn clip_display(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

/// The y/a/n decision row, rendered so that **all three keys always survive**.
///
/// A consent surface that clips the reject key is a safety bug, not a layout
/// bug: the operator must never be in a state where "allow" is on screen and
/// "deny" is not. So the labels degrade (full words → single letters → the
/// bare keycaps) and the tool name is dropped, but the three badges are never
/// truncated. If even the bare keycaps do not fit, they are still emitted —
/// a clipped-but-present row beats a silently missing option.
fn decision_line(theme: Theme, width: u16, tool: Option<&str>) -> Line<'static> {
    let width = width as usize;
    let keys = [
        ("y", Tone::Ok, "allow once", "allow"),
        ("a", Tone::Warn, "always allow", "always"),
        ("n", Tone::Error, "deny", "deny"),
    ];

    // Widest-first label plans; the first that fits wins.
    //   0: full words + the tool name
    //   1: full words alone
    //   2: short words
    //   3: bare keycaps
    for plan in 0..4 {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, (key, tone, long, short)) in keys.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  ", theme.note()));
            }
            spans.push(Span::styled(format!(" {key} "), theme.badge_solid(*tone)));
            match plan {
                0 | 1 => spans.push(Span::styled(format!(" {long}"), theme.note())),
                2 => spans.push(Span::styled(format!(" {short}"), theme.note())),
                _ => {}
            }
        }
        // The tool name is context, not a control: it is the first thing cut.
        if plan == 0
            && let Some(name) = tool
        {
            spans.push(Span::styled(format!("   ·   {name}"), theme.note()));
        }
        let painted: usize = spans.iter().map(Span::width).sum();
        if painted <= width || plan == 3 {
            return Line::from(spans);
        }
    }
    Line::from(Vec::new())
}

/// A tool row label: `verb · argument`, never a raw shell blob.
///
/// The Go design notes are explicit about this (`label.go`): build the label
/// from the verb and a meaningful argument, and never mid-string-truncate a
/// shell blob into noise. A chained command like
/// `bash echo ---; cat AGENTS.md; ls -la; git log` is four lines of unreadable
/// transcript that pushes the operator's own prompt off screen.
///
/// So a multi-command blob is reduced to its first command plus a count of
/// what follows. The full text is still in the engine's log; the transcript
/// is a place to see *that* something ran, not to re-read the script.
fn tool_label(name: &str) -> (String, String) {
    let clean = sanitize_preview(name);
    let clean = clean.trim();
    let (verb, rest) = match clean.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb.to_string(), rest.trim().to_string()),
        None => (clean.to_string(), String::new()),
    };
    if rest.is_empty() {
        return (verb, String::new());
    }

    // Count the separate commands in a shell chain. Quoting is not parsed —
    // this is a display heuristic, and over-counting only costs a suffix.
    let extra = rest
        .split(&[';', '\n'][..])
        .filter(|part| !part.trim().is_empty())
        .count()
        .saturating_sub(1);

    let head = rest
        .split(&[';', '\n'][..])
        .map(str::trim)
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let head = clip_display(head, TOOL_ARG_COLS);

    let rest = if extra > 0 {
        format!("{head} (+{extra} more)")
    } else {
        head
    };
    (verb, rest)
}

fn entry_text(entry: &Entry) -> String {
    match entry {
        Entry::User(text) | Entry::Assistant(text) | Entry::Note(text) => text.clone(),
        Entry::Tool { name, .. } => name.clone(),
        Entry::Tools { tools, .. } => tools
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(" "),
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

/// Visible rows the composer may occupy before it starts scrolling.
const INPUT_MAX_ROWS: u16 = 10;

/// Height of the input textarea for an inner width of `width` columns: grows
/// with both explicit newlines and soft-wrapped long text, up to a cap.
fn input_height(app: &App, width: u16) -> u16 {
    (prompt_rows(app, width).len() as u16).clamp(1, INPUT_MAX_ROWS)
}

fn overlay_block(app: &App, title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.chrome_focus())
        .style(app.theme.overlay())
        .padding(Padding::new(1, 1, 0, 0))
        .title(Span::styled(
            format!(" {title} "),
            app.theme.overlay_title(),
        ))
}

/// Modal chrome with the hint text parked on the bottom rail instead of buried
/// in the title. Titles name the thing; the rail names the keys.
fn overlay_block_hint(app: &App, title: &str, hint: &str) -> Block<'static> {
    overlay_block(app, title).title_bottom(
        Line::from(vec![Span::styled(
            format!(" {hint} "),
            app.theme.note().patch(app.theme.overlay()),
        )])
        .alignment(Alignment::Right),
    )
}

/// Dim the document behind a modal so the eye lands on the overlay.
///
/// Only the document region is scrimmed. The header safety chips and the
/// footer decision keys are chrome the operator must still be able to read
/// *while* the modal is up — dimming them would hide the very keys the modal
/// is asking them to press.
///
/// The scrim keeps the glyphs: the frame stays recognisable, it just recedes.
fn draw_scrim(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let buffer = frame.buffer_mut();
    let scrim = app.theme.scrim();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut buffer[(x, y)];
            if app.theme.colored {
                if let Some(fg) = scrim.fg {
                    cell.set_fg(fg);
                }
                if let Some(bg) = scrim.bg {
                    cell.set_bg(bg);
                }
                cell.modifier = Modifier::empty();
            } else {
                // No colour to recede with: fall back to the dim attribute so
                // the document still drops behind the modal.
                cell.modifier = Modifier::DIM;
            }
        }
    }
}

/// Hard-wrap `text` to `width` *display columns*.
///
/// Columns, not runes: a CJK ideograph or an emoji occupies two cells, so
/// counting characters overruns the pane and shears any background band that
/// was padded to match.
fn wrap_cols(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if col + cw > width && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            col = 0;
        }
        current.push(ch);
        col += cw;
    }
    rows.push(current);
    rows
}

/// Wrap a styled line to `width` without using Paragraph wrap, so space-padded
/// bands keep their background instead of being reflowed into a hole.
fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }
    let line_style = line.style;
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    let mut col = 0usize;

    let flush_span = |spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    for span in line.spans {
        if span.style != style {
            flush_span(&mut spans, &mut buf, style);
            style = span.style;
        }
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            // Wrap on display columns so double-width glyphs do not overrun
            // the pane and shear the padded band behind them.
            if col + cw > width && col > 0 {
                flush_span(&mut spans, &mut buf, style);
                rows.push(Line::from(std::mem::take(&mut spans)).style(line_style));
                col = 0;
            }
            buf.push(ch);
            col += cw;
        }
    }
    flush_span(&mut spans, &mut buf, style);
    rows.push(Line::from(spans).style(line_style));
    rows
}

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.tick = frame.count() as u64;
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, app, area);
        return;
    }

    frame.render_widget(Block::new().style(app.theme.base()), area);

    // header · hairline · transcript · [activity] · input · footer
    let mut rows = vec![Constraint::Length(1), Constraint::Length(1)];
    rows.push(Constraint::Fill(1));
    if app.busy {
        rows.push(Constraint::Length(1));
    }
    // The composer block eats 2 columns of border and 2 of padding; size the
    // textarea against that inner width so wrapped text grows the box.
    let input_cols = area.width.saturating_sub(4);
    rows.push(Constraint::Length(input_height(app, input_cols) + 2));
    rows.push(Constraint::Length(1));
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(rows)
        .split(area);
    let rest = &areas[2..];
    let (transcript_area, activity, input_area, footer_area) = if app.busy {
        (rest[0], Some(rest[1]), rest[2], rest[3])
    } else {
        (rest[0], None, rest[1], rest[2])
    };

    paint_filled_header(frame, app, areas[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(areas[1].width as usize),
            app.theme.chrome().patch(app.theme.header_bg()),
        )))
        .style(app.theme.header_bg()),
        areas[1],
    );

    // The activity band exists only while a turn runs.
    if let Some(band) = activity {
        frame.render_widget(
            Paragraph::new(Line::styled(app.activity(), app.theme.note()))
                .style(app.theme.base())
                .block(Block::new().padding(Padding::horizontal(1))),
            band,
        );
    }

    draw_transcript(frame, app, transcript_area);

    let input_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.chrome())
        .style(app.theme.surface())
        .padding(Padding::horizontal(1));
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);
    frame.render_widget(
        Paragraph::new(prompt_text(app, input_inner.width)).style(app.theme.surface()),
        input_inner,
    );
    if app.pending_perm.is_none() && !app.welcome && !app.overlay.is_open() {
        frame.set_cursor_position(input_cursor_pos(app, input_inner));
    }

    // While a decision is pending the footer becomes the decision bar: the
    // keys stay visible even if the overlay is scrolled past or dimmed.
    // The block eats one column of padding on each side.
    frame.render_widget(
        Paragraph::new(app.footer_line(footer_area.width.saturating_sub(2)))
            .style(app.theme.footer_bg())
            .block(Block::new().padding(Padding::horizontal(1))),
        footer_area,
    );

    // The scrim covers the document only: header safety chips, the composer
    // and the footer decision bar all stay sharp, because those are what the
    // operator reads and types into while a modal is up. `doc` is the
    // transcript plus the activity band (when present), ending at the composer.
    let doc = Rect {
        x: area.x,
        y: transcript_area.y,
        width: area.width,
        height: input_area.y.saturating_sub(transcript_area.y),
    };
    if app.welcome {
        draw_scrim(frame, app, doc);
        draw_welcome(frame, app, doc);
        return;
    }
    if app.pending_perm.is_some() {
        draw_scrim(frame, app, doc);
        draw_permission(frame, app, transcript_area);
        return;
    }
    let mut overlay = std::mem::replace(&mut app.overlay, Overlay::None);
    if overlay.is_open() {
        draw_scrim(frame, app, doc);
    }
    // Overlays sit on the document, not over the whole frame: the composer
    // and the status bar stay readable underneath.
    match &mut overlay {
        Overlay::Help(state) => draw_help(frame, app, state, doc),
        Overlay::Sessions { items, state } => draw_sessions(frame, app, items, state, doc),
        Overlay::Models { list, state } => draw_models(frame, app, list, state, doc),
        Overlay::Efforts { list, state } => draw_efforts(frame, app, list, state, doc),
        Overlay::Completions { items, state } => draw_completions(frame, app, items, state, doc),
        Overlay::Peer => draw_peer(frame, app, doc),
        Overlay::None => {}
    }
    app.overlay = overlay;
}

/// Paint the header as a solid mantle bar: fill every cell, then overlay chips.
fn paint_filled_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        app.theme.header_bg(),
    ));
    frame.render_widget(Paragraph::new(fill).style(app.theme.header_bg()), area);
    frame.render_widget(
        Paragraph::new(app.header_line(area.width)).style(app.theme.header_bg()),
        area,
    );
}

/// Input text with the prompt glyph on the first line, padded to `width` so
/// the surface wash is a rectangle.
/// Lay the composer text out into visual rows for an inner width of `width`
/// columns. Only the very first row carries the prompt glyph; every later row —
/// whether it came from a newline or from soft wrapping — is gutter-aligned, so
/// long text expands downward instead of running off the right edge.
/// One visual row of the composer: whether it carries the prompt glyph, its
/// text, and the char offset into `input` where it starts.
struct PromptRow {
    first: bool,
    text: String,
    start: usize,
}

/// Lay the composer out into visual rows once, so the painted text and the
/// caret can never disagree about where a row begins.
///
/// Rows break on display columns, and each row records the char offset it
/// started at — that offset is what turns `app.cursor` back into an (x, y).
fn prompt_layout(app: &App, width: u16) -> Vec<PromptRow> {
    // Two columns go to the glyph/continuation gutter, one is kept free on the
    // right so the caret at end-of-line still has a cell to sit in.
    let usable = (width as usize).saturating_sub(3).max(1);
    let mut rows: Vec<PromptRow> = Vec::new();
    let mut offset = 0usize;
    for (line_idx, logical) in app.input.split('\n').enumerate() {
        let chunks = if logical.is_empty() {
            vec![String::new()]
        } else {
            wrap_cols(logical, usable)
        };
        for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
            let len = chunk.chars().count();
            rows.push(PromptRow {
                first: line_idx == 0 && chunk_idx == 0,
                text: chunk,
                start: offset,
            });
            offset += len;
        }
        // Step over the newline that separated this logical line.
        offset += 1;
    }
    if rows.is_empty() {
        rows.push(PromptRow {
            first: true,
            text: String::new(),
            start: 0,
        });
    }
    rows
}

fn prompt_rows(app: &App, width: u16) -> Vec<(bool, String)> {
    prompt_layout(app, width)
        .into_iter()
        .map(|row| (row.first, row.text))
        .collect()
}

fn prompt_text(app: &App, width: u16) -> Vec<Line<'static>> {
    let cols = width as usize;
    let rows = prompt_rows(app, width);
    // Keep the tail visible once the composer is taller than its cap.
    let skip = rows.len().saturating_sub(INPUT_MAX_ROWS as usize);
    rows.into_iter()
        .skip(skip)
        .map(|(first, text)| {
            let glyph = if first { app.prompt_glyph() } else { "  " };
            let mut spans = vec![
                Span::styled(glyph, app.theme.accent().patch(app.theme.surface())),
                Span::styled(text, app.theme.user()),
            ];
            let painted: usize = spans.iter().map(Span::width).sum();
            let pad = cols.saturating_sub(painted);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), app.theme.surface()));
            }
            Line::from(spans)
        })
        .collect()
}

/// Screen position of the caret.
///
/// Derived from the same `prompt_layout` the text is painted from, so the
/// caret follows soft-wrapped rows and double-width glyphs instead of drifting
/// off the character it is supposed to be sitting on.
fn input_cursor_pos(app: &App, inner: Rect) -> Position {
    let rows = prompt_layout(app, inner.width);
    let visible = INPUT_MAX_ROWS as usize;
    let skip = rows.len().saturating_sub(visible);
    let glyph_w = app.prompt_glyph().width();

    // The row containing the caret is the last one that starts at or before
    // it; `find` from the back keeps end-of-input on the final row.
    let (index, row) = rows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, row)| row.start <= app.cursor)
        .unwrap_or((0, &rows[0]));

    let gutter = if row.first { glyph_w } else { 2 };
    // Columns, not chars: measure the text actually left of the caret.
    let into = app.cursor.saturating_sub(row.start);
    let col: usize = row
        .text
        .chars()
        .take(into)
        .map(|c| c.width().unwrap_or(0))
        .sum();

    let y = inner
        .y
        .saturating_add(index.saturating_sub(skip) as u16)
        .min(inner.y.saturating_add(inner.height.saturating_sub(1)));
    let x = inner
        .x
        .saturating_add((gutter + col) as u16)
        .min(inner.x.saturating_add(inner.width.saturating_sub(1)));
    Position { x, y }
}

fn draw_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Right padding of 2 leaves the last column free for the scrollbar, so a
    // full-width diff band never runs under the thumb.
    let block = Block::new()
        .style(app.theme.base())
        .padding(Padding::new(1, 2, 0, 0));
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
            .style(app.theme.base())
            .scroll((scroll, 0)),
        inner,
    );
    let content = app.estimated_total_lines().max(1);
    let position = if app.follow {
        content.saturating_sub(height)
    } else {
        app.scroll as usize
    };
    app.scrollbar_state = ScrollbarState::new(content)
        .position(position)
        .viewport_content_length(height);
    if overflow > 0 {
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(app.theme.chrome())
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            area,
            &mut app.scrollbar_state,
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

/// First-run splash.
///
/// A new session should answer three questions before anything is typed: where
/// am I, what am I talking to, and what is it allowed to do. Capability is
/// shown as a badge because "this agent can run shell commands" is a safety
/// fact, not a decoration.
fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let workspace = if app.session.workspace.is_empty() {
        "workspace".to_string()
    } else {
        app.session.workspace.clone()
    };
    let model = if app.session.model.is_empty() {
        "model unknown".to_string()
    } else {
        app.session.model.clone()
    };

    let cap_tone = if app.allow_shell || app.allow_write {
        Tone::Warn
    } else {
        Tone::Ok
    };
    let mut body = vec![
        Line::from(Span::styled(
            "mowi",
            app.theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "ratatui client for the mow harness",
            app.theme.note().patch(app.theme.overlay()),
        )),
        Line::raw(""),
        field_row(app, "workspace", &workspace),
        field_row(app, "model", &model),
    ];
    if !app.effort.is_empty() {
        body.push(field_row(app, "effort", &app.effort));
    }
    body.push(Line::from(vec![
        Span::styled(
            format!("{:<10}", "access"),
            app.theme.note().patch(app.theme.overlay()),
        ),
        Span::styled(
            format!(" {} ", app.capability_chip()),
            app.theme.badge_solid(cap_tone),
        ),
        Span::styled(
            format!("  {} mode", app.mode_chip()),
            app.theme.note().patch(app.theme.overlay()),
        ),
    ]));
    body.push(Line::raw(""));
    body.push(Line::from(vec![
        Span::styled("type to begin", app.theme.text().patch(app.theme.overlay())),
        Span::styled(
            "  ·  ? for keys  ·  / for commands",
            app.theme.note().patch(app.theme.overlay()),
        ),
    ]));

    let height = (body.len() as u16 + 2).min(area.height);
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(6).clamp(24, 60)),
        Constraint::Length(height),
    );
    frame.render_widget(Clear, spot);
    frame.render_widget(
        Paragraph::new(body).block(overlay_block(app, "session")),
        spot,
    );
}

/// `label      value` with the label in a fixed gutter, so stacked fields read
/// as a table instead of drifting text.
fn field_row(app: &App, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<10}"),
            app.theme.note().patch(app.theme.overlay()),
        ),
        Span::styled(
            value.to_string(),
            app.theme.text().patch(app.theme.overlay()),
        ),
    ])
}

fn draw_help(frame: &mut Frame<'_>, app: &App, state: &mut TableState, area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(64)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let rows: Vec<Row<'static>> = app
        .help_rows()
        .into_iter()
        .map(|(key, what)| {
            Row::new(vec![
                Cell::from(Span::styled(key, app.theme.chip())),
                Cell::from(Span::styled(what, app.theme.note())),
            ])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(20), Constraint::Fill(1)])
        .header(
            Row::new(vec!["key", "action"]).style(app.theme.accent().add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .row_highlight_style(app.theme.selected())
        .highlight_symbol("▸ ")
        .block(overlay_block_hint(
            app,
            "keyboard reference",
            "↑↓/jk scroll · esc close",
        ));
    frame.render_stateful_widget(table, spot, state);
}

fn draw_sessions(
    frame: &mut Frame<'_>,
    app: &App,
    sessions: &[SessionSummary],
    state: &mut ListState,
    area: Rect,
) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(72)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let items: Vec<ListItem<'static>> = if sessions.is_empty() {
        vec![ListItem::new(Line::styled(
            "no stored sessions",
            app.theme.note(),
        ))]
    } else {
        sessions
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
            .collect()
    };
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_style(app.theme.accent().add_modifier(Modifier::BOLD))
        .block(overlay_block_hint(
            app,
            "sessions",
            "enter resume · mowi --session <id> · esc close",
        ));
    frame.render_stateful_widget(list, spot, state);
}

fn draw_models(
    frame: &mut Frame<'_>,
    app: &App,
    list: &ModelList,
    state: &mut ListState,
    area: Rect,
) {
    let title = if list.current.is_empty() {
        "models".to_string()
    } else {
        format!("models · {}", list.current)
    };
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(64)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let items: Vec<ListItem<'static>> = if list.models.is_empty() {
        vec![ListItem::new(Line::styled("no models", app.theme.note()))]
    } else {
        list.models
            .iter()
            .map(|model| {
                let active = model.current || model.id == list.current;
                let mut spans = vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        app.theme.badge(Tone::Ok).patch(app.theme.overlay()),
                    ),
                    Span::styled(
                        model.id.clone(),
                        if active {
                            app.theme.text().add_modifier(Modifier::BOLD)
                        } else {
                            app.theme.text()
                        },
                    ),
                ];
                // The wire name is provenance, not identity: keep it quiet.
                if !model.wire.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", model.wire),
                        app.theme.note().patch(app.theme.overlay()),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let widget = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_style(app.theme.accent().add_modifier(Modifier::BOLD))
        .block(overlay_block_hint(
            app,
            &title,
            "enter set · /model <id> · esc close",
        ));
    frame.render_stateful_widget(widget, spot, state);
}

fn draw_efforts(
    frame: &mut Frame<'_>,
    app: &App,
    list: &EffortList,
    state: &mut ListState,
    area: Rect,
) {
    let title = if list.current.is_empty() {
        "reasoning effort".to_string()
    } else {
        format!("reasoning effort · {}", list.current)
    };
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(48)),
        Constraint::Length(area.height.saturating_sub(2).min(16)),
    );
    frame.render_widget(Clear, spot);
    let items: Vec<ListItem<'static>> = if list.efforts.is_empty() {
        vec![ListItem::new(Line::styled("no efforts", app.theme.note()))]
    } else {
        list.efforts
            .iter()
            .map(|effort| {
                let active = effort.current || effort.id == list.current;
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "  " },
                        app.theme.badge(Tone::Ok).patch(app.theme.overlay()),
                    ),
                    Span::styled(
                        effort.id.clone(),
                        if active {
                            app.theme.text().add_modifier(Modifier::BOLD)
                        } else {
                            app.theme.text()
                        },
                    ),
                ]))
            })
            .collect()
    };
    let widget = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_style(app.theme.accent().add_modifier(Modifier::BOLD))
        .block(overlay_block_hint(app, &title, "enter set · esc close"));
    frame.render_stateful_widget(widget, spot, state);
}

fn draw_completions(
    frame: &mut Frame<'_>,
    app: &App,
    items: &[String],
    state: &mut ListState,
    area: Rect,
) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(40)),
        Constraint::Length(area.height.saturating_sub(2).min(14)),
    );
    frame.render_widget(Clear, spot);
    let rows: Vec<ListItem<'static>> = items
        .iter()
        .map(|name| ListItem::new(Line::styled(format!("/{name}"), app.theme.chip())))
        .collect();
    let widget = List::new(rows)
        .highlight_symbol("▸ ")
        .highlight_style(app.theme.accent().add_modifier(Modifier::BOLD))
        .block(overlay_block_hint(app, "commands", "enter run · esc close"));
    frame.render_stateful_widget(widget, spot, state);
}

fn draw_peer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let spot = centered(
        area,
        Constraint::Length(area.width.saturating_sub(4).min(72)),
        Constraint::Length(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, spot);
    let agent = app.peer_focus.clone().unwrap_or_else(|| "peer".to_string());
    let raw = app.peers.get(&agent).cloned().unwrap_or_default();
    // Same rule as the collapsed row: foreign bytes are stripped per line
    // before they can reach the backend.
    let body: Vec<Line<'static>> = raw
        .lines()
        .map(|line| Line::styled(sanitize_preview(line), app.theme.context()))
        .collect();
    let tail = body
        .len()
        .saturating_sub(spot.height.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(body)
            .style(app.theme.context())
            .scroll((tail as u16, 0))
            .wrap(Wrap { trim: false })
            .block(overlay_block_hint(app, &format!("⇄ {agent}"), "esc close")),
        spot,
    );
}

/// The approval prompt.
///
/// This is the highest-stakes surface in the client: it is the moment a human
/// grants an agent the ability to touch their machine. It is sized to its
/// content (never a fixed 14-row slab), the payload is the visual focus, and
/// the decision keys are a labelled row rather than border decoration.
fn draw_permission(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(permission) = &app.pending_perm else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(24, 76);
    // Inner width: two border columns and two padding columns.
    let inner_w = width.saturating_sub(4).max(8) as usize;

    let mut body: Vec<Line<'static>> = Vec::new();
    body.push(Line::from(vec![
        Span::styled("▲ ", app.theme.warn()),
        Span::styled(
            permission.name.clone(),
            app.theme.text().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" wants to run", app.theme.note()),
    ]));
    body.push(Line::raw(""));
    // The payload is what is actually being consented to — give it the code
    // treatment so it cannot be confused with the surrounding prose.
    for line in permission_preview(permission).lines() {
        for chunk in wrap_cols(&sanitize_preview(line), inner_w) {
            // Pad by display columns: a CJK or emoji argument is double-width,
            // so counting runes would leave a ragged code band.
            let pad = inner_w.saturating_sub(chunk.width());
            body.push(Line::from(vec![
                Span::styled(chunk, app.theme.md_code_block()),
                Span::styled(" ".repeat(pad), app.theme.md_code_block()),
            ]));
        }
    }
    body.push(Line::raw(""));
    body.push(decision_line(app.theme, inner_w as u16, None));

    // Fit to content, but never taller than the pane it is shown over.
    let height = (body.len() as u16 + 2).min(area.height.max(3));
    let spot = centered(area, Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, spot);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.warn())
        .style(app.theme.surface())
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " approval required ",
            app.theme.warn().patch(app.theme.surface()),
        ));
    frame.render_widget(Paragraph::new(body).block(block), spot);
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
                    // Usage only moves at turn end; refresh the gauge here
                    // rather than polling. A failure is not fatal to the UI.
                    if app.supports("context")
                        && let Ok(usage) = client.context(Duration::from_secs(5))
                    {
                        app.apply_context(&usage);
                    }
                    if let Some(next) = app.next_queued_prompt() {
                        turn = Some(start_prompt(client, app, &next)?);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    // The child died mid-turn. Its stderr is captured (never
                    // inherited, or it would paint over this frame), so show
                    // the tail instead of a bare "closed".
                    for line in client.stderr_tail() {
                        app.entries
                            .push(Entry::Note(format!("mow: {}", sanitize_preview(&line))));
                    }
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
                    handle_key(key, client, app, &mut turn, &mut slash_rx)?;
                }
                // Bracketed paste lands at the input cursor, multi-line safe.
                Event::Paste(text) => {
                    // A stray paste must never answer a permission prompt.
                    if app.pending_perm.is_none() {
                        if app.welcome {
                            app.welcome = false;
                        }
                        app.insert_text(&text);
                    }
                }
                Event::Resize(..) if !app.follow => {
                    // Layout recomputes on the next draw; just clamp scroll so
                    // a shrink cannot strand the viewport past the end.
                    app.scroll = app.scroll.min(max_scroll(app));
                }
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

    // Welcome banner is chrome only: the key still reaches the composer.
    if app.welcome {
        app.welcome = false;
    }

    // Overlays: esc closes; arrows/jk move; enter picks.
    if app.overlay.is_open() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.overlay = Overlay::None,
            KeyCode::Char('?') => app.overlay = Overlay::None,
            KeyCode::Char('c') if ctrl => app.quit = true,
            KeyCode::Up | KeyCode::Char('k') => app.overlay_move(-1),
            KeyCode::Down | KeyCode::Char('j') => app.overlay_move(1),
            KeyCode::Enter => overlay_activate(client, app)?,
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if ctrl => {
            if app.busy || turn.is_some() {
                let _ = client.cancel();
            }
            app.quit = true;
        }
        KeyCode::Char('j') if ctrl => app.insert_char('\n'),
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
        KeyCode::Char('/') if ctrl => app.overlay = Overlay::help(),
        KeyCode::Char('?') if app.input.is_empty() => app.overlay = Overlay::help(),
        KeyCode::Esc => {
            if app.dismiss_overlay() {
            } else if app.collapse_tool_group() {
                // Esc is a view op first: collapse an expanded tool group
                // before it ever reaches the destructive cancel.
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
                app.clear_input();
            } else if text.starts_with('/') {
                handle_slash(&text, client, app, turn, slash_rx)?;
                app.clear_input();
            } else if app.busy || turn.is_some() {
                if !text.is_empty() {
                    app.enqueue_prompt(text);
                }
                app.clear_input();
            } else if !text.is_empty() {
                *turn = Some(start_prompt(client, app, &text)?);
                app.clear_input();
            }
        }
        KeyCode::Backspace => app.backspace_char(),
        KeyCode::Delete => app.delete_char(),
        KeyCode::Home => app.cursor_home(),
        KeyCode::End => app.cursor_end(),
        KeyCode::PageUp => leave_follow(app, 5),
        KeyCode::PageDown => scroll_down(app, 5),
        // `t` on empty input toggles the last tool group; with nothing to
        // toggle the key falls through and types like any other character.
        KeyCode::Char('t') if app.input.is_empty() && app.toggle_tool_group() => {}
        KeyCode::Tab => {
            if app.input.starts_with('/') {
                app.complete_slash();
            }
        }
        KeyCode::Left => app.move_cursor(-1),
        KeyCode::Right => app.move_cursor(1),
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
        KeyCode::Char(c) => app.insert_char(c),
        _ => {}
    }
    Ok(())
}

fn overlay_activate(client: &mut Client, app: &mut App) -> Result<(), Error> {
    let selection = app.overlay_selection();
    let picker = match &app.overlay {
        Overlay::Help(_) | Overlay::Peer => "close",
        Overlay::Sessions { .. } => "session",
        Overlay::Models { .. } => "model",
        Overlay::Efforts { .. } => "effort",
        Overlay::Completions { .. } => "complete",
        Overlay::None => "none",
    };
    match picker {
        "close" => app.overlay = Overlay::None,
        "session" => {
            if let Some(id) = selection {
                app.status = format!("resume with: mowi --session {id}");
                app.entries
                    .push(Entry::Note(format!("resume with: mowi --session {id}")));
            }
            app.overlay = Overlay::None;
        }
        "model" => {
            if let Some(id) = selection {
                let model = client.model_set(&id, Duration::from_secs(20))?;
                app.apply_model_set(&model);
            } else {
                app.overlay = Overlay::None;
            }
        }
        "effort" => {
            if let Some(id) = selection {
                let effort = client.effort_set(&id, Duration::from_secs(20))?;
                app.apply_effort_set(&effort);
            } else {
                app.overlay = Overlay::None;
            }
        }
        "complete" => {
            if let Some(name) = selection {
                app.set_input(format!("/{name} "));
            }
            app.overlay = Overlay::None;
        }
        _ => {}
    }
    Ok(())
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
    match slash_route(name, &app.slash_commands) {
        SlashRoute::Quit => {
            if app.busy || turn.is_some() {
                let _ = client.cancel();
            }
            app.quit = true;
            return Ok(());
        }
        SlashRoute::Unknown => {
            app.note_unknown_slash(name);
            return Ok(());
        }
        SlashRoute::Rpc => {
            if app.refuses_exclusive_slash(name) {
                app.status = format!("/{name} is unavailable while busy");
            } else {
                *slash_rx = Some(client.slash(name, &args, false)?);
            }
            return Ok(());
        }
        SlashRoute::Local => {}
    }
    match canonical_slash(name) {
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
            app.overlay = Overlay::help();
        }
        "clear" => {
            app.clear_transcript();
        }
        "sessions" | "resume" => {
            if canonical_slash(name) == "resume" && !args.is_empty() {
                let id = &args[0];
                app.status = format!("resume with: mowi --session {id}");
                app.entries
                    .push(Entry::Note(format!("resume with: mowi --session {id}")));
            } else {
                let sessions = client.sessions(Duration::from_secs(20))?;
                app.overlay = Overlay::sessions(sessions);
            }
        }
        "transcript" => {
            let messages = client.transcript(Duration::from_secs(20))?;
            app.load_transcript(messages);
        }
        "model" => {
            if args.is_empty() {
                let list = client.model_list(Duration::from_secs(20))?;
                app.apply_model_list(list);
            } else {
                let model = client.model_set(&args.join(" "), Duration::from_secs(20))?;
                app.apply_model_set(&model);
            }
        }
        "effort" => {
            if args.is_empty() {
                let list = client.effort_list(Duration::from_secs(20))?;
                app.apply_effort_list(list);
            } else {
                let effort = client.effort_set(&args.join(" "), Duration::from_secs(20))?;
                app.apply_effort_set(&effort);
            }
        }
        "status" => {
            let status = client.status(Duration::from_secs(20))?;
            app.apply_status(&status);
            app.entries.push(Entry::Note(format!("status: {status}")));
        }
        "context" => {
            let usage = client.context(Duration::from_secs(20))?;
            app.apply_context(&usage);
            app.entries.push(Entry::Note(app.context_summary()));
        }
        "compact" => {
            let max_chars = args
                .first()
                .and_then(|a| a.trim().parse::<i64>().ok())
                .unwrap_or(0);
            let rep = client.compact(max_chars, Duration::from_secs(120))?;
            let saved = if rep.over_budget {
                " (still over budget)"
            } else {
                ""
            };
            app.entries.push(Entry::Note(format!(
                "compacted [{}]: {} -> {} messages, {} chars saved{saved}",
                rep.layer, rep.messages_before, rep.messages_after, rep.chars_saved
            )));
            // Engine history changed under us; repaint from the new truth.
            let messages = client.transcript(Duration::from_secs(20))?;
            app.load_transcript(messages);
            app.status = format!("compacted · {} tokens", rep.tokens);
        }
        "rewind" | "undo" => match client.rewind(Duration::from_secs(20))? {
            Some(last_user) => {
                let messages = client.transcript(Duration::from_secs(20))?;
                app.load_transcript(messages);
                app.set_input(last_user);
                app.status = "rewound — edit and send again".into();
            }
            None => app.status = "nothing to rewind".into(),
        },
        "skills" => {
            if args.is_empty() {
                let skills = client.skill_list(Duration::from_secs(20))?;
                if skills.is_empty() {
                    app.status = "no skills in this workspace".into();
                } else {
                    app.entries
                        .push(Entry::Note(format!("skills: {}", skills.join(", "))));
                }
            } else {
                let (activated, unknown) = client.skill_activate(&args, Duration::from_secs(20))?;
                let mut msg = String::new();
                if !activated.is_empty() {
                    msg.push_str(&format!("activated: {}", activated.join(", ")));
                }
                if !unknown.is_empty() {
                    if !msg.is_empty() {
                        msg.push_str(" · ");
                    }
                    msg.push_str(&format!("unknown: {}", unknown.join(", ")));
                }
                app.entries.push(Entry::Note(msg));
            }
        }
        "steer" => {
            app.status = "steer is /steer <text> while a turn is running".into();
        }
        _ => {
            app.status = format!("/{name} is handled locally");
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
    use crate::rpc::ModelInfo;

    fn usage(tokens: u64, window: Option<u64>, pct: Option<f64>) -> ContextUsage {
        ContextUsage {
            tokens,
            context_window: window,
            remaining: window.map(|w| w.saturating_sub(tokens)),
            percent: pct,
        }
    }

    #[test]
    fn capabilities_gate_unknown_methods() {
        let mut app = App::new(SessionInfo::default());
        // No list yet (or a pre-capabilities server): assume support rather
        // than hiding features we cannot prove absent.
        assert!(app.supports("compact"));
        app.set_capabilities(&["prompt".into(), "context".into()]);
        assert!(app.supports("context"));
        assert!(!app.supports("compact"));
    }

    #[test]
    fn context_summary_renders_window_when_known() {
        let mut app = App::new(SessionInfo::default());
        assert_eq!(app.context_summary(), "context: unknown");
        app.apply_context(&usage(12_300, Some(200_000), Some(6.15)));
        assert_eq!(app.context_summary(), "context: 12.3k / 200k (6%)");
        app.apply_context(&usage(950, None, None));
        assert_eq!(app.context_summary(), "context: 950 tokens");
    }

    #[test]
    fn tool_labels_collapse_shell_chains() {
        // The exact shape that garbled the transcript: a chained shell blob
        // passed through as the tool name.
        let (verb, rest) = tool_label(
            "bash echo ----; cat AGENTS.md 2>/dev/null || cat CLAUDE.md 2>/dev/null; ls -la",
        );
        assert_eq!(verb, "bash");
        assert!(rest.starts_with("echo ----"), "{rest}");
        assert!(rest.contains("+2 more"), "{rest}");
        // The whole chain must never be reproduced verbatim.
        assert!(!rest.contains("CLAUDE.md"), "{rest}");
        assert!(rest.width() <= TOOL_ARG_COLS + 12, "{rest}");

        // A plain single command keeps its argument intact.
        let (verb, rest) = tool_label("read src/app.rs");
        assert_eq!(verb, "read");
        assert_eq!(rest, "src/app.rs");

        // A bare tool name has no argument half.
        let (verb, rest) = tool_label("status");
        assert_eq!(verb, "status");
        assert!(rest.is_empty());
    }

    #[test]
    fn long_tool_names_do_not_eat_the_user_prompt() {
        // A tool row whose name is a whole shell blob wraps to several lines.
        // The transcript window math must agree with what is painted, or the
        // overflow scrolls the user's own prompt off the top of the pane.
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::User("summarise the repo".into()));
        app.entries.push(Entry::Tool {
            name: "bash echo ----; cat AGENTS.md 2>/dev/null || cat CLAUDE.md 2>/dev/null; \
                   echo ----; ls -la; git log --oneline | head -20"
                .into(),
            duration_ms: Some(120),
        });
        app.entries
            .push(Entry::Assistant("Here is the summary.".into()));

        let out = render(&mut app, 60, 20);
        assert!(
            out.contains("summarise the repo"),
            "user prompt lost:\n{out}"
        );
        assert!(out.contains("Here is the summary."), "answer lost:\n{out}");
    }

    #[test]
    fn estimated_height_matches_painted_height() {
        // `visible_transcript_lines` slices the document using estimates. If an
        // estimate is smaller than what `entry_lines` actually paints, the
        // window slides and earlier entries get scrolled away.
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.last_view_w = 40;
        let entries = vec![
            Entry::User("a short prompt".into()),
            Entry::User("a prompt that is quite a lot longer than forty columns".into()),
            Entry::Assistant("one line".into()),
            Entry::Assistant("a much longer answer that has to wrap several times over".into()),
            Entry::Tool {
                name: "bash echo ----; cat AGENTS.md; echo ----; ls -la; git log".into(),
                duration_ms: Some(90),
            },
            Entry::Note("a note that also happens to be rather long indeed".into()),
        ];
        for entry in entries {
            let painted = app.entry_lines(&entry).len();
            let estimated = app.estimated_entry_lines(&entry) - 1; // minus separator
            assert!(
                estimated >= painted,
                "under-estimated {entry:?}: estimated {estimated}, painted {painted}"
            );
        }
    }

    #[test]
    fn caret_tracks_wrapped_and_wide_text() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        let inner = Rect {
            x: 1,
            y: 5,
            width: 20,
            height: 10,
        };

        // Empty input: caret sits just past the prompt glyph.
        assert_eq!(input_cursor_pos(&app, inner), Position { x: 3, y: 5 });

        // ASCII on the first row advances one column per char.
        app.input = "abc".into();
        app.cursor = 3;
        assert_eq!(input_cursor_pos(&app, inner), Position { x: 6, y: 5 });

        // Double-width glyphs advance two columns each, not one.
        app.input = "日本".into();
        app.cursor = 2;
        assert_eq!(input_cursor_pos(&app, inner), Position { x: 7, y: 5 });

        // An explicit newline moves to the continuation gutter on the next row.
        app.input = "ab\ncd".into();
        app.cursor = 4;
        let pos = input_cursor_pos(&app, inner);
        assert_eq!(pos.y, 6, "caret should be on the second row");
        assert_eq!(pos.x, 4, "continuation gutter is two columns");

        // Soft wrap: the caret follows onto the wrapped row rather than
        // running off the right edge of the composer.
        app.input = "x".repeat(40);
        app.cursor = 40;
        let pos = input_cursor_pos(&app, inner);
        assert!(pos.y > 5, "caret should have wrapped down");
        assert!(
            pos.x < inner.x + inner.width,
            "caret escaped the composer: {pos:?}"
        );
    }

    #[test]
    fn scrim_dims_the_document_without_colour() {
        // With NO_COLOR there is no ground to recede with, so the scrim has to
        // fall back to the DIM attribute — otherwise modals float on top of
        // undimmed text and the layering is invisible.
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: false };
        app.entries.push(Entry::User("hello there".into()));
        app.overlay = Overlay::help();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        // Row 2 is inside the document region, above the overlay body.
        let dimmed = (0..80u16).any(|x| buf[(x, 2)].modifier.contains(Modifier::DIM));
        assert!(dimmed, "document was not dimmed behind the modal");

        // No colour leaked in while doing it.
        for y in 0..24u16 {
            for x in 0..80u16 {
                assert_eq!(buf[(x, y)].fg, Color::Reset, "fg leaked at {x},{y}");
                assert_eq!(buf[(x, y)].bg, Color::Reset, "bg leaked at {x},{y}");
            }
        }
    }

    #[test]
    fn deny_survives_every_supported_width() {
        // A consent surface that clips the reject key is a safety bug. At no
        // width the client claims to support may "allow" be reachable while
        // "deny" is off screen.
        for width in MIN_WIDTH..=120 {
            let line = decision_line(Theme { colored: true }, width, Some("bash"));
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for key in [" y ", " a ", " n "] {
                assert!(text.contains(key), "width {width} lost {key}: {text:?}");
            }
            let painted: usize = line.spans.iter().map(Span::width).sum();
            assert!(
                painted <= width as usize,
                "width {width} overflowed to {painted}: {text:?}"
            );
        }
    }

    #[test]
    fn permission_modal_shows_all_decisions_when_narrow() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.pending_perm = Some(PermissionRequest {
            id: "perm-1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "cargo test"}),
            tool_call_id: "call-1".into(),
        });
        // MIN_WIDTH is the narrowest frame the client will paint at all.
        // Assert on the decision labels, not bare letters: "command" contains
        // an "n" and would make a broken row look like a passing test.
        let out = render(&mut app, MIN_WIDTH, 20);
        for label in ["allow", "always", "deny"] {
            assert!(out.contains(label), "missing {label} at MIN_WIDTH:\n{out}");
        }
    }

    #[test]
    fn narrow_header_drops_the_gauge_before_the_model() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/very/long/workspace/path/that/eats/columns".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.apply_context(&usage(100_000, Some(200_000), Some(50.0)));
        // Usage must be present: it is the chip that previously outlived the
        // model, because the drop loop peels from the front.
        app.usage.input_tokens = 41_500;
        app.usage.output_tokens = 3_200;

        for width in [48u16, 60, 70] {
            let line = app.header_line(width);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            // Identity outranks both the meter and the token counter.
            assert!(text.contains("gpt-5-mini"), "width {width}: {text}");
            assert!(!text.contains('▰'), "width {width} kept the gauge: {text}");
            // Safety chips never drop, at any width.
            assert!(text.contains("read-only"), "width {width}: {text}");
            assert!(text.contains("ask"), "width {width}: {text}");
        }
    }

    #[test]
    fn wrap_cols_counts_display_columns_not_runes() {
        // Each ideograph is two cells wide, so four of them fill 8 columns.
        let rows = wrap_cols("日本語文字", 8);
        assert_eq!(rows[0], "日本語文");
        assert_eq!(rows[1], "字");
    }

    #[test]
    fn overlays_leave_the_composer_and_status_bar_alone() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.input.push_str("draft text");
        app.cursor = app.input.chars().count();
        app.overlay = Overlay::help();
        let out = render(&mut app, 80, 24);
        // The modal must not swallow the thing the operator was typing.
        assert!(out.contains("draft text"), "{out}");
        assert!(out.contains("idle"), "{out}");
    }

    #[test]
    fn footer_shows_context_percent_only_under_pressure() {
        let mut app = App::new(SessionInfo::default());
        assert!(!app.footer().contains("ctx "));
        // Quiet while there is plenty of headroom: the header gauge covers it.
        app.apply_context(&usage(40_000, Some(200_000), Some(20.0)));
        assert!(!app.footer().contains("ctx "), "footer: {}", app.footer());
        // Once the window is filling up the footer speaks.
        app.apply_context(&usage(150_000, Some(200_000), Some(75.0)));
        assert!(app.footer().contains("ctx 75%"), "footer: {}", app.footer());
    }
    use ratatui::{Terminal, backend::TestBackend, style::Color};

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
        app.theme = Theme { colored: true };
        app.entries.push(Entry::User("hi".into()));
        app.live.push_str("hello");

        let out = render(&mut app, 80, 14);
        assert!(out.contains("mowi"), "{out}");
        assert!(out.contains("gpt-5-mini"), "{out}");
        assert!(out.contains("abcdef01"), "{out}");
        assert!(out.contains("hi"), "{out}");
        assert!(!out.contains("❯ hi"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("enter send"), "{out}");
        // Safety chips are always painted.
        assert!(out.contains("read-only"), "{out}");
        assert!(out.contains("ask"), "{out}");
    }

    #[test]
    fn header_row_fills_header_bg() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.theme = Theme { colored: true };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let mantle = Color::Rgb(0x18, 0x18, 0x25);
        for x in 0..100 {
            assert_eq!(
                buf[(x, 0)].bg,
                mantle,
                "header cell ({x},0) bg={:?} symbol={:?}",
                buf[(x, 0)].bg,
                buf[(x, 0)].symbol()
            );
        }
    }

    #[test]
    fn user_entry_is_a_filled_band() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::User("hi".into()));
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let user_bg = Color::Rgb(0x31, 0x32, 0x44);
        let rail_fg = Color::Rgb(0xb4, 0xbe, 0xfe);
        let mut found = None;
        for y in 0..16u16 {
            let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if let Some(x) = row.find("hi") {
                found = Some((x as u16, y));
                break;
            }
        }
        let (x, y) = found.expect("user text not painted");
        assert_eq!(buf[(x, y)].bg, user_bg, "text cell {x},{y}");
        // Inner transcript starts at x=1 (left pad); the rail is a lavender
        // glyph on the band ground, not a differently-coloured cell.
        assert_eq!(buf[(1, y)].symbol(), "▎", "accent rail glyph");
        assert_eq!(buf[(1, y)].fg, rail_fg, "accent rail colour");
        assert_eq!(buf[(1, y)].bg, user_bg, "rail sits on the band");
        let pad_cell = &buf[(x + 4, y)];
        assert_eq!(pad_cell.bg, user_bg, "pad cell after text");
        assert_eq!(pad_cell.symbol(), " ", "pad with spaces, not blocks");
        // Blank rows inside the band, above and below the text.
        assert_eq!(buf[(x, y - 1)].bg, user_bg, "pad row above");
        assert_eq!(buf[(x, y - 1)].symbol(), " ", "pad row is spaces");
        assert_eq!(buf[(x, y + 1)].bg, user_bg, "pad row below");
        assert_eq!(buf[(x, y + 1)].symbol(), " ", "pad row is spaces");
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
    fn welcome_banner_paints_and_clears_without_swallowing_a_key() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.welcome = true;
        let out = render(&mut app, 60, 16);
        assert!(out.contains("type to begin"), "{out}");
        assert!(out.contains("workspace"), "{out}");
        assert!(out.contains("read-only"), "{out}");
        assert!(!out.contains("any key"), "{out}");

        // Typing clears the banner and still lands in the composer: no key is
        // spent just to dismiss the splash.
        app.welcome = false;
        app.input.push('h');
        let out = render(&mut app, 60, 16);
        assert!(!out.contains("ask anything"), "{out}");
        assert!(out.contains("❯ h"), "{out}");
    }

    #[test]
    fn long_prompt_wraps_and_grows_the_composer() {
        let mut app = App::new(SessionInfo::default());
        app.input = "x".repeat(120);
        // 60 cols of frame → 56 of textarea inner width, so 120 chars need 3 rows.
        assert!(input_height(&app, 56) >= 3, "{}", input_height(&app, 56));
        let out = render(&mut app, 60, 20);
        for row in out.lines() {
            assert!(row.chars().count() <= 60, "{row}");
        }
        let rows = prompt_rows(&app, 56);
        assert!(rows.len() >= 3, "{rows:?}");
        assert!(rows[0].0 && !rows[1].0, "{rows:?}");
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
        app.overlay = Overlay::help();
        let out = render(&mut app, 70, 24);
        assert!(out.contains("help") || out.contains("keyboard"), "{out}");
        assert!(out.contains("ctrl+j"), "{out}");
        // The table is longer than any overlay: the tail is reachable by
        // scrolling, not by being painted at once.
        for _ in 0..app.help_rows().len() {
            app.overlay_move(1);
        }
        let tail = render(&mut app, 70, 24);
        assert!(tail.contains("/review"), "{tail}");
        assert!(tail.contains("/model"), "{tail}");
        assert!(tail.contains("/quit"), "{tail}");
        assert!(!out.contains("ctrl+s"), "{out}");

        assert!(app.dismiss_overlay());
        assert_eq!(app.overlay, Overlay::None);
        let out = render(&mut app, 70, 20);
        assert!(!out.contains("ctrl+j"), "{out}");
    }

    #[test]
    fn sessions_overlay_lists_rows_and_resume_hint() {
        let mut app = App::new(SessionInfo::default());
        app.overlay = Overlay::sessions(vec![SessionSummary {
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
    fn models_overlay_lists_ids_and_current() {
        let mut app = App::new(SessionInfo {
            model: "claude-sonnet-4".into(),
            ..Default::default()
        });
        app.apply_model_list(ModelList {
            current: "gpt-5-mini".into(),
            models: vec![
                ModelInfo {
                    id: "gpt-5-mini".into(),
                    current: true,
                    wire: "openai-responses".into(),
                },
                ModelInfo {
                    id: "claude-sonnet-4".into(),
                    current: false,
                    wire: String::new(),
                },
            ],
        });
        assert_eq!(app.session.model, "gpt-5-mini");
        let out = render(&mut app, 72, 16);
        assert!(out.contains("gpt-5-mini"), "{out}");
        // Current model: named in the title, dotted in the list.
        assert!(out.contains("models · gpt-5-mini"), "{out}");
        assert!(out.contains("● gpt-5-mini"), "{out}");
        assert!(out.contains("claude-sonnet-4"), "{out}");
        assert!(out.contains("/model"), "{out}");
    }

    #[test]
    fn model_set_updates_session_model() {
        let mut app = App::new(SessionInfo {
            model: "claude-sonnet-4".into(),
            ..Default::default()
        });
        app.apply_model_set("gpt-5-mini");
        assert_eq!(app.session.model, "gpt-5-mini");
        assert!(app.status.contains("gpt-5-mini"), "{}", app.status);
        assert!(
            app.entries
                .iter()
                .any(|entry| matches!(entry, Entry::Note(note) if note.contains("gpt-5-mini")))
        );
    }

    #[test]
    fn effort_overlay_lists_ids_and_set_updates_status() {
        let mut app = App::new(SessionInfo::default());
        app.apply_effort_list(EffortList {
            current: "none".into(),
            default: "none".into(),
            efforts: vec![
                crate::rpc::EffortInfo {
                    id: "none".into(),
                    current: true,
                },
                crate::rpc::EffortInfo {
                    id: "high".into(),
                    current: false,
                },
            ],
        });
        assert_eq!(app.effort, "none");
        assert_eq!(app.overlay_selection().as_deref(), Some("none"));
        app.overlay_move(1);
        assert_eq!(app.overlay_selection().as_deref(), Some("high"));
        let out = render(&mut app, 72, 16);
        assert!(out.contains("high"), "{out}");
        // The active effort is marked with a dot and named in the title.
        assert!(out.contains("reasoning effort · none"), "{out}");
        assert!(out.contains("● none"), "{out}");

        app.apply_effort_set("high");
        assert_eq!(app.effort, "high");
        assert!(app.status.contains("high"), "{}", app.status);
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn unknown_slash_is_a_local_error_and_tab_completes() {
        let mut app = App::new(SessionInfo::default());
        app.slash_commands.push(SlashCommand {
            name: "review".into(),
            summary: "review changes".into(),
            exclusive: true,
            aliases: vec![],
        });
        app.note_unknown_slash("bogus");
        let note = match app.entries.last() {
            Some(Entry::Note(text)) => text.clone(),
            other => panic!("want note, got {other:?}"),
        };
        assert!(note.contains("unknown /bogus"), "{note}");
        assert!(note.contains("/effort"), "{note}");
        assert!(note.contains("/review"), "{note}");

        app.set_input("/eff".into());
        app.complete_slash();
        assert_eq!(app.input, "/effort ");
        app.set_input("/".into());
        app.complete_slash();
        assert!(matches!(app.overlay, Overlay::Completions { .. }));
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
        // The tool, the exact payload, and all three decisions are visible
        // before the operator can approve anything.
        assert!(out.contains("bash"), "{out}");
        assert!(out.contains("build"), "{out}");
        assert!(out.contains("allow once"), "{out}");
        assert!(out.contains("always allow"), "{out}");
        assert!(out.contains("deny"), "{out}");
        // Send hints are suppressed: the only live keys are the decisions.
        assert!(!out.contains("enter send"), "{out}");
    }

    #[test]
    fn ctrl_j_grows_the_input_area() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        assert_eq!(input_height(&app, 56), 1);
        app.input.push_str("one");
        app.input.push('\n');
        app.input.push_str("two");
        assert_eq!(input_height(&app, 56), 2);
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
        assert!(out.contains("⇄ peer-agent"), "{out}");

        assert!(app.toggle_peer_expand());
        assert_eq!(app.peer_focus.as_deref(), Some("peer-agent"));
        assert!(app.toggle_peer_expand());
        assert!(app.peer_focus.is_none());
    }

    #[test]
    fn context_meter_paints_its_pressure_tone_in_the_header() {
        let mut app = App::new(SessionInfo::default());
        app.ctx = Some(ContextUsage {
            tokens: 9_500,
            context_window: Some(10_000),
            percent: Some(95.0),
            remaining: Some(500),
        });
        let line = app.header_line(120);
        let meter = line
            .spans
            .iter()
            .find(|s| s.content.contains("95%"))
            .expect("meter missing from header");
        // At 95% the meter must not look like ordinary chrome.
        assert_eq!(meter.style.fg, app.theme.badge(Tone::Error).fg);
    }

    #[test]
    fn context_meter_fills_and_escalates_tone() {
        let mut app = App::new(SessionInfo::default());
        // No context result yet: no chip, never a fake zero.
        assert!(app.context_chip().is_none());

        app.ctx = Some(ContextUsage {
            tokens: 1_000,
            context_window: Some(10_000),
            percent: Some(10.0),
            remaining: Some(9_000),
        });
        let chip = app.context_chip().unwrap();
        assert!(chip.contains("10%"), "{chip}");
        assert!(chip.starts_with('▰'), "{chip}");
        assert_eq!(app.context_tone(), Tone::Muted);

        app.ctx = Some(ContextUsage {
            tokens: 8_000,
            context_window: Some(10_000),
            percent: Some(80.0),
            remaining: Some(2_000),
        });
        assert_eq!(app.context_tone(), Tone::Warn);

        app.ctx = Some(ContextUsage {
            tokens: 9_500,
            context_window: Some(10_000),
            percent: Some(95.0),
            remaining: Some(500),
        });
        assert_eq!(app.context_tone(), Tone::Error);
        let full = app.context_chip().unwrap();
        assert!(!full.contains('▱'), "95% should be nearly solid: {full}");
    }

    #[test]
    fn context_meter_is_derived_when_percent_is_absent() {
        let mut app = App::new(SessionInfo::default());
        app.ctx = Some(ContextUsage {
            tokens: 500,
            context_window: Some(1_000),
            percent: None,
            remaining: None,
        });
        assert!(app.context_chip().unwrap().contains("50%"));
    }

    #[test]
    fn running_and_finished_tools_read_differently() {
        let mut app = App::new(SessionInfo::default());
        app.animate = false;
        app.entries.push(Entry::Tool {
            name: "read".into(),
            duration_ms: Some(1500),
        });
        app.entries.push(Entry::Tool {
            name: "bash".into(),
            duration_ms: None,
        });
        let out = render(&mut app, 70, 16);
        assert!(out.contains("✓"), "finished tool needs a check: {out}");
        assert!(out.contains("1.5s"), "finished tool needs timing: {out}");
        assert!(
            out.contains("running"),
            "in-flight tool needs a state: {out}"
        );
    }

    #[test]
    fn ansi_from_a_tool_name_cannot_reach_the_frame() {
        let mut app = App::new(SessionInfo::default());
        app.animate = false;
        app.entries.push(Entry::Tool {
            name: "\u{1b}[31mevil\u{1b}[0m".into(),
            duration_ms: Some(10),
        });
        let out = render(&mut app, 70, 16);
        assert!(out.contains("evil"));
        assert!(!out.contains('\u{1b}'), "escape leaked from a tool name");
    }

    #[test]
    fn ansi_escapes_from_a_peer_never_reach_the_frame() {
        // A cursor-agent style chunk: SGR colour, a cursor move, an erase-line
        // and an OSC title set. Painting any of these raw shears the TUI.
        let dirty = "\u{1b}[32mbuilding\u{1b}[0m\u{1b}[2K\u{1b}[1;5H\u{1b}]0;title\u{7} ok";
        let clean = sanitize_preview(dirty);
        assert_eq!(clean, "building ok");
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.chars().any(char::is_control));
    }

    #[test]
    fn carriage_returns_redraw_rather_than_concatenate() {
        // Progress bars stream "10%\r20%\r30%" — we want the final state only.
        assert_eq!(sanitize_preview("10%\r20%\r30%"), "30%");
        assert_eq!(sanitize_preview("abcX\u{8} "), "abc ");
        assert_eq!(sanitize_preview("a\tb"), "a    b");
    }

    #[test]
    fn peer_preview_clips_by_display_width_not_char_count() {
        // Double-width CJK: 40 chars = 80 columns, must clip to PEER_PREVIEW.
        let wide = "宽".repeat(40);
        let clipped = clip_display(&wide, PEER_PREVIEW);
        assert!(clipped.width() <= PEER_PREVIEW, "{}", clipped.width());
        assert!(clipped.ends_with('…'));
        assert_eq!(clip_display("short", PEER_PREVIEW), "short");
    }

    #[test]
    fn peer_row_shows_the_last_meaningful_line() {
        let buffer = "first\n\u{1b}[31msecond\u{1b}[0m\n\n   \n";
        assert_eq!(last_visible_line(buffer), "second");
    }

    #[test]
    fn garbled_peer_chunk_renders_a_clean_single_row() {
        let mut app = App::new(SessionInfo::default());
        app.animate = false;
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "harness.delegate.chunk",
                "agent": "kimi",
                "delta": "\u{1b}[2K\rreading app.rs\u{1b}[0m\n",
            }),
        });
        let out = render(&mut app, 70, 16);
        assert!(out.contains("reading app.rs"), "{out}");
        assert!(!out.contains('\u{1b}'), "escape leaked into the frame");
        assert!(!out.contains("[2K"), "raw CSI leaked into the frame: {out}");
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
            assert_eq!(slash_route(name, &[]), SlashRoute::Quit, "/{name}");
        }
        for name in [
            "help", "search", "copy", "retry", "edit", "steer", "sessions", "status", "model",
            "effort", "clear",
        ] {
            assert_eq!(slash_route(name, &[]), SlashRoute::Local, "/{name}");
        }
        let packs = [SlashCommand {
            name: "review".into(),
            summary: "review changes".into(),
            exclusive: true,
            aliases: vec![],
        }];
        assert_eq!(slash_route("review", &packs), SlashRoute::Rpc);
        assert_eq!(slash_route("review", &[]), SlashRoute::Unknown);
        assert_eq!(slash_route("bogus", &[]), SlashRoute::Unknown);
        assert_eq!(slash_route("goal", &[]), SlashRoute::Unknown);
    }

    #[test]
    fn fenced_diff_in_prose_is_not_one_card() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::Assistant(
            "Edited `cfg.go`:\n\n```diff\n--- a/cfg.go\n+++ b/cfg.go\n@@ -1 +1 @@\n-old\n+new\n```\n\nThe client now waits a minute.".into(),
        ));
        let out = render(&mut app, 80, 24);
        assert!(out.contains("Edited"), "{out}");
        assert!(out.contains("─ cfg.go"), "{out}");
        assert!(out.contains("waits a minute"), "{out}");
        // Fence markers are consumed by the splitter, not painted as chrome.
        assert!(!out.contains("```diff"), "{out}");
        let prose = out.find("Edited").expect("prose");
        let card = out.find("─ cfg.go").expect("card");
        let after = out.find("waits a minute").expect("trailing prose");
        assert!(prose < card, "prose should precede the card\n{out}");
        assert!(card < after, "trailing prose should follow the card\n{out}");
        // The whole message is not a generic wrapping card.
        assert!(!out.contains("─ diff "), "{out}");
    }

    #[test]
    fn pathless_hunk_is_titled_hunk_not_diff() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::Assistant(
            "Edited `cfg.go`:\n\n```diff\n@@ -1 +1 @@\n-old\n+new\n```\n".into(),
        ));
        let out = render(&mut app, 80, 20);
        assert!(out.contains("─ hunk "), "{out}");
        assert!(!out.contains("─ diff "), "{out}");
        assert!(out.contains("Edited"), "{out}");
    }

    #[test]
    fn add_band_cells_use_add_background() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::Assistant(
            "--- a/x.go\n+++ b/x.go\n@@ -1 +1 @@\n-old\n+new".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Reference the palette constant, not a literal: the band color is a
        // theme concern and this test should survive a flavor tweak.
        let add_bg = crate::theme::mocha::ADD_BAND;
        let mut found = None;
        for y in 0..16u16 {
            let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if row.contains("new") && row.contains('+') {
                found = Some(y);
                break;
            }
        }
        let y = found.expect("add band not painted");
        let mut saw_add = false;
        for x in 0..60u16 {
            if buf[(x, y)].bg == add_bg {
                saw_add = true;
            }
        }
        assert!(saw_add, "add row y={y} had no add-band background");
        // The wash reaches past the text, not a ragged stripe.
        let row: String = (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect();
        let text_at = row.find("new").expect("new");
        assert_eq!(
            buf[(text_at as u16 + 6, y)].bg,
            add_bg,
            "pad after add text"
        );
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
                Entry::Note("⇄ peer · done".into())
            ]
        );
        assert!(app.live.is_empty());
    }

    #[test]
    fn tool_events_add_and_complete_tool_line() {
        let mut app = App::new(SessionInfo::default());
        // Tool events accumulate in the live group while the loop runs; the
        // transcript entry is only committed when the loop (or turn) ends.
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "loop.tool.start", "tool": "grep"
            }),
        });
        assert_eq!(app.live_tools, vec![("grep".to_string(), None)]);
        assert!(app.entries.is_empty(), "not committed mid-turn");
        assert_eq!(app.status, "tool · grep");
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "loop.tool.end", "duration_ms": 400
            }),
        });
        assert_eq!(app.live_tools, vec![("grep".to_string(), Some(400))]);
        // A single call keeps the plain one-row shape it always had.
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "loop.run.end"
            }),
        });
        assert_eq!(
            app.entries,
            vec![Entry::Tool {
                name: "grep".into(),
                duration_ms: Some(400)
            }]
        );
        assert!(app.live_tools.is_empty(), "group consumed on commit");
    }

    #[test]
    fn multi_tool_turn_commits_one_collapsed_group() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type": "loop.run.start"}),
        });
        for (tool, ms) in [
            ("read src/app.rs", 120),
            ("grep estimated_entry_lines", 40),
            ("bash cargo test", 940),
        ] {
            app.on_notification(&Notification {
                method: "event".into(),
                params: serde_json::json!({"type": "loop.tool.start", "tool": tool}),
            });
            app.on_notification(&Notification {
                method: "event".into(),
                params: serde_json::json!({"type": "loop.tool.end", "tool": tool, "duration_ms": ms}),
            });
        }
        assert!(app.entries.is_empty(), "nothing committed mid-turn");
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type": "loop.run.end"}),
        });
        assert_eq!(
            app.entries,
            vec![Entry::Tools {
                tools: vec![
                    ("read src/app.rs".into(), Some(120)),
                    ("grep estimated_entry_lines".into(), Some(40)),
                    ("bash cargo test".into(), Some(940)),
                ],
                expanded: false,
            }]
        );
        // A second commit (the finish_turn fallback) is a safe no-op: the
        // error note is the only new entry, never a duplicate tool group.
        app.finish_turn(Err(crate::rpc::Error::Closed));
        let groups = app
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::Tools { .. }))
            .count();
        assert_eq!(groups, 1, "group committed exactly once");
    }

    #[test]
    fn tool_group_collapsed_paints_one_row_expanded_paints_all() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.entries.push(Entry::User("run the suite".into()));
        app.entries.push(Entry::Tools {
            tools: vec![
                ("read src/app.rs".into(), Some(120)),
                ("grep estimated_entry_lines".into(), Some(40)),
                ("bash cargo test".into(), Some(940)),
            ],
            expanded: false,
        });
        let out = render(&mut app, 80, 20);
        let collapsed_rows: Vec<&str> =
            out.lines().filter(|l| l.contains("3 tool calls")).collect();
        assert_eq!(collapsed_rows.len(), 1, "collapsed group is one row");
        assert!(!out.contains("grep estimated_entry_lines"), "{out}");

        assert!(app.toggle_tool_group(), "t toggles the group");
        let out = render(&mut app, 80, 20);
        assert!(out.contains("grep estimated_entry_lines"), "{out}");
        let tool_rows = out
            .lines()
            .filter(|l| l.contains("read src/app.rs") || l.contains("bash cargo test"))
            .count();
        assert_eq!(tool_rows, 2, "expanded group paints every call");

        assert!(app.toggle_tool_group(), "t collapses again");
        let out = render(&mut app, 80, 20);
        assert!(!out.contains("grep estimated_entry_lines"), "{out}");
    }

    #[test]
    fn tool_group_toggle_has_nothing_to_do_without_a_group() {
        let mut app = App::new(SessionInfo::default());
        assert!(!app.toggle_tool_group(), "no group: key falls through");
        assert!(!app.collapse_tool_group(), "no group to collapse");
        app.entries.push(Entry::Tools {
            tools: vec![("a".into(), Some(1)), ("b".into(), Some(2))],
            expanded: true,
        });
        assert!(app.collapse_tool_group(), "esc collapses the open group");
        assert!(!app.collapse_tool_group(), "second esc has nothing left");
    }

    #[test]
    fn tools_estimate_matches_painted_height() {
        // The scrollbar extent derives from the estimate; a collapsed group
        // must claim exactly the row it paints, an expanded group header+rows.
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme { colored: true };
        app.last_view_w = 40;
        let collapsed = Entry::Tools {
            tools: vec![
                (
                    "bash echo ----; cat AGENTS.md; ls -la; git log --oneline".into(),
                    Some(90),
                ),
                ("write docs/roadmap.md".into(), Some(60)),
                ("read src/render.rs".into(), Some(30)),
            ],
            expanded: false,
        };
        let mut expanded = collapsed.clone();
        if let Entry::Tools { expanded: e, .. } = &mut expanded {
            *e = true;
        }
        for entry in [collapsed, expanded] {
            let painted = app.entry_lines(&entry).len();
            let estimated = app.estimated_entry_lines(&entry) - 1; // minus separator
            assert!(
                estimated >= painted,
                "under-estimated {entry:?}: estimated {estimated}, painted {painted}"
            );
        }
    }

    #[test]
    fn paste_inserts_at_cursor_and_moves_it() {
        let mut app = App::new(SessionInfo::default());
        app.set_input("ab".into());
        app.move_cursor(-1); // between 'a' and 'b'
        app.insert_text("XY\nZ");
        assert_eq!(app.input, "aXY\nZb");
        assert_eq!(app.cursor, 5, "cursor after the pasted text");
        // Paste at the end, like a terminal paste lands after a completed line.
        app.cursor_end();
        app.insert_text(" tail");
        assert_eq!(app.input, "aXY\nZb tail");
    }

    #[test]
    fn home_end_and_delete_edit_like_a_text_field() {
        let mut app = App::new(SessionInfo::default());
        app.set_input("hello".into());
        app.cursor_home();
        assert_eq!(app.cursor, 0);
        app.delete_char(); // 'h' is before the cursor now
        assert_eq!(app.input, "ello");
        app.cursor_end();
        assert_eq!(app.cursor, 4);
        app.delete_char(); // nothing after the end
        assert_eq!(app.input, "ello");
        app.move_cursor(-1);
        app.delete_char();
        assert_eq!(app.input, "ell");
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
        assert!(crate::render::is_unified_diff("@@ -1 +1 @@\n+foo\n-bar"));
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
        let footer = app.footer();
        for part in [" y ", "allow", " n ", "deny", " a ", "always", "write"] {
            assert!(footer.contains(part), "{footer}");
        }
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
    fn copy_state_is_local() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::Assistant("answer".into()));
        assert!(app.copy_last_assistant());
        assert_eq!(app.last_copy, "answer");
        assert!(!app.footer().contains("ctrl+s"), "{}", app.footer());
        // The copy is a local state note, not an engine round trip.
        assert!(app.footer().contains("copied locally"), "{}", app.footer());
    }
}
