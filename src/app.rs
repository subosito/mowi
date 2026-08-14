//! Ratatui document: header, transcript, input, footer.
//!
//! The app owns no Engine. Every state change is either a local key edit or a
//! message from the `mow rpc` child (`rpc::Client`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Flex, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph,
        Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState, Wrap,
    },
};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::render::{Segment, diff_title, markdown_lines, split_markdown_and_diffs};
use crate::rpc::{
    Client, ContextUsage, EVENT_LSP_DIAGNOSTICS, EffortList, Error, LspDiagnostic, LspProblems,
    ModelList, Notification, PermissionRequest, SessionInfo, SessionSummary, SlashCommand,
    TranscriptMessage, decode_lsp_diagnostics, decode_rewind, extract_thinking, reasoning_delta,
    token_delta,
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
/// Extra transcript rows materialized above and below the viewport so a
/// PageUp does not have to rebuild the document from scratch.
const TRANSCRIPT_OVERSCAN: usize = 16;
/// Cached painted entries kept around the viewport. Enough for a burst of
/// scroll keys without retaining a whole resumed session.
const TRANSCRIPT_CACHE_ENTRIES: usize = 48;
/// Bound retained UI source for multi-day sessions. The Engine remains the
/// source of truth and `/transcript` can reload history on demand.
const TRANSCRIPT_ENTRY_CEILING: usize = 4_000;
const TRANSCRIPT_TRIM_TARGET: usize = 3_500;
const EARLIER_ENTRIES_NOTE: &str =
    "… earlier transcript omitted from memory · /transcript reloads history";
/// Bound work per event-loop turn so a continuously streaming RPC cannot
/// starve input and painting. Remaining notifications are handled next turn.
const NOTIFICATION_BATCH: usize = 256;
/// Bound one burst while still clearing typical key-repeat backlogs in a
/// single turn. Navigation repeats are coalesced, so this does not mean 4096
/// expensive transcript recalculations.
const INPUT_BATCH: usize = 4096;
/// Newest retained LSP batches (one per path).
const MAX_LSP_PROBLEM_PATHS: usize = 10;
/// `/lsp` dumps at most this many newest batches.
const MAX_LSP_RECENT_BATCHES: usize = 5;
/// `/lsp` dumps at most this many transcript lines.
const MAX_LSP_RECENT_LINES: usize = 40;

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
    /// Operator prompt. `at` is the wall clock when *this client* recorded
    /// it. Resumed engine history has no per-message time on the wire, so
    /// those entries stay `None` rather than inventing a stamp.
    User {
        text: String,
        at: Option<SystemTime>,
    },
    Assistant(String),
    Note(String),
    Tool {
        name: String,
        duration_ms: Option<u64>,
    },
    /// One turn's worth of tool calls, collapsed to a single summary line
    /// (`bash ×2 · grep`) unless expanded. A long agent turn that runs
    /// `bash`/`write`/`read` a dozen times is one row, not a flood. Esc
    /// collapses an expanded group.
    Tools {
        tools: Vec<(String, Option<u64>)>,
        expanded: bool,
    },
}

impl Entry {
    /// A prompt recorded now. Snapshots and resumed history should set `at`
    /// explicitly (`Some` fixed, or `None`) instead of calling this.
    pub fn user(text: impl Into<String>) -> Self {
        Entry::User {
            text: text.into(),
            at: Some(SystemTime::now()),
        }
    }
}

/// UTC `HH:MM` for a recorded user prompt. UTC keeps the stamp timezone-stable
/// without a datetime crate; resumed entries omit it entirely.
fn parse_rfc3339_system_time(value: &str) -> Option<SystemTime> {
    // Host emits UTC RFC3339. Convert civil days without another time crate;
    // malformed/legacy timestamps simply remain untimed.
    let v = value.strip_suffix('Z')?;
    let (date, clock) = v.split_once('T')?;
    let mut d = date.split('-').map(|n| n.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = clock.split(':');
    let h = t.next()?.parse::<i64>().ok()?;
    let min = t.next()?.parse::<i64>().ok()?;
    let sec = t.next()?.split('.').next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let y0 = y - i64::from(m <= 2);
    let era = if y0 >= 0 { y0 } else { y0 - 399 } / 400;
    let yoe = y0 - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let seconds = days
        .checked_mul(86400)?
        .checked_add(h * 3600 + min * 60 + sec)?;
    (seconds >= 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

fn format_user_stamp(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mins = (secs / 60) % (24 * 60);
    format!("{:02}:{:02}", mins / 60, mins % 60)
}

fn user_display_text(text: &str, at: Option<SystemTime>) -> String {
    match at {
        Some(at) => format!("{} {text}", format_user_stamp(at)),
        None => text.to_string(),
    }
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

/// Left-side identity chips, least important first so the drop loop peels
/// from the front. Session id is never a header chip.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IdentityChip {
    Workspace(String),
    Effort(String),
    Model(String),
}

/// Right-side usage chips, least important first. Tokens sit immediately
/// before the gauge; the gauge is the far-rightmost chip when shown.
/// Safety never drops.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MetricChip {
    Tokens(String),
    Gauge(String),
}

/// Painted lines for one transcript block, reused across the 50ms redraws.
#[derive(Debug, Clone)]
struct CachedPaint {
    bytes: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
}

/// Viewport-local paint cache. Width/theme changes drop it; entries are
/// evicted in bulk past [`TRANSCRIPT_CACHE_ENTRIES`].
#[derive(Debug, Default)]
struct TranscriptCache {
    width: u16,
    colored: bool,
    entries: HashMap<usize, CachedPaint>,
    live: Option<CachedPaint>,
}

impl TranscriptCache {
    fn clear(&mut self) {
        self.entries.clear();
        self.live = None;
    }

    fn matches(&self, width: u16, colored: bool) -> bool {
        self.width == width && self.colored == colored
    }
}

/// Per-entry wrap estimate, keyed by the same fingerprint as the paint cache.
#[derive(Debug, Clone, Copy)]
struct CachedHeight {
    bytes: usize,
    expanded: bool,
    rows: usize,
}

/// Document-height cache. `estimated_total_lines` used to re-wrap every
/// entry (and the whole live buffer) on every 50ms tick; a buried 2k-line
/// answer made that the dominant cost even when the window never painted it.
#[derive(Debug, Default)]
struct HeightCache {
    width: u16,
    entries: Vec<CachedHeight>,
    /// Prefix row offset for each entry plus a final total sentinel.
    row_starts: Vec<usize>,
    entries_total: usize,
    live_bytes: usize,
    live_height: usize,
    live_last_line_start: usize,
    live_last_line_cols: usize,
    live_last_line_head: [u8; 8],
    live_last_line_head_len: u8,
}

impl HeightCache {
    fn clear(&mut self) {
        self.entries.clear();
        self.row_starts.clear();
        self.entries_total = 0;
        self.invalidate_live();
    }

    fn invalidate_live(&mut self) {
        self.live_bytes = 0;
        self.live_height = 0;
        self.live_last_line_start = 0;
        self.live_last_line_cols = 0;
        self.live_last_line_head = [0; 8];
        self.live_last_line_head_len = 0;
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
    /// Peer overlay scroll offset measured upward from the newest output.
    pub peer_scroll: u16,
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
    transcript_cache: RefCell<TranscriptCache>,
    height_cache: RefCell<HeightCache>,
    /// Local RPC operation currently using `slash_rx` (kept off the UI thread).
    pending_local: Option<String>,
    /// Operator cancelled the in-flight turn; queued follow-ups must not start.
    cancelled: bool,
    /// Last-user text from an in-flight `rewind`, used after transcript refresh.
    rewind_user: Option<String>,
    /// OSC52 payload to flush on the next terminal write (not painted).
    pending_osc52: Option<String>,
    /// Raw host-token accumulator for `<think>` extraction (never painted).
    think_raw: String,
    /// Reasoning channel or an open think tag is armed — status says thinking.
    thinking: bool,
    /// Newest LSP batch per path, newest first, capped at `MAX_LSP_PROBLEM_PATHS`.
    lsp_problems: Vec<LspProblems>,
    /// `/compact` requested while a non-control compact would block the worker.
    pending_compact: Option<i64>,
    /// Methods the host answers while a prompt is in flight (empty = unknown).
    control_caps: Vec<String>,
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
            peer_scroll: 0,
            animate: std::env::var_os("MOW_NO_ANIM").is_none(),
            effort: String::new(),
            ctx: None,
            caps: Vec::new(),
            cursor: 0,
            tick: 0,
            scrollbar_state: ScrollbarState::default(),
            transcript_cache: RefCell::new(TranscriptCache::default()),
            height_cache: RefCell::new(HeightCache::default()),
            pending_local: None,
            cancelled: false,
            rewind_user: None,
            pending_osc52: None,
            think_raw: String::new(),
            thinking: false,
            lsp_problems: Vec::new(),
            pending_compact: None,
            control_caps: Vec::new(),
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
                "user" => Entry::User {
                    text: message.content,
                    at: message
                        .timestamp
                        .as_deref()
                        .and_then(parse_rfc3339_system_time),
                },
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
        if let Some(workspace) = status.get("workspace").and_then(Value::as_str)
            && !workspace.is_empty()
        {
            self.session.workspace = workspace.to_string();
        }
        if let Some(session_id) = status.get("session_id").and_then(Value::as_str)
            && !session_id.is_empty()
        {
            self.session.session_id = session_id.to_string();
        }
    }

    /// Curated `/status` note: chips the operator already sees, not raw JSON.
    pub fn status_summary(&self) -> String {
        let mut bits = Vec::new();
        if !self.session.model.is_empty() {
            bits.push(self.session.model.clone());
        }
        let workspace = workspace_basename(&self.session.workspace);
        if !workspace.is_empty() {
            bits.push(workspace);
        }
        bits.push(format!("perm {}", self.mode_chip()));
        bits.push(self.capability_chip());
        if !self.effort.is_empty() {
            bits.push(format!("effort {}", self.effort));
        }
        if self.busy {
            bits.push("busy".into());
        }
        let mut lines = vec![bits.join(" · ")];
        let ctx = self.context_summary();
        if ctx != "context: unknown" {
            lines.push(ctx);
        }
        if self.usage.total() > 0 || self.usage.peer_tokens > 0 {
            lines.push(self.usage.chip());
        }
        if !self.session.session_id.is_empty() {
            lines.push(format!("session {}", self.session.session_id));
        }
        lines.join("\n")
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

    /// Record methods the host answers while a prompt is in flight.
    pub fn set_control_methods(&mut self, methods: &[String]) {
        self.control_caps = methods.to_vec();
    }

    /// `compact` is a worker method on current hosts unless advertised as control.
    pub fn compact_is_control(&self) -> bool {
        self.control_caps
            .iter()
            .any(|method| method.eq_ignore_ascii_case("compact"))
    }

    /// Feature-detect a method; an empty list (older server) means assume yes.
    pub fn supports(&self, method: &str) -> bool {
        self.caps.is_empty() || self.caps.iter().any(|m| m == method)
    }

    /// Store a `context` result for the gauge.
    pub fn apply_context(&mut self, usage: &ContextUsage) {
        self.ctx = Some(usage.clone());
    }

    /// Keep the newest batch per path and paint one summary line — not every finding.
    fn ingest_lsp_problems(&mut self, problems: LspProblems) {
        self.lsp_problems
            .retain(|existing| existing.path != problems.path);
        self.lsp_problems.insert(0, problems.clone());
        if self.lsp_problems.len() > MAX_LSP_PROBLEM_PATHS {
            self.lsp_problems.truncate(MAX_LSP_PROBLEM_PATHS);
        }
        let note = format!("lsp · {} · {} problem(s)", problems.path, problems.count);
        self.status = note.clone();
        self.entries.push(Entry::Note(note));
    }

    /// Dump retained batches without replaying an unbounded session transcript.
    fn show_lsp_problems(&mut self) {
        if self.lsp_problems.is_empty() {
            self.status = "lsp · none".into();
            self.entries.push(Entry::Note("lsp · none".into()));
            return;
        }
        let mut lines = 0usize;
        let mut batches = 0usize;
        let mut notes = Vec::new();
        for problems in &self.lsp_problems {
            if batches >= MAX_LSP_RECENT_BATCHES || lines >= MAX_LSP_RECENT_LINES {
                break;
            }
            notes.push(format!(
                "lsp · {} · {} problem(s)",
                problems.path, problems.count
            ));
            lines += 1;
            for diagnostic in &problems.diagnostics {
                if lines >= MAX_LSP_RECENT_LINES {
                    break;
                }
                notes.push(lsp_diagnostic_text(&problems.path, diagnostic));
                lines += 1;
            }
            batches += 1;
        }
        if batches < self.lsp_problems.len() || lines >= MAX_LSP_RECENT_LINES {
            notes.push("lsp · …older omitted".into());
        }
        self.status = format!("lsp · {} file(s)", batches);
        self.entries.push(Entry::Note(notes.join("\n")));
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

    /// Left-side identity in **drop order**: least important first.
    ///
    /// Workspace is the directory name, not the full path. Effort rides with
    /// the model visually (`model (effort)`) but still peels first. The model
    /// is last — "which brain am I talking to" is the last identity to go.
    fn identity_chips(&self) -> Vec<IdentityChip> {
        let mut chips = Vec::new();
        let workspace = workspace_basename(&self.session.workspace);
        if !workspace.is_empty() {
            chips.push(IdentityChip::Workspace(workspace));
        }
        if !self.effort.is_empty() {
            chips.push(IdentityChip::Effort(self.effort.clone()));
        }
        if !self.session.model.is_empty() {
            chips.push(IdentityChip::Model(self.session.model.clone()));
        }
        chips
    }

    /// Right-side usage chips in **drop order**: tokens first, then the gauge.
    /// Tokens peel first even though they paint immediately before the gauge.
    ///
    /// Below `GAUGE_MIN_COLS` the meter is not offered at all, so leftover
    /// columns go to identity and safety rather than a bar.
    fn metric_chips(&self, width: u16) -> Vec<MetricChip> {
        let mut chips = Vec::new();
        if self.usage.total() > 0 || self.usage.peer_tokens > 0 {
            chips.push(MetricChip::Tokens(self.usage.chip()));
        }
        if width >= GAUGE_MIN_COLS
            && let Some(ctx) = self.context_chip()
        {
            chips.push(MetricChip::Gauge(ctx));
        }
        chips
    }

    /// Identity side of the header: `mowi · basename · model (effort)`.
    /// Only the effort word is dimmed; the parentheses keep the header style.
    fn header_identity_spans(&self, identity: &[IdentityChip]) -> Vec<Span<'static>> {
        let header = self.theme.header();
        let fold_effort = identity
            .iter()
            .any(|chip| matches!(chip, IdentityChip::Effort(_)))
            && identity
                .iter()
                .any(|chip| matches!(chip, IdentityChip::Model(_)));
        let effort = identity.iter().find_map(|chip| match chip {
            IdentityChip::Effort(text) => Some(text.as_str()),
            _ => None,
        });

        let mut groups: Vec<Vec<Span<'static>>> = vec![vec![Span::styled("mowi", header)]];
        for chip in identity {
            match chip {
                IdentityChip::Effort(_) if fold_effort => {}
                IdentityChip::Model(name) if fold_effort => {
                    groups.push(vec![
                        Span::styled(name.clone(), header),
                        Span::styled(" (", header),
                        Span::styled(
                            effort.unwrap_or_default().to_string(),
                            self.theme.note().patch(self.theme.header_bg()),
                        ),
                        Span::styled(")", header),
                    ]);
                }
                IdentityChip::Workspace(text)
                | IdentityChip::Effort(text)
                | IdentityChip::Model(text) => {
                    groups.push(vec![Span::styled(text.clone(), header)]);
                }
            }
        }

        let mut spans = Vec::new();
        for (i, group) in groups.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", header));
            }
            spans.extend(group);
        }
        spans
    }

    fn header_token_spans(&self, metrics: &[MetricChip]) -> Vec<Span<'static>> {
        metrics
            .iter()
            .find_map(|chip| match chip {
                MetricChip::Tokens(text) => Some(vec![Span::styled(
                    text.clone(),
                    self.theme.note().patch(self.theme.header_bg()),
                )]),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn header_gauge_spans(&self, metrics: &[MetricChip]) -> Vec<Span<'static>> {
        metrics
            .iter()
            .find_map(|chip| match chip {
                MetricChip::Gauge(text) => Some(vec![Span::styled(
                    text.clone(),
                    self.theme
                        .badge(self.context_tone())
                        .patch(self.theme.header_bg()),
                )]),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Header as spans. Left is identity only. Token usage sits immediately
    /// before the context gauge; the gauge is the far-rightmost chip
    /// whenever it is shown. A ` · ` joins safety to the first metric and
    /// is omitted when both metrics are hidden. Usage drops before
    /// identity. Safety never drops. Session id is not painted here.
    pub fn header_line(&self, width: u16) -> Line<'static> {
        let safety = format!("{} · {}", self.capability_chip(), self.mode_chip());
        let mut identity = self.identity_chips();
        let mut metrics = self.metric_chips(width);
        let width = width as usize;
        let safety_w = Span::raw(safety.as_str()).width();
        let chip = self.theme.chip().patch(self.theme.header_bg());
        loop {
            let left = self.header_identity_spans(&identity);
            let token_spans = self.header_token_spans(&metrics);
            let gauge_spans = self.header_gauge_spans(&metrics);
            let left_w: usize = left.iter().map(Span::width).sum();
            let token_w: usize = token_spans.iter().map(Span::width).sum();
            let gauge_w: usize = gauge_spans.iter().map(Span::width).sum();
            let sep = if token_w > 0 || gauge_w > 0 {
                " · "
            } else {
                ""
            };
            let sep_w = Span::raw(sep).width();
            let metric_gap = if token_w > 0 && gauge_w > 0 { 2 } else { 0 };
            let right_w = safety_w + sep_w + token_w + metric_gap + gauge_w;
            if left_w + right_w <= width || (identity.is_empty() && metrics.is_empty()) {
                let pad = width.saturating_sub(left_w + right_w);
                let mut spans = left;
                spans.push(Span::styled(" ".repeat(pad), self.theme.header_bg()));
                spans.push(Span::styled(safety, chip));
                if !sep.is_empty() {
                    spans.push(Span::styled(sep, chip));
                }
                spans.extend(token_spans);
                if metric_gap > 0 {
                    spans.push(Span::styled("  ", self.theme.header_bg()));
                }
                spans.extend(gauge_spans);
                return Line::from(spans);
            }
            if !metrics.is_empty() {
                metrics.remove(0);
            } else if !identity.is_empty() {
                identity.remove(0);
            }
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

        // One live clock, on this row. Busy owns spinner + elapsed + verb
        // (and the typing pulse while tokens land). Idle is a state light.
        // Session identity lives in the help overlay, not here.
        if self.busy {
            left.extend(self.live_clock_spans());
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
        // Idle-only news: while busy the verb already carries status.
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

        let mut chosen_hint = None;
        for hint in hints {
            let hint_w = Span::raw(hint.as_str()).width();
            // +2 keeps a breathing gap between state and hints.
            if left_w + hint_w + 2 <= width {
                chosen_hint = Some(hint);
                break;
            }
        }

        let left_w: usize = left.iter().map(Span::width).sum();
        let mut spans = left;
        if let Some(hint) = chosen_hint {
            let hint_w = Span::raw(hint.as_str()).width();
            let pad = width.saturating_sub(left_w + hint_w);
            spans.push(Span::styled(" ".repeat(pad), self.theme.chrome()));
            spans.extend(styled_key_hint(&hint, self.theme));
        }
        Line::from(spans)
    }

    /// Key hints, widest first: the widest one that still fits is painted.
    ///
    /// The enter verb tracks state — `send` when idle, `queue` while a turn
    /// is running — so the footer does not advertise a send that will not
    /// happen.
    fn footer_hints(&self) -> Vec<String> {
        let enter = if self.busy {
            "enter queue"
        } else {
            "enter send"
        };
        vec![
            format!("{enter} · esc cancel · ↑↓ scroll · ? help"),
            format!("{enter} · esc cancel · ? help"),
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

    fn entry_lines(&self, entry: &Entry) -> Vec<Line<'static>> {
        let width = self.last_view_w.max(8) as usize;
        match entry {
            Entry::User { text, at } => self.user_lines(text, *at),
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
                // Collapsed (the default): one counted row (`bash ×2 · grep`)
                // so a busy turn stays readable. Expanded: a header plus one
                // row per call, same visual language as a plain Tool entry.
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
                    let text = collapsed_tool_group_text(tools, width);
                    wrap_styled_line(Line::styled(text, self.theme.note()), width)
                }
            }
        }
    }

    /// User prompt as a full-width filled band: accent bar, padded body, blank
    /// rows above and below so it reads as a message, not a leftover prompt.
    /// A recorded `at` becomes an inline muted `HH:MM` on the first row.
    fn user_lines(&self, text: &str, at: Option<SystemTime>) -> Vec<Line<'static>> {
        let width = self.last_view_w.max(8) as usize;
        let stamp = at.map(format_user_stamp);
        let display = user_display_text(text, at);
        let blank = self.user_band_row("", width, None);
        let mut out = vec![blank.clone()];
        let mut first = true;
        for raw in display.split('\n') {
            let inner = width.saturating_sub(3).max(1); // accent + gutters
            for chunk in wrap_cols(raw, inner) {
                let row_stamp = if first { stamp.as_deref() } else { None };
                first = false;
                out.push(self.user_band_row(&chunk, width, row_stamp));
            }
        }
        out.push(blank);
        out
    }

    fn user_band_row(&self, body: &str, width: usize, stamp: Option<&str>) -> Line<'static> {
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
        if let Some(stamp) = stamp.filter(|stamp| text.contains(*stamp)) {
            let muted = format!(" {stamp} ");
            let (head, tail) = if let Some(rest) = text.strip_prefix(muted.as_str()) {
                (muted, rest.to_string())
            } else if let Some(rest) = text.strip_prefix(&format!(" {stamp}")) {
                (format!(" {stamp}"), rest.to_string())
            } else {
                (String::new(), text)
            };
            if !head.is_empty() {
                spans.push(Span::styled(
                    head,
                    self.theme.timing().patch(self.theme.user_bg()),
                ));
            }
            if !tail.is_empty() {
                spans.push(Span::styled(tail, self.theme.user()));
            }
        } else if !text.is_empty() {
            spans.push(Span::styled(text, self.theme.user()));
        }
        let painted: usize = spans.iter().map(Span::width).sum();
        let pad = width.saturating_sub(painted);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), self.theme.user()));
        }
        Line::from(spans).style(self.theme.user_bg())
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
    ///
    /// Entries (and live) that overlap the window are painted, then sliced to
    /// the overlapping rows. A buried 2k-line answer must not become 2k
    /// `Line`s on every PageUp, and a streaming `live` buffer must not be
    /// re-parsed when the operator has scrolled into earlier history.
    fn visible_transcript_lines(&self) -> (Vec<Line<'static>>, u16) {
        let (total_lines, live_height) = self.ensure_heights();
        let entries_height = total_lines.saturating_sub(live_height);
        let viewport = self.last_view_h.max(1) as usize;
        let target = if self.follow {
            total_lines.saturating_sub(viewport + TRANSCRIPT_OVERSCAN)
        } else {
            (self.scroll as usize).saturating_sub(TRANSCRIPT_OVERSCAN)
        };
        let window_end = target + viewport + (TRANSCRIPT_OVERSCAN * 2);
        let mut lines = Vec::new();
        let mut base = target;
        // Jump directly to the first overlapping entry. Scrolling used to
        // scan/clone every height from row zero on each key repeat, which made
        // resumed sessions feel much slower than the Go viewport.
        let entry_window: Vec<(usize, usize)> = {
            let cache = self.height_cache.borrow();
            let first = cache
                .row_starts
                .partition_point(|start| *start <= target)
                .saturating_sub(1)
                .min(cache.entries.len());
            (first..cache.entries.len())
                .take_while(|index| cache.row_starts[*index] < window_end)
                .map(|index| (index, cache.row_starts[index]))
                .collect()
        };
        for (index, cursor) in entry_window {
            self.push_entry_window(&mut lines, &mut base, index, cursor, target, window_end);
        }
        if live_height > 0 && entries_height < window_end && entries_height + live_height > target {
            self.push_live_window(
                &mut lines,
                &mut base,
                entries_height,
                live_height,
                target,
                window_end,
            );
        }
        if !self.peers.is_empty() {
            let peer_at = entries_height + live_height;
            if peer_at < window_end {
                let painted = self.peer_lines();
                push_window_slice(&mut lines, &mut base, peer_at, target, window_end, &painted);
            }
        }
        (lines, base.min(u16::MAX as usize) as u16)
    }

    fn push_entry_window(
        &self,
        out: &mut Vec<Line<'static>>,
        base: &mut usize,
        index: usize,
        cursor: usize,
        target: usize,
        window_end: usize,
    ) {
        let (bytes, expanded) = match self.entries.get(index) {
            Some(entry) => entry_fingerprint(entry),
            None => return,
        };
        {
            let cache = self.transcript_cache.borrow();
            if cache.matches(self.last_view_w, self.theme.colored)
                && let Some(hit) = cache.entries.get(&index)
                && hit.bytes == bytes
                && hit.expanded == expanded
            {
                push_window_slice_with_sep(out, base, cursor, target, window_end, &hit.lines);
                return;
            }
        }
        let painted = self.entry_lines(&self.entries[index]);
        self.store_entry_paint(index, bytes, expanded, painted.clone());
        push_window_slice_with_sep(out, base, cursor, target, window_end, &painted);
    }

    fn push_live_window(
        &self,
        out: &mut Vec<Line<'static>>,
        base: &mut usize,
        entries_height: usize,
        live_height: usize,
        target: usize,
        window_end: usize,
    ) {
        let from = target.saturating_sub(entries_height);
        let to = window_end.saturating_sub(entries_height).min(live_height);
        let (painted, cursor) = self.live_window_lines(from, to, entries_height, live_height);
        push_window_slice(out, base, cursor, target, window_end, &painted);
    }

    /// Paint the live overlap. A follow-mode tail only parses a source suffix
    /// so a multi-thousand-line stream does not re-run markdown every tick.
    fn live_window_lines(
        &self,
        from: usize,
        to: usize,
        entries_height: usize,
        live_height: usize,
    ) -> (Vec<Line<'static>>, usize) {
        let need = to.saturating_sub(from).max(1);
        let wants_tail = from > 0 && to + TRANSCRIPT_OVERSCAN >= live_height;
        if wants_tail {
            let width = self.last_view_w.max(8) as usize;
            let source = live_tail_source(&self.live, need, width);
            if source.len() < self.live.len() {
                let painted = self.assistant_lines(source);
                let tail_est = estimated_wrapped_lines(source, width) + 1;
                let cursor = entries_height + live_height.saturating_sub(tail_est);
                return (painted, cursor);
            }
        }
        (self.cached_live_lines(), entries_height)
    }

    fn cached_live_lines(&self) -> Vec<Line<'static>> {
        let bytes = self.live.len();
        if let Some(lines) = self.cached_paint(true, 0, bytes, false) {
            return lines;
        }
        let lines = self.assistant_lines(self.live.as_str());
        self.store_live_paint(bytes, lines.clone());
        lines
    }

    fn cached_paint(
        &self,
        live: bool,
        index: usize,
        bytes: usize,
        expanded: bool,
    ) -> Option<Vec<Line<'static>>> {
        let cache = self.transcript_cache.borrow();
        if !cache.matches(self.last_view_w, self.theme.colored) {
            return None;
        }
        let hit = if live {
            cache.live.as_ref()
        } else {
            cache.entries.get(&index)
        }?;
        if hit.bytes == bytes && hit.expanded == expanded {
            Some(hit.lines.clone())
        } else {
            None
        }
    }

    fn store_entry_paint(
        &self,
        index: usize,
        bytes: usize,
        expanded: bool,
        lines: Vec<Line<'static>>,
    ) {
        let mut cache = self.transcript_cache.borrow_mut();
        self.prepare_cache(&mut cache);
        if cache.entries.len() >= TRANSCRIPT_CACHE_ENTRIES {
            cache.entries.clear();
        }
        cache.entries.insert(
            index,
            CachedPaint {
                bytes,
                expanded,
                lines,
            },
        );
    }

    fn store_live_paint(&self, bytes: usize, lines: Vec<Line<'static>>) {
        let mut cache = self.transcript_cache.borrow_mut();
        self.prepare_cache(&mut cache);
        cache.live = Some(CachedPaint {
            bytes,
            expanded: false,
            lines,
        });
    }

    fn prepare_cache(&self, cache: &mut TranscriptCache) {
        if !cache.matches(self.last_view_w, self.theme.colored) {
            cache.clear();
            cache.width = self.last_view_w;
            cache.colored = self.theme.colored;
        }
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
        (match entry {
            // The user band reserves the accent rail plus its gutters, and
            // pads with a blank row above and below.
            Entry::User { text, at } => {
                estimated_wrapped_lines(&user_display_text(text, *at), width.saturating_sub(3)) + 2
            }
            Entry::Assistant(text) => estimated_wrapped_lines(text, width),
            Entry::Note(text) => estimated_wrapped_lines(text, width),
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
                    // The collapsed summary is fitted to `width` (whole tokens
                    // only). Count the same string the painter emits so the
                    // estimate never reports fewer rows than the group paints.
                    let text = collapsed_tool_group_text(tools, width);
                    estimated_wrapped_lines(&text, width)
                }
            }
        }) + 1
    }

    /// Live text is a real document block. Counting it as one row made
    /// follow/scroll math treat a multi-thousand-line stream as a single
    /// line, so every frame rematerialized the whole buffer.
    fn enforce_transcript_memory_ceiling(&mut self) {
        if self.entries.len() <= TRANSCRIPT_ENTRY_CEILING {
            return;
        }
        let keep = TRANSCRIPT_TRIM_TARGET.min(self.entries.len());
        let drop_n = self.entries.len().saturating_sub(keep);
        self.entries.drain(..drop_n);
        if !matches!(self.entries.first(), Some(Entry::Note(note)) if note == EARLIER_ENTRIES_NOTE)
        {
            self.entries
                .insert(0, Entry::Note(EARLIER_ENTRIES_NOTE.into()));
        }
        self.transcript_cache.borrow_mut().clear();
        self.height_cache.borrow_mut().clear();
        if !self.follow {
            self.scroll = self
                .scroll
                .saturating_sub(drop_n.min(u16::MAX as usize) as u16);
        }
    }

    fn estimated_live_lines(&self) -> usize {
        self.ensure_heights().1
    }

    fn estimated_total_lines(&self) -> usize {
        self.ensure_heights().0
    }

    /// `(total, live)` row counts, reused across the 50ms tick.
    fn ensure_heights(&self) -> (usize, usize) {
        let mut cache = self.height_cache.borrow_mut();
        if cache.width != self.last_view_w {
            cache.clear();
            cache.width = self.last_view_w;
        }
        let mut entries_changed = false;
        if cache.entries.len() > self.entries.len() {
            cache.entries.truncate(self.entries.len());
            entries_changed = true;
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let (bytes, expanded) = entry_fingerprint(entry);
            if cache
                .entries
                .get(index)
                .is_some_and(|hit| hit.bytes == bytes && hit.expanded == expanded)
            {
                continue;
            }
            let rec = CachedHeight {
                bytes,
                expanded,
                rows: self.estimated_entry_lines(entry),
            };
            if index < cache.entries.len() {
                cache.entries[index] = rec;
            } else {
                cache.entries.push(rec);
            }
            entries_changed = true;
        }
        if entries_changed || cache.row_starts.len() != cache.entries.len() + 1 {
            let rows: Vec<usize> = cache.entries.iter().map(|height| height.rows).collect();
            cache.row_starts.clear();
            cache.row_starts.reserve(rows.len() + 1);
            let mut cursor = 0usize;
            cache.row_starts.push(cursor);
            for rows in rows {
                cursor = cursor.saturating_add(rows);
                cache.row_starts.push(cursor);
            }
            cache.entries_total = cursor;
        }
        let live = self.ensure_live_height(&mut cache);
        (cache.entries_total + live, live)
    }

    fn ensure_live_height(&self, cache: &mut HeightCache) -> usize {
        if self.live.is_empty() {
            cache.invalidate_live();
            return 0;
        }
        let width = self.last_view_w.max(8) as usize;
        if cache.live_bytes == self.live.len() && cache.live_height > 0 {
            return cache.live_height;
        }
        if self.live_is_append(cache) {
            self.extend_live_height(cache, width);
        } else {
            self.recompute_live_height(cache, width);
        }
        cache.live_height
    }

    fn live_is_append(&self, cache: &HeightCache) -> bool {
        if cache.live_bytes == 0 || self.live.len() < cache.live_bytes {
            return false;
        }
        if cache.live_last_line_start > cache.live_bytes {
            return false;
        }
        if cache.live_last_line_start > 0
            && self.live.as_bytes().get(cache.live_last_line_start - 1) != Some(&b'\n')
        {
            return false;
        }
        let head_len = cache.live_last_line_head_len as usize;
        let start = cache.live_last_line_start;
        let Some(slice) = self.live.get(start..start.saturating_add(head_len)) else {
            return false;
        };
        slice.as_bytes() == &cache.live_last_line_head[..head_len]
    }

    fn recompute_live_height(&self, cache: &mut HeightCache, width: usize) {
        cache.live_height = estimated_wrapped_lines(&self.live, width) + 1;
        cache.live_bytes = self.live.len();
        self.store_live_last_line(cache);
    }

    fn extend_live_height(&self, cache: &mut HeightCache, width: usize) {
        let start = cache.live_last_line_start;
        let old_last_rows = if cache.live_last_line_cols == 0 {
            1
        } else {
            cache.live_last_line_cols.div_ceil(width.max(1))
        };
        let Some(tail) = self.live.get(start..) else {
            self.recompute_live_height(cache, width);
            return;
        };
        let new_tail = estimated_wrapped_lines(tail, width);
        cache.live_height = cache.live_height.saturating_sub(old_last_rows) + new_tail;
        cache.live_bytes = self.live.len();
        self.store_live_last_line(cache);
    }

    fn store_live_last_line(&self, cache: &mut HeightCache) {
        let start = self.live.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let last = &self.live[start..];
        cache.live_last_line_start = start;
        cache.live_last_line_cols = last.width();
        let bytes = last.as_bytes();
        let n = bytes.len().min(8);
        cache.live_last_line_head = [0; 8];
        cache.live_last_line_head[..n].copy_from_slice(&bytes[..n]);
        cache.live_last_line_head_len = n as u8;
    }

    /// Plain-text live clock (styling dropped) — used by tests.
    pub fn activity(&self) -> String {
        self.activity_line()
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Styled live clock: spinner, elapsed, verb and (when streaming) pulse
    /// each keep their own role, so the status bar has hierarchy instead of
    /// reading as one muted sentence.
    pub fn activity_line(&self) -> Line<'static> {
        if !self.busy {
            return Line::default();
        }
        Line::from(self.live_clock_spans())
    }

    /// Spans for the running turn: spinner · elapsed · verb [pulse].
    ///
    /// Painted on the status bar so there is one clock, not a band plus a
    /// second state word ticking out of step with it.
    fn live_clock_spans(&self) -> Vec<Span<'static>> {
        let elapsed = self.elapsed().unwrap_or_else(|| "0.0s".into());
        let status = self.status_or_default();
        let status_style = if is_tool_activity_status(&status) {
            self.theme.tool()
        } else {
            self.theme.text()
        };
        let mut spans = vec![
            Span::styled(format!("{} ", self.spinner_frame()), self.theme.spinner()),
            Span::styled(elapsed, self.theme.timing()),
            Span::styled(" · ", self.theme.chrome()),
            Span::styled(status.to_string(), status_style),
        ];
        // The pulse only runs while tokens are actually landing, so a stalled
        // turn looks different from a streaming one.
        if !self.live.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                self.typing_frame().to_string(),
                self.theme.typing(),
            ));
        }
        spans
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
                self.peer_scroll = 0;
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
        self.transcript_cache.borrow_mut().clear();
        self.height_cache.borrow_mut().clear();
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
                self.reset_think_state();
            }
            "loop.run.end" | "run.end" => {
                self.busy = false;
                self.status.clear();
                self.activity_started = None;
                self.thinking = false;
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
                    self.status = activity_tool_label(name);
                    self.live_tools.push((name.to_string(), None));
                }
            }
            k if k == EVENT_LSP_DIAGNOSTICS || k.ends_with("lsp.diagnostics") => {
                if let Some(problems) = decode_lsp_diagnostics(params) {
                    self.ingest_lsp_problems(problems);
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
        if reasoning_delta(params).is_some() {
            self.arm_thinking();
        }
        if let Some(d) = token_delta(params) {
            self.push_visible_token(d);
        }
    }

    fn reset_think_state(&mut self) {
        self.think_raw.clear();
        self.thinking = false;
    }

    fn arm_thinking(&mut self) {
        self.thinking = true;
        if self.live.is_empty() && !is_tool_activity_status(&self.status) {
            self.status = "thinking".into();
        }
    }

    /// Ingest a host-token delta: strip think wrappers, hide an open block,
    /// and never let the reasoning body reach `live`.
    fn push_visible_token(&mut self, delta: &str) {
        self.think_raw.push_str(delta);
        let (visible, think, unclosed) = extract_thinking(&self.think_raw);
        if !think.is_empty() || unclosed {
            self.arm_thinking();
        }
        if unclosed {
            if !self.live.is_empty() {
                self.live.clear();
            }
            return;
        }
        if visible != self.live {
            self.live = visible;
            if !self.live.is_empty() && self.status == "thinking" {
                self.status.clear();
            }
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

    /// Expand/collapse the most recent tool group. No letter key binds this —
    /// `t` always types — so this is a view helper (tests, and Esc collapse).
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
        self.thinking = false;
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
                let raw = if !text.is_empty() {
                    text.to_string()
                } else {
                    std::mem::take(&mut self.live)
                };
                self.live.clear();
                let body = visible_answer(&raw);
                if !body.trim().is_empty() {
                    self.entries.push(Entry::Assistant(body));
                }
            }
            Err(e) => {
                if !self.live.trim().is_empty() {
                    let body = visible_answer(&std::mem::take(&mut self.live));
                    if !body.trim().is_empty() {
                        self.entries.push(Entry::Assistant(body));
                    }
                }
                self.live.clear();
                self.entries.push(Entry::Note(format!("error: {e}")));
            }
        }
        self.reset_think_state();
        self.status.clear();
        self.finish_peers();
        if self.follow {
            self.scroll = u16::MAX;
        }
    }

    /// Discard queued follow-ups. Cancelled turns must not send them later.
    pub fn drop_queue(&mut self) -> usize {
        let n = self.queue.len();
        self.queue.clear();
        if n > 0 {
            let msg = format!("cancelled · dropped {n} queued message(s)");
            self.status = msg.clone();
            self.entries.push(Entry::Note(msg));
        }
        n
    }

    /// Mark the in-flight turn cancelled and drop any parked prompts.
    pub fn request_cancel(&mut self) {
        self.cancelled = true;
        self.drop_queue();
    }

    /// Start the next queued prompt only after a successful, uncancelled turn.
    pub fn take_queued_after_turn(&mut self, completed_ok: bool) -> Option<String> {
        if self.cancelled || !completed_ok {
            self.cancelled = false;
            None
        } else {
            self.next_queued_prompt()
        }
    }

    /// Remove the most recent painted turn (last user prompt through the end).
    fn drop_last_turn_entries(&mut self) {
        if let Some(i) = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::User { .. }))
        {
            self.entries.truncate(i);
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
            Entry::User { text, .. } => Some(text.clone()),
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
        self.last_copy = text.clone();
        match osc52_sequence(&text) {
            Some(seq) => {
                self.pending_osc52 = Some(seq);
                self.status = "copied".into();
                true
            }
            None => {
                self.status = "copy failed — select in the terminal instead".into();
                false
            }
        }
    }

    fn take_osc52(&mut self) -> Option<String> {
        self.pending_osc52.take()
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
        slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
    ) -> Result<(), Error> {
        if let Some(permission) = self.pending_perm.take() {
            self.perm_shown = None;
            self.status = format!("{} {}", permission.name, decision);
            *slash_rx = Some(client.request_perm_decide(&permission.id, decision)?);
            self.pending_local = Some("perm.decide".into());
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
            ("↑ / ↓", "scroll transcript"),
            ("pgup / pgdn", "scroll transcript"),
            ("ctrl+l", "clear transcript (engine history kept)"),
            ("shift+tab", "ask ↔ auto"),
            ("ctrl+p", "expand peer output"),
            ("home / end", "cursor to start / end"),
            ("delete", "delete forward"),
            ("ctrl+/ or ?", "this help"),
            ("esc", "dismiss overlay, else collapse tools, else cancel"),
            ("ctrl+c", "quit (cancel first if busy)"),
            ("tab (on /)", "slash autocomplete"),
            ("/edit", "rewind last turn into the composer"),
            ("/steer", "guide the running turn (while busy)"),
            ("/btw", "aside — not added to context"),
            ("/model", "list models, or /model <id> to set"),
            ("/effort", "list efforts, or /effort high to set"),
            ("/clear", "clear transcript (engine history kept)"),
            ("/quit", "quit"),
            ("/status /lsp", "session summary · diagnostics"),
            ("/perm /compact", "ask/auto · compact history"),
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
            Overlay::Peer => {
                if delta < 0 {
                    self.peer_scroll = self.peer_scroll.saturating_add(delta.unsigned_abs() as u16);
                } else {
                    self.peer_scroll = self.peer_scroll.saturating_sub(delta as u16);
                }
            }
            Overlay::None => {}
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
                "user" => Entry::User {
                    text: message.content,
                    at: message
                        .timestamp
                        .as_deref()
                        .and_then(parse_rfc3339_system_time),
                },
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

fn visible_answer(text: &str) -> String {
    extract_thinking(text).0
}

/// OSC52 clipboard write. Not a colour sequence — emitted even under NO_COLOR.
/// Mouse capture stays off so native terminal select/copy still works.
fn osc52_sequence(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    const MAX: usize = 100_000;
    let bytes = text.as_bytes();
    let slice = if bytes.len() > MAX {
        &bytes[..MAX]
    } else {
        bytes
    };
    Some(format!("\x1b]52;c;{}\x07", base64_encode(slice)))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn flush_clipboard(app: &mut App) {
    let Some(seq) = app.take_osc52() else {
        return;
    };
    let mut out = std::io::stdout();
    if out
        .write_all(seq.as_bytes())
        .and_then(|_| out.flush())
        .is_err()
    {
        app.status = "copy failed — select in the terminal instead".into();
    }
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

/// Last path component, so the header can name the workspace without the
/// parent directories eating the row.
fn workspace_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// Truncate to `max` display columns, appending `…` when clipped.
/// Paint key names with accent while action words recede.
fn styled_key_hint(hint: &str, theme: Theme) -> Vec<Span<'static>> {
    const KEYS: &[&str] = &["enter", "esc", "↑↓", "?", "pgup", "pgdn"];
    hint.split_inclusive(' ')
        .map(|part| {
            let token = part.trim_end();
            let style = if KEYS.contains(&token) {
                theme.chip()
            } else if token == "·" {
                theme.chrome()
            } else {
                theme.note()
            };
            Span::styled(part.to_string(), style)
        })
        .collect()
}

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

/// Collapsed tool-group counts in first-seen verb order: `bash ×2 · grep`.
///
/// A verb that appears once stays bare (`grep`, never `grep ×1`). The verb is
/// the same first token `tool_label` already uses, so a shell chain still
/// counts as `bash` rather than as its script.
fn tool_group_counts(tools: &[(String, Option<u64>)]) -> Vec<(String, usize)> {
    let mut counts = Vec::new();
    for (name, _) in tools {
        let verb = tool_label(name).0;
        if verb.is_empty() {
            continue;
        }
        if let Some((_, n)) = counts.iter_mut().find(|(v, _)| *v == verb) {
            *n += 1;
        } else {
            counts.push((verb, 1));
        }
    }
    counts
}

fn tool_group_item(verb: &str, count: usize, with_count: bool) -> String {
    if with_count && count > 1 {
        format!("{verb} ×{count}")
    } else {
        verb.to_string()
    }
}

fn join_tool_group_items(items: &[(String, usize)], with_counts: bool) -> String {
    items
        .iter()
        .map(|(verb, count)| tool_group_item(verb, *count, with_counts))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Pack whole tokens from the left. If anything is omitted, the tail is `…`
/// — never a mid-verb or mid-`×N` cut.
fn pack_tool_group_items(
    items: &[(String, usize)],
    max: usize,
    with_counts: bool,
) -> Option<String> {
    if max == 0 || items.is_empty() {
        return None;
    }
    let mut included: Vec<String> = Vec::new();
    for (i, (verb, count)) in items.iter().enumerate() {
        let mut trial = included.clone();
        trial.push(tool_group_item(verb, *count, with_counts));
        let rest = i + 1 < items.len();
        let text = if rest {
            format!("{} · …", trial.join(" · "))
        } else {
            trial.join(" · ")
        };
        if text.width() <= max {
            included = trial;
        } else {
            break;
        }
    }
    if included.is_empty() {
        return None;
    }
    if included.len() < items.len() {
        Some(format!("{} · …", included.join(" · ")))
    } else {
        Some(included.join(" · "))
    }
}

/// Ordered counts with no width budget: `bash ×2 · grep`.
fn tool_group_summary(tools: &[(String, Option<u64>)]) -> String {
    join_tool_group_items(&tool_group_counts(tools), true)
}

/// Same counts, fitted to `max` display columns without mid-token cuts.
///
/// Full counted form first; if that overruns, keep left-hand counts and
/// drop the tail to `…` (`bash ×2 · grep · …`). If even one counted token
/// plus the ellipsis is too wide, fall back to verbs only.
fn tool_group_summary_for_width(tools: &[(String, Option<u64>)], max: usize) -> String {
    let items = tool_group_counts(tools);
    if items.is_empty() {
        return String::new();
    }
    let counted = join_tool_group_items(&items, true);
    if counted.width() <= max {
        return counted;
    }
    if let Some(packed) = pack_tool_group_items(&items, max, true) {
        return packed;
    }
    let verbs = join_tool_group_items(&items, false);
    if verbs.width() <= max {
        return verbs;
    }
    pack_tool_group_items(&items, max, false).unwrap_or_else(|| {
        if "…".width() <= max {
            "…".to_string()
        } else {
            String::new()
        }
    })
}

/// Collapsed group row: gear, width-fitted counts, optional total elapsed.
///
/// Timing stays when it fits beside a real summary. If the duration suffix
/// would leave only an ellipsis, drop the clock so a verb can survive.
fn collapsed_tool_group_text(tools: &[(String, Option<u64>)], width: usize) -> String {
    let total_ms: u64 = tools.iter().filter_map(|(_, d)| *d).sum();
    let prefix = "⚙ ";
    let suffix = if total_ms > 0 {
        format!(" · {:.1}s", total_ms as f64 / 1000.0)
    } else {
        String::new()
    };
    let prefix_w = prefix.width();
    let budget = width.saturating_sub(prefix_w + suffix.width());
    let summary = tool_group_summary_for_width(tools, budget);
    if !summary.is_empty() && summary != "…" {
        return format!("{prefix}{summary}{suffix}");
    }
    let summary = tool_group_summary_for_width(tools, width.saturating_sub(prefix_w));
    if summary.is_empty() {
        format!("{prefix}…")
    } else {
        format!("{prefix}{summary}")
    }
}

/// Broad activity phase from Go `toolActivityState`, used on the live status bar.
fn tool_activity_state(name: &str) -> &'static str {
    let clean = sanitize_preview(name);
    let name = clean.trim().to_ascii_lowercase();
    if name.is_empty() {
        return "";
    }
    if is_delegate_name(&name) || name.contains(':') {
        return "delegating";
    }
    let mut verb = name.split_whitespace().next().unwrap_or(&name).to_string();
    if let Some(slash) = verb.rfind('/') {
        verb = verb[slash + 1..].to_string();
    }
    let verb = verb.trim_end_matches(':');
    match verb {
        "read" | "glob" | "grep" => "searching",
        "write" | "edit" => "shaping",
        "mcp" | "lsp" => "connecting",
        "generate_image" | "generate_speech" | "generate_video" => "creating",
        "understand_image" | "understand_voice" | "understand_video" => "inspecting",
        "bash" | "proc_start" | "proc_status" | "proc_stop" => "running",
        _ => "working",
    }
}

fn is_delegate_name(name: &str) -> bool {
    if name == "acp_delegate" {
        return true;
    }
    name.split_once(':')
        .is_some_and(|(_, rest)| rest.trim() == "acp_delegate")
}

/// Status-bar label: `searching · grep · file`, keeping the concrete tool_label.
fn activity_tool_label(name: &str) -> String {
    let (verb, rest) = tool_label(name);
    let phase = tool_activity_state(name);
    if phase.is_empty() {
        return if rest.is_empty() {
            verb
        } else {
            format!("{verb} · {rest}")
        };
    }
    if rest.is_empty() {
        format!("{phase} · {verb}")
    } else {
        format!("{phase} · {verb} · {rest}")
    }
}

fn is_tool_activity_status(status: &str) -> bool {
    status.starts_with("tool")
        || status.starts_with("searching")
        || status.starts_with("shaping")
        || status.starts_with("connecting")
        || status.starts_with("creating")
        || status.starts_with("inspecting")
        || status.starts_with("running")
        || status.starts_with("working")
        || status.starts_with("delegating")
}

fn lsp_diagnostic_text(path: &str, diagnostic: &LspDiagnostic) -> String {
    let mut text = format!("lsp · {path}:{} {}", diagnostic.line, diagnostic.message);
    if !diagnostic.source.is_empty() {
        text.push_str(" · ");
        text.push_str(&diagnostic.source);
    }
    text
}

fn entry_text(entry: &Entry) -> String {
    match entry {
        Entry::User { text, .. } | Entry::Assistant(text) | Entry::Note(text) => text.clone(),
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

/// Rows a block of text needs once hard-wrapped into `inner` columns.
fn estimated_wrapped_lines(text: &str, inner: usize) -> usize {
    let inner = inner.max(1);
    text.split('\n')
        .map(|line| {
            let cols = line.width();
            if cols == 0 { 1 } else { cols.div_ceil(inner) }
        })
        .sum::<usize>()
        .max(1)
}

fn entry_fingerprint(entry: &Entry) -> (usize, bool) {
    match entry {
        Entry::User { text, .. } | Entry::Assistant(text) | Entry::Note(text) => {
            (text.len(), false)
        }
        Entry::Tool { name, duration_ms } => (
            name.len().saturating_add(duration_ms.unwrap_or(0) as usize),
            false,
        ),
        Entry::Tools { tools, expanded } => (tools.len(), *expanded),
    }
}

/// Keep only the rows of `lines` that overlap `[target, window_end)` in
/// document coordinates starting at `cursor`.
///
/// If the estimate over-reported and the requested rows sit past what was
/// painted, keep the actual tail so follow does not land on a blank pane.
fn push_window_slice(
    out: &mut Vec<Line<'static>>,
    base: &mut usize,
    cursor: usize,
    target: usize,
    window_end: usize,
    lines: &[Line<'static>],
) {
    if lines.is_empty() {
        return;
    }
    let start = target.saturating_sub(cursor);
    let end = window_end.saturating_sub(cursor);
    let (start, end) = if start < lines.len() {
        (start, end.min(lines.len()).max(start))
    } else if cursor < window_end {
        let keep = end.saturating_sub(start).min(lines.len()).max(1);
        let start = lines.len().saturating_sub(keep);
        (start, lines.len())
    } else {
        return;
    };
    if start >= end {
        return;
    }
    if out.is_empty() {
        *base = cursor + start;
    }
    out.extend(lines[start..end].iter().cloned());
}

/// Like [`push_window_slice`], but the block is `painted` plus a trailing
/// blank separator — without cloning the whole painted entry first.
fn push_window_slice_with_sep(
    out: &mut Vec<Line<'static>>,
    base: &mut usize,
    cursor: usize,
    target: usize,
    window_end: usize,
    painted: &[Line<'static>],
) {
    let blank = Line::raw("");
    if painted.is_empty() {
        push_window_slice(out, base, cursor, target, window_end, &[blank]);
        return;
    }
    let virtual_len = painted.len() + 1;
    let start = target.saturating_sub(cursor);
    let end = window_end.saturating_sub(cursor);
    let (start, end) = if start < virtual_len {
        (start, end.min(virtual_len).max(start))
    } else if cursor < window_end {
        let keep = end.saturating_sub(start).min(virtual_len).max(1);
        let start = virtual_len.saturating_sub(keep);
        (start, virtual_len)
    } else {
        return;
    };
    if start >= end {
        return;
    }
    if out.is_empty() {
        *base = cursor + start;
    }
    let painted_end = end.min(painted.len());
    if start < painted.len() {
        out.extend(painted[start..painted_end].iter().cloned());
    }
    if end > painted.len() {
        out.push(blank);
    }
}

/// Source suffix whose wrap estimate covers at least `need_rows` rows.
///
/// Used so follow-mode live paint can markdown-parse only the visible tail.
/// An odd fence count means the cut landed inside a code block; the suffix
/// is extended back to the opening fence so highlighting stays intact.
fn live_tail_source(text: &str, need_rows: usize, width: usize) -> &str {
    if text.is_empty() || need_rows == 0 {
        return text;
    }
    let width = width.max(1);
    let slack = need_rows.saturating_add(TRANSCRIPT_OVERSCAN);
    let mut rows = 0usize;
    let mut start = 0usize;
    let mut line_end = text.len();
    while line_end > 0 && rows < slack {
        let line_start = text[..line_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &text[line_start..line_end];
        let cols = line.width();
        rows += if cols == 0 { 1 } else { cols.div_ceil(width) };
        start = line_start;
        if line_start == 0 {
            break;
        }
        line_end = line_start - 1;
    }
    if start == 0 {
        return text;
    }
    extend_to_open_fence(text, start)
}

fn extend_to_open_fence(text: &str, start: usize) -> &str {
    let suffix = &text[start..];
    let fences = suffix
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 0 {
        return suffix;
    }
    let Some(open) = text[..start].rfind("```") else {
        return text;
    };
    let line_start = text[..open].rfind('\n').map(|i| i + 1).unwrap_or(0);
    &text[line_start..]
}

fn leave_follow(app: &mut App, n: u16) {
    if app.follow {
        let max = max_scroll(app);
        // When the document still fits, there is nowhere to scroll away
        // from the tail. Keep following so incoming output remains visible.
        if max == 0 {
            return;
        }
        app.follow = false;
        app.scroll = max.saturating_sub(n);
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

/// Help chrome: title left, full session id right, keys on the bottom rail.
///
/// The id is identity, not a key row — putting it on the title keeps the
/// card from growing a header line. The title shortens to `help` when the
/// long name plus the id would collide.
fn overlay_block_help(app: &App, width: u16, hint: &str) -> Block<'static> {
    let id = app.session.session_id.as_str();
    let title = help_card_title(width, id);
    let mut block = overlay_block_hint(app, title, hint);
    if id.is_empty() {
        return block;
    }
    let title_w = title.width() + 2;
    let room = (width as usize).saturating_sub(2 + title_w + 1);
    let shown = clip_display(id, room.max(8));
    block = block.title(
        Line::from(Span::styled(
            format!(" {shown} "),
            app.theme.note().patch(app.theme.overlay()),
        ))
        .alignment(Alignment::Right),
    );
    block
}

fn help_card_title(width: u16, session_id: &str) -> &'static str {
    let id_w = if session_id.is_empty() {
        0
    } else {
        session_id.width() + 3
    };
    let long = "keyboard reference".width() + 2;
    if 2 + long + id_w + 1 <= width as usize {
        "keyboard reference"
    } else {
        "help"
    }
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
/// Help card chrome: two border columns, two padding columns, one gutter
/// between the key and action cells.
const HELP_CHROME_W: u16 = 5;
/// Top border, column header, bottom border.
const HELP_CHROME_H: u16 = 3;
/// Caps so a tall or wide terminal still sees a floating table, not a slab.
const HELP_MAX_WIDTH: u16 = 62;
const HELP_MAX_HEIGHT: u16 = 24;
/// Action column keeps at least this many columns when the pane is tight.
const HELP_ACTION_FLOOR: u16 = 18;

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    app.enforce_transcript_memory_ceiling();
    app.tick = frame.count() as u64;
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, app, area);
        return;
    }

    // header · hairline · transcript · input · footer
    let mut rows = vec![Constraint::Length(1), Constraint::Length(1)];
    rows.push(Constraint::Fill(1));
    // Composer sits on the document ground with no box: horizontal padding
    // is the only inset. Blank pad rows share that ground and read as a
    // tall empty well against the footer hairline, so they stay off.
    // The status bar owns the bottom hairline — and the live clock, when a
    // turn is running.
    let input_cols = area.width.saturating_sub(2);
    rows.push(Constraint::Length(input_height(app, input_cols)));
    // Footer hairline consumes a row; keep the status text on the row below.
    rows.push(Constraint::Length(2));
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(rows)
        .split(area);
    let transcript_area = areas[2];
    let input_area = areas[3];
    let footer_area = areas[4];

    // Document ground only. Header, hairline, and status sit on the
    // terminal default so a second fill cannot misalign with the pane.
    frame.render_widget(Block::new().style(app.theme.base()), transcript_area);

    paint_filled_header(frame, app, areas[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(areas[1].width as usize),
            app.theme.chrome().patch(app.theme.header_bg()),
        )))
        .style(app.theme.header_bg()),
        areas[1],
    );

    draw_transcript(frame, app, transcript_area);

    let input_block = Block::new()
        .style(app.theme.base())
        .padding(Padding::horizontal(1));
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);
    frame.render_widget(
        Paragraph::new(prompt_text(app, input_inner.width)).style(app.theme.base()),
        input_inner,
    );
    if app.pending_perm.is_none() && !app.welcome && !app.overlay.is_open() {
        frame.set_cursor_position(input_cursor_pos(app, input_inner));
    }

    // While a decision is pending the footer becomes the decision bar: the
    // keys stay visible even if the overlay is scrolled past or dimmed.
    // Its own top rule is a separate row so the status text is never eaten.
    let footer_block = Block::new()
        .borders(Borders::TOP)
        .border_style(app.theme.chrome())
        .style(app.theme.footer_bg())
        .padding(Padding::horizontal(1));
    let footer_inner = footer_block.inner(footer_area);
    frame.render_widget(footer_block, footer_area);
    frame.render_widget(
        Paragraph::new(app.footer_line(footer_inner.width)).style(app.theme.footer_bg()),
        footer_inner,
    );

    // The scrim covers the document only: header safety chips, the composer
    // and the footer decision bar all stay sharp, because those are what the
    // operator reads and types into while a modal is up. `doc` is the
    // transcript, ending at the composer.
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

/// Paint the header on the terminal default: clear the row, then overlay chips.
///
/// One column of inset on each side matches the composer and footer, so the
/// three chrome rows share a vertical rhythm instead of the header kissing
/// the frame edge while everything below is padded.
fn paint_filled_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        app.theme.header_bg(),
    ));
    frame.render_widget(Paragraph::new(fill).style(app.theme.header_bg()), area);
    let inner_w = area.width.saturating_sub(2);
    frame.render_widget(
        Paragraph::new(app.header_line(inner_w))
            .style(app.theme.header_bg())
            .block(Block::new().padding(Padding::horizontal(1))),
        area,
    );
}

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
                Span::styled(glyph, app.theme.accent()),
                Span::styled(text, app.theme.text()),
            ];
            let painted: usize = spans.iter().map(Span::width).sum();
            let pad = cols.saturating_sub(painted);
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
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
    // Paint the bar on `area` (not `inner`): VerticalRight uses the last
    // column, which the right padding reserved. Composer/footer are outside
    // this rect, so the end symbol sits on the last transcript row.
    let content = app.estimated_total_lines();
    app.scrollbar_state =
        transcript_scrollbar_state(content, height, app.scroll as usize, app.follow);
    if content > height {
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

/// Map a document scroll offset onto ratatui `ScrollbarState`.
///
/// `ScrollbarState::new(n)` is an item count; `position` is an index in
/// `[0, n)`, and the thumb reaches the end only at `n - 1`. Our offset
/// lives in `[0, max_scroll]`, so `n` is `max_scroll + 1`. Passing the raw
/// line count made the thumb stop a cell short of `↓` when following the
/// bottom (`position = content - viewport`, while ratatui's last slot is
/// `content - 1`).
fn transcript_scrollbar_state(
    content: usize,
    viewport: usize,
    scroll: usize,
    follow: bool,
) -> ScrollbarState {
    let viewport = viewport.max(1);
    let max_scroll = content.saturating_sub(viewport);
    let position = if follow {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };
    ScrollbarState::new(max_scroll.saturating_add(1).max(1))
        .position(position)
        .viewport_content_length(viewport)
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
    let width = area.width.saturating_sub(6).clamp(24, 60);
    let inner_w = width.saturating_sub(4).max(8) as usize;
    // Fit the card to a layout that actually fits the pane, so a 40×10
    // first-run still answers "what can it do" and "how do I start".
    let max_inner_h = area.height.saturating_sub(2).max(3) as usize;
    let body = welcome_lines(app, inner_w, max_inner_h);
    let height = (body.len() as u16 + 2).min(area.height);
    let spot = centered(area, Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, spot);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(overlay_block(app, "session")),
        spot,
    );
}

/// Welcome body, richest layout that still fits `inner_h` rows.
///
/// Priority: identity and the start hint outrank the tagline and effort.
/// Access is a safety fact and is kept as long as three rows exist.
fn welcome_lines(app: &App, inner_w: usize, inner_h: usize) -> Vec<Line<'static>> {
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

    let title = Line::from(Span::styled(
        "mowi",
        app.theme.accent().add_modifier(Modifier::BOLD),
    ));
    let tagline = Line::from(Span::styled(
        clip_display("ratatui client for the mow harness", inner_w),
        app.theme.note().patch(app.theme.overlay()),
    ));
    let mut fields = vec![
        field_row(app, "workspace", &workspace, inner_w),
        field_row(app, "model", &model, inner_w),
    ];
    if !app.effort.is_empty() {
        fields.push(field_row(app, "effort", &app.effort, inner_w));
    }
    let access = welcome_access_row(app, inner_w);
    let hint = welcome_hint_row(app, inner_w);

    let mut full = vec![title.clone(), tagline, Line::raw("")];
    full.extend(fields.clone());
    full.push(Line::raw(""));
    full.push(access.clone());
    full.push(Line::raw(""));
    full.push(hint.clone());

    let mut mid = vec![title.clone()];
    mid.extend(fields);
    mid.push(access.clone());
    mid.push(Line::raw(""));
    mid.push(hint.clone());

    let compact = vec![
        title.clone(),
        field_row(app, "workspace", &workspace, inner_w),
        field_row(app, "model", &model, inner_w),
        access.clone(),
        hint.clone(),
    ];
    let tight = vec![title, access.clone(), hint.clone()];
    let emergency = vec![access, hint];

    for candidate in [full, mid, compact, tight, emergency] {
        if candidate.len() <= inner_h {
            return candidate;
        }
    }
    welcome_hint_only(app, inner_w)
}

fn welcome_hint_only(app: &App, inner_w: usize) -> Vec<Line<'static>> {
    vec![welcome_hint_row(app, inner_w)]
}

fn welcome_access_row(app: &App, inner_w: usize) -> Line<'static> {
    let cap_tone = if app.allow_shell || app.allow_write {
        Tone::Warn
    } else {
        Tone::Ok
    };
    let mode = if inner_w >= 36 {
        format!("  {} mode", app.mode_chip())
    } else {
        format!("  {}", app.mode_chip())
    };
    Line::from(vec![
        Span::styled(
            format!("{:<10}", "access"),
            app.theme.note().patch(app.theme.overlay()),
        ),
        Span::styled(
            format!(" {} ", app.capability_chip()),
            app.theme.badge_solid(cap_tone),
        ),
        Span::styled(mode, app.theme.note().patch(app.theme.overlay())),
    ])
}

fn welcome_hint_row(app: &App, inner_w: usize) -> Line<'static> {
    let extra = if inner_w >= 46 {
        "  ·  ? for keys  ·  / for commands"
    } else if inner_w >= 26 {
        "  ·  ?  ·  /"
    } else {
        ""
    };
    Line::from(vec![
        Span::styled("type to begin", app.theme.text().patch(app.theme.overlay())),
        Span::styled(extra, app.theme.note().patch(app.theme.overlay())),
    ])
}

/// `label      value` with the label in a fixed gutter, so stacked fields read
/// as a table instead of drifting text. Values clip to the remaining columns.
fn field_row(app: &App, label: &str, value: &str, inner_w: usize) -> Line<'static> {
    let label = format!("{label:<10}");
    let room = inner_w.saturating_sub(label.width());
    Line::from(vec![
        Span::styled(label, app.theme.note().patch(app.theme.overlay())),
        Span::styled(
            clip_display(value, room),
            app.theme.text().patch(app.theme.overlay()),
        ),
    ])
}

/// Size the help card to its table, not the document pane.
///
/// Height hugs header + rows + borders, then caps so a long `slash.list`
/// scrolls instead of becoming a slab. Width hugs a readable action column
/// up to `HELP_MAX_WIDTH`; on a narrow pane the key column yields so
/// actions stay readable.
fn help_geometry(rows: &[(String, String)], area: Rect, session_id: &str) -> (Rect, u16) {
    let key_natural = rows
        .iter()
        .map(|(key, _)| key.width())
        .max()
        .unwrap_or(12)
        .saturating_add(2)
        .clamp(12, 18) as u16;
    let action_natural = rows
        .iter()
        .map(|(_, what)| what.width())
        .max()
        .unwrap_or(24) as u16;
    // Size to a readable action column; the longest outlier can clip.
    let action_want = action_natural.min(40).max(HELP_ACTION_FLOOR);
    let want_w = key_natural
        .saturating_add(action_want)
        .saturating_add(HELP_CHROME_W);
    let mut width = if area.width <= MIN_WIDTH + 8 {
        // Floor widths: every column matters, so use the pane.
        area.width.saturating_sub(2).max(24.min(area.width))
    } else {
        want_w
            .min(HELP_MAX_WIDTH)
            .min(area.width.saturating_sub(4))
            .max(32.min(area.width.saturating_sub(2)))
    };
    // Title rail must be able to hold `help` plus the full session id
    // when the pane allows — identity is not the first thing to clip.
    if !session_id.is_empty() {
        let rail = 11u16.saturating_add(session_id.width() as u16);
        width = width.max(rail.min(area.width.saturating_sub(2)));
    }

    let want_h = (rows.len() as u16).saturating_add(HELP_CHROME_H);
    let height = want_h
        .min(HELP_MAX_HEIGHT)
        .min(area.height)
        .max(5.min(area.height));

    let spot = centered(area, Constraint::Length(width), Constraint::Length(height));
    let inner_w = width.saturating_sub(4);
    let key_w = key_natural.min(inner_w.saturating_sub(1 + HELP_ACTION_FLOOR).max(8));
    (spot, key_w)
}

fn draw_help(frame: &mut Frame<'_>, app: &App, state: &mut TableState, area: Rect) {
    let rows_data = app.help_rows();
    let (spot, key_w) = help_geometry(&rows_data, area, &app.session.session_id);
    frame.render_widget(Clear, spot);
    let rows: Vec<Row<'static>> = rows_data
        .into_iter()
        .map(|(key, what)| {
            Row::new(vec![
                Cell::from(Span::styled(key, app.theme.chip())),
                Cell::from(Span::styled(what, app.theme.note())),
            ])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(key_w), Constraint::Fill(1)])
        .header(
            Row::new(vec!["key", "action"]).style(app.theme.accent().add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .row_highlight_style(app.theme.selected())
        .highlight_symbol("▸ ")
        .block(overlay_block_help(
            app,
            spot.width,
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
    let agent = app.peer_focus.clone().unwrap_or_else(|| "peer".to_string());
    let raw = app.peers.get(&agent).map(String::as_str).unwrap_or("");

    // Hug readable content instead of opening a document-height slab. Width is
    // bounded first; the same inner width then drives wrapping and height.
    let natural = raw
        .lines()
        .map(|line| sanitize_preview(line).width() as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(4);
    let max_w = area.width.saturating_sub(2).min(72);
    let width = natural.clamp(32.min(max_w), max_w.max(1));
    let inner_w = width.saturating_sub(4).max(1) as usize;
    let body: Vec<Line<'static>> = raw
        .lines()
        .flat_map(|line| wrap_cols(&sanitize_preview(line), inner_w))
        .map(|line| Line::styled(line, app.theme.context()))
        .collect();
    let wanted_h = (body.len() as u16).saturating_add(2).max(5);
    let height = wanted_h.min(18).min(area.height.max(1));
    let spot = centered(area, Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, spot);

    let viewport = spot.height.saturating_sub(2) as usize;
    let max_top = body.len().saturating_sub(viewport);
    let from_bottom = (app.peer_scroll as usize).min(max_top);
    let top = max_top.saturating_sub(from_bottom);
    let end = (top + viewport).min(body.len());
    let visible = if top < end {
        body[top..end].to_vec()
    } else {
        Vec::new()
    };
    let position = if body.len() > viewport {
        format!(" · {}/{}", top.saturating_add(1), max_top.saturating_add(1))
    } else {
        String::new()
    };
    frame.render_widget(
        Paragraph::new(visible)
            .style(app.theme.context())
            .block(overlay_block_hint(
                app,
                &format!("⇄ {agent}{position}"),
                "↑↓ scroll · esc close",
            )),
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
        .style(app.theme.overlay())
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            " approval required ",
            app.theme.warn().patch(app.theme.overlay()),
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

/// Terminal loop: drain RPC, paint when dirty or busy, then wait for keys.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &mut Client,
    app: &mut App,
) -> Result<(), Error> {
    let mut turn: Option<Receiver<Result<Value, Error>>> = None;
    let mut slash_rx: Option<Receiver<Result<Value, Error>>> = None;
    let mut context_rx: Option<Receiver<Result<Value, Error>>> = None;
    let mut dirty = true;

    while !app.quit {
        if drain_notifications(client, app) {
            dirty = true;
        }
        if poll_turn(client, app, &mut turn, &mut context_rx)? {
            dirty = true;
        }
        if try_start_pending_compact(client, app, &mut slash_rx, &turn) {
            dirty = true;
        }
        if poll_slash(client, app, &mut slash_rx, &mut turn) {
            dirty = true;
        }
        if poll_context(app, &mut context_rx) {
            dirty = true;
        }
        if needs_paint(app, dirty) {
            terminal.draw(|f| draw(f, app)).map_err(Error::Io)?;
            flush_clipboard(app);
            dirty = false;
        }
        if poll_input(client, app, &mut turn, &mut slash_rx)? {
            dirty = true;
        }
    }
    Ok(())
}

fn needs_paint(app: &App, dirty: bool) -> bool {
    dirty || app.busy
}

fn drain_notifications(client: &Client, app: &mut App) -> bool {
    let mut any = false;
    for _ in 0..NOTIFICATION_BATCH {
        match client.notifications().try_recv() {
            Ok(n) => {
                app.on_notification(&n);
                any = true;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                app.entries.push(Entry::Note("mow rpc exited".into()));
                app.quit = true;
                any = true;
                break;
            }
        }
    }
    any
}

fn poll_turn(
    client: &mut Client,
    app: &mut App,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    context_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<bool, Error> {
    let Some(rx) = turn.as_ref() else {
        return Ok(false);
    };
    match rx.try_recv() {
        Ok(res) => {
            let ok = res.is_ok();
            app.finish_turn(res);
            *turn = None;
            // Usage only moves at turn end; refresh the gauge without
            // blocking the 50ms loop. A failure is not fatal to the UI.
            if app.supports("context")
                && let Ok(rx) = client.request_context()
            {
                *context_rx = Some(rx);
            }
            if let Some(next) = app.take_queued_after_turn(ok) {
                *turn = Some(start_prompt(client, app, &next)?);
            }
            Ok(true)
        }
        Err(TryRecvError::Empty) => Ok(false),
        Err(TryRecvError::Disconnected) => {
            // The child died mid-turn. Its stderr is captured (never
            // inherited, or it would paint over this frame), so show
            // the tail instead of a bare "closed".
            for line in client.stderr_tail() {
                app.entries
                    .push(Entry::Note(format!("mow: {}", sanitize_preview(&line))));
            }
            app.finish_turn(Err(Error::Closed));
            *turn = None;
            let _ = app.take_queued_after_turn(false);
            Ok(true)
        }
    }
}

fn poll_slash(
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
) -> bool {
    let Some(rx) = slash_rx.as_ref() else {
        return false;
    };
    match rx.try_recv() {
        Ok(result) => {
            if let Some(kind) = app.pending_local.take() {
                match (kind.as_str(), result) {
                    ("compact", Ok(value)) => {
                        let rep = Client::decode_compact(&value);
                        let saved = if rep.over_budget {
                            " (still over budget)"
                        } else {
                            ""
                        };
                        app.entries.push(Entry::Note(format!(
                            "compacted [{}]: {} -> {} messages, {} chars saved{saved}",
                            rep.layer, rep.messages_before, rep.messages_after, rep.chars_saved
                        )));
                        app.status = format!("compacted · {} tokens · refreshing", rep.tokens);
                        match client.send("transcript", None) {
                            Ok(rx) => {
                                *slash_rx = Some(rx);
                                app.pending_local = Some("compact.transcript".into());
                            }
                            Err(error) => {
                                app.entries
                                    .push(Entry::Note(format!("transcript refresh: {error}")));
                                *slash_rx = None;
                            }
                        }
                    }
                    ("compact.transcript", Ok(value)) => {
                        match crate::rpc::decode_transcript(&value) {
                            Ok(messages) => app.load_transcript(messages),
                            Err(error) => app
                                .entries
                                .push(Entry::Note(format!("transcript refresh: {error}"))),
                        }
                        app.status = "compacted".into();
                        *slash_rx = None;
                    }
                    ("steer", Ok(_)) => {
                        app.status = "steered".into();
                        *slash_rx = None;
                    }
                    ("rewind.edit" | "rewind.retry" | "rewind.composer", Ok(value)) => {
                        let follow = kind.as_str();
                        match decode_rewind(&value) {
                            Some(last_user) => {
                                app.rewind_user = Some(last_user);
                                match client.request_transcript() {
                                    Ok(rx) => {
                                        *slash_rx = Some(rx);
                                        app.pending_local = Some(format!("{follow}.transcript"));
                                    }
                                    Err(error) => {
                                        app.entries.push(Entry::Note(format!(
                                            "transcript refresh: {error}"
                                        )));
                                        app.drop_last_turn_entries();
                                        apply_rewind_followup(app, turn, client, follow);
                                        *slash_rx = None;
                                    }
                                }
                            }
                            None => {
                                app.status = rewind_empty_status(follow).into();
                                *slash_rx = None;
                            }
                        }
                    }
                    (
                        "rewind.edit.transcript"
                        | "rewind.retry.transcript"
                        | "rewind.composer.transcript",
                        Ok(value),
                    ) => {
                        let follow = kind.strip_suffix(".transcript").unwrap_or(kind.as_str());
                        match crate::rpc::decode_transcript(&value) {
                            Ok(messages) => app.load_transcript(messages),
                            Err(error) => {
                                app.drop_last_turn_entries();
                                app.entries
                                    .push(Entry::Note(format!("transcript refresh: {error}")));
                            }
                        }
                        apply_rewind_followup(app, turn, client, follow);
                        *slash_rx = None;
                    }
                    ("model.list", Ok(value)) => {
                        match crate::rpc::decode_model_list(&value) {
                            Ok(list) => app.apply_model_list(list),
                            Err(error) => {
                                app.entries.push(Entry::Note(format!("model: {error}")));
                            }
                        }
                        *slash_rx = None;
                    }
                    ("model.set", Ok(value)) => {
                        let model = value
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        app.apply_model_set(model);
                        *slash_rx = None;
                    }
                    ("effort.list", Ok(value)) => {
                        match crate::rpc::decode_effort_list(&value) {
                            Ok(list) => app.apply_effort_list(list),
                            Err(error) => {
                                app.entries.push(Entry::Note(format!("effort: {error}")));
                            }
                        }
                        *slash_rx = None;
                    }
                    ("effort.set", Ok(value)) => {
                        let effort = value
                            .get("effort")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        app.apply_effort_set(effort);
                        *slash_rx = None;
                    }
                    ("perm.set" | "perm.decide", Ok(_)) => {
                        *slash_rx = None;
                    }
                    (_, Err(error)) => {
                        if kind.ends_with(".transcript") && kind.starts_with("rewind.") {
                            app.drop_last_turn_entries();
                            apply_rewind_followup(
                                app,
                                turn,
                                client,
                                kind.strip_suffix(".transcript").unwrap_or(&kind),
                            );
                        } else if kind.starts_with("rewind.") {
                            app.status = "rewind failed".into();
                            app.rewind_user = None;
                        } else {
                            app.status = "operation failed".into();
                        }
                        app.entries
                            .push(Entry::Note(format!("operation failed: {error}")));
                        *slash_rx = None;
                    }
                    _ => {
                        *slash_rx = None;
                    }
                }
                return true;
            }
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
            *slash_rx = None;
            true
        }
        Err(TryRecvError::Disconnected) => {
            app.entries
                .push(Entry::Note("slash connection closed".into()));
            *slash_rx = None;
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

fn poll_context(app: &mut App, context_rx: &mut Option<Receiver<Result<Value, Error>>>) -> bool {
    let Some(rx) = context_rx.as_ref() else {
        return false;
    };
    match rx.try_recv() {
        Ok(Ok(value)) => {
            app.apply_context(&ContextUsage::from_value(&value));
            *context_rx = None;
            true
        }
        Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
            *context_rx = None;
            false
        }
        Err(TryRecvError::Empty) => false,
    }
}

fn poll_input(
    client: &mut Client,
    app: &mut App,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<bool, Error> {
    if !event::poll(Duration::from_millis(50)).map_err(Error::Io)? {
        return Ok(false);
    }
    let mut dirty = false;
    let mut scroll_rows = 0i32;
    // Drain a whole input burst before repainting. Held arrows can enqueue
    // hundreds of repeats ahead of the next typed character; process those
    // repeats as one scroll operation so the composer becomes responsive as
    // soon as the key is released.
    for _ in 0..INPUT_BATCH {
        match event::read().map_err(Error::Io)? {
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && app.pending_perm.is_none()
                    && !app.overlay.is_open()
                    && matches!(
                        key.code,
                        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
                    ) =>
            {
                scroll_rows += match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -5,
                    KeyCode::PageDown => 5,
                    _ => 0,
                };
                dirty = true;
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                handle_key(key, client, app, turn, slash_rx)?;
                dirty = true;
            }
            // Bracketed paste lands at the input cursor, multi-line safe.
            Event::Paste(text) => {
                // A stray paste must never answer a permission prompt.
                if app.pending_perm.is_none() {
                    if app.welcome {
                        app.welcome = false;
                    }
                    app.insert_text(&text);
                    dirty = true;
                }
            }
            Event::Resize(..) => {
                // Layout recomputes on the next draw; clamp scroll so a
                // shrink cannot strand the viewport past the end.
                if !app.follow {
                    app.scroll = app.scroll.min(max_scroll(app));
                }
                dirty = true;
            }
            // Wheel is transcript-only. Mouse capture is off (native
            // select/copy), so this only fires if a host still delivers it.
            Event::Mouse(mouse) => {
                handle_mouse(app, mouse.kind);
                dirty = true;
            }
            _ => {}
        }
        if !event::poll(Duration::ZERO).map_err(Error::Io)? {
            break;
        }
    }
    if scroll_rows < 0 {
        leave_follow(app, scroll_rows.unsigned_abs().min(u16::MAX as u32) as u16);
    } else if scroll_rows > 0 {
        scroll_down(app, (scroll_rows as u32).min(u16::MAX as u32) as u16);
    }
    Ok(dirty)
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
            KeyCode::Char('y') => return app.permission_decision("allow", client, slash_rx),
            KeyCode::Char('n') | KeyCode::Esc => {
                return app.permission_decision("deny", client, slash_rx);
            }
            KeyCode::Char('a') => return app.permission_decision("always", client, slash_rx),
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
            KeyCode::Enter => overlay_activate(client, app, slash_rx)?,
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if ctrl => {
            if app.busy || turn.is_some() {
                app.request_cancel();
                let _ = client.cancel();
            }
            app.quit = true;
        }
        KeyCode::BackTab => {
            let mode = app.toggle_ask_mode();
            *slash_rx = Some(client.request_perm_set(mode)?);
            app.pending_local = Some("perm.set".into());
            app.status = format!("mode: {mode}");
        }
        KeyCode::Esc => {
            if app.dismiss_overlay() {
            } else if app.collapse_tool_group() {
                // Esc is a view op first: collapse an expanded tool group
                // before it ever reaches the destructive cancel.
            } else if app.busy || turn.is_some() {
                app.request_cancel();
                client.cancel()?;
                if !app.status.starts_with("cancelled ·") {
                    app.status = "cancelling".into();
                }
            }
        }
        KeyCode::Enter => {
            let text = app.input.trim().to_string();
            if text.starts_with('/') {
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
        _ => handle_view_key(app, key),
    }
    Ok(())
}

/// Composer, transcript scroll, and other local keys. Never talks to the Engine.
///
/// ↑/↓, PgUp/PgDn, and the mouse wheel scroll the transcript and never rewrite
/// the composer. Last-prompt recall is `/edit` only. Plain letters, including
/// `t`, always type.
fn handle_view_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
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
        // ctrl+/ arrives as Char('/') with CONTROL on most terminals.
        KeyCode::Char('/') if ctrl => app.overlay = Overlay::help(),
        KeyCode::Char('?') if app.input.is_empty() => app.overlay = Overlay::help(),
        KeyCode::Backspace => app.backspace_char(),
        KeyCode::Delete => app.delete_char(),
        KeyCode::Home => app.cursor_home(),
        KeyCode::End => app.cursor_end(),
        KeyCode::PageUp => leave_follow(app, 5),
        KeyCode::PageDown => scroll_down(app, 5),
        KeyCode::Tab => {
            if app.input.starts_with('/') {
                app.complete_slash();
            }
        }
        KeyCode::Left => app.move_cursor(-1),
        KeyCode::Right => app.move_cursor(1),
        KeyCode::Up => leave_follow(app, 1),
        KeyCode::Down => scroll_down(app, 1),
        // Unknown chords must not leak a letter into the composer.
        KeyCode::Char(_) if ctrl => {}
        KeyCode::Char(c) => app.insert_char(c),
        _ => {}
    }
}

fn handle_mouse(app: &mut App, kind: MouseEventKind) {
    if app.pending_perm.is_some() || app.overlay.is_open() {
        return;
    }
    if app.welcome {
        app.welcome = false;
    }
    match kind {
        MouseEventKind::ScrollUp => leave_follow(app, 3),
        MouseEventKind::ScrollDown => scroll_down(app, 3),
        _ => {}
    }
}

fn overlay_activate(
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<(), Error> {
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
            app.overlay = Overlay::None;
            if let Some(id) = selection {
                *slash_rx = Some(client.request_model_set(&id)?);
                app.pending_local = Some("model.set".into());
                app.status = "switching model…".into();
            }
        }
        "effort" => {
            app.overlay = Overlay::None;
            if let Some(id) = selection {
                *slash_rx = Some(client.request_effort_set(&id)?);
                app.pending_local = Some("effort.set".into());
                app.status = "switching effort…".into();
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

const STEER_USAGE: &str = "steer · usage: /steer <guidance>  (while a turn runs)";
const STEER_IDLE: &str = "steer · no turn running — just send your message";
const STEER_UNSUPPORTED: &str = "steer is not supported by this host";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactPlan {
    Send,
    Defer,
}

/// Host `compact` is a worker method unless advertised in `control_methods`.
fn plan_compact(busy: bool, compact_is_control: bool) -> CompactPlan {
    if busy && !compact_is_control {
        CompactPlan::Defer
    } else {
        CompactPlan::Send
    }
}

fn start_compact(
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
    max_chars: i64,
) -> Result<(), Error> {
    let params = (max_chars > 0).then(|| serde_json::json!({ "max_chars": max_chars }));
    *slash_rx = Some(client.send("compact", params)?);
    app.pending_local = Some("compact".into());
    app.status = "compacting…".into();
    Ok(())
}

fn try_start_pending_compact(
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
    turn: &Option<Receiver<Result<Value, Error>>>,
) -> bool {
    if app.busy || turn.is_some() || slash_rx.is_some() {
        return false;
    }
    let Some(max_chars) = app.pending_compact.take() else {
        return false;
    };
    match start_compact(client, app, slash_rx, max_chars) {
        Ok(()) => true,
        Err(error) => {
            app.pending_compact = Some(max_chars);
            app.status = format!("compact: {error}");
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SteerPlan {
    Usage,
    Idle,
    Unsupported,
    Send(String),
}

/// Decide what `/steer` should do. Local so a missing host method never
/// blocks the composer, and so idle/empty stay UI errors instead of RPC.
fn plan_steer(args: &[String], busy: bool, supported: bool) -> SteerPlan {
    if !supported {
        return SteerPlan::Unsupported;
    }
    let text = args.join(" ");
    if text.is_empty() {
        return SteerPlan::Usage;
    }
    if !busy {
        return SteerPlan::Idle;
    }
    SteerPlan::Send(text)
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
    if name == "perm" {
        return handle_perm_slash(&args, client, app, slash_rx);
    }
    match slash_route(name, &app.slash_commands) {
        SlashRoute::Quit => {
            if app.busy || turn.is_some() {
                app.request_cancel();
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
            start_rewind(client, app, slash_rx, turn, "rewind.edit")?;
        }
        "btw" => {
            let question = args.join(" ");
            if question.trim().is_empty() {
                app.status = "usage: /btw <question>".into();
            } else if app.busy || turn.is_some() {
                app.status = "/btw is unavailable while busy".into();
            } else {
                app.entries
                    .push(Entry::Note("btw · aside — not added to context".into()));
                app.entries.push(Entry::user(&question));
                app.live.clear();
                app.reset_think_state();
                app.cancelled = false;
                app.busy = true;
                app.follow = true;
                app.scroll = u16::MAX;
                app.status = "running aside".into();
                app.activity_started = Some(Instant::now());
                *turn = Some(client.prompt_with(&question, true)?);
            }
        }
        "retry" => {
            start_rewind(client, app, slash_rx, turn, "rewind.retry")?;
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
                *slash_rx = Some(client.send("model.list", None)?);
                app.pending_local = Some("model.list".into());
                app.status = "listing models…".into();
            } else {
                *slash_rx = Some(client.request_model_set(&args.join(" "))?);
                app.pending_local = Some("model.set".into());
                app.status = "switching model…".into();
            }
        }
        "effort" => {
            if args.is_empty() {
                *slash_rx = Some(client.send("effort.list", None)?);
                app.pending_local = Some("effort.list".into());
                app.status = "listing efforts…".into();
            } else {
                *slash_rx = Some(client.request_effort_set(&args.join(" "))?);
                app.pending_local = Some("effort.set".into());
                app.status = "switching effort…".into();
            }
        }
        "status" => {
            let status = client.status(Duration::from_secs(20))?;
            app.apply_status(&status);
            app.entries.push(Entry::Note(app.status_summary()));
        }
        "lsp" => {
            app.show_lsp_problems();
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
            let busy = app.busy || turn.is_some();
            match plan_compact(busy, app.compact_is_control()) {
                CompactPlan::Defer => {
                    app.pending_compact = Some(max_chars);
                    let msg = "compact · applies when the turn finishes";
                    app.status = msg.into();
                    app.entries.push(Entry::Note(msg.into()));
                }
                CompactPlan::Send => {
                    start_compact(client, app, slash_rx, max_chars)?;
                }
            }
        }
        "rewind" | "undo" => {
            start_rewind(client, app, slash_rx, turn, "rewind.composer")?;
        }
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
            let busy = app.busy || turn.is_some();
            match plan_steer(&args, busy, app.supports("steer")) {
                SteerPlan::Unsupported => {
                    app.status = STEER_UNSUPPORTED.into();
                }
                SteerPlan::Usage => {
                    app.status = STEER_USAGE.into();
                }
                SteerPlan::Idle => {
                    app.status = STEER_IDLE.into();
                }
                SteerPlan::Send(text) => {
                    *slash_rx = Some(client.request_steer(&text)?);
                    app.pending_local = Some("steer".into());
                    app.entries
                        .push(Entry::Note(format!("steer · {}", clip_display(&text, 80))));
                    app.status = "steering…".into();
                }
            }
        }
        _ => {
            app.status = format!("/{name} is handled locally");
        }
    }
    Ok(())
}

fn start_rewind(
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    follow: &str,
) -> Result<(), Error> {
    if app.busy || turn.is_some() {
        app.status = match follow {
            "rewind.retry" => "retry · wait for the current turn to finish",
            "rewind.edit" => "edit · wait for the current turn to finish",
            _ => "rewind · wait for the current turn to finish",
        }
        .into();
        return Ok(());
    }
    if !app.supports("rewind") {
        app.status = "rewind is not supported by this host".into();
        return Ok(());
    }
    *slash_rx = Some(client.request_rewind()?);
    app.pending_local = Some(follow.into());
    app.status = "rewinding…".into();
    Ok(())
}

fn rewind_empty_status(follow: &str) -> &'static str {
    match follow {
        "rewind.retry" => "retry · nothing to retry",
        "rewind.edit" => "edit · nothing to edit",
        _ => "nothing to rewind",
    }
}

fn apply_rewind_followup(
    app: &mut App,
    turn: &mut Option<Receiver<Result<Value, Error>>>,
    client: &mut Client,
    follow: &str,
) {
    let last = app.rewind_user.take().unwrap_or_default();
    match follow {
        "rewind.edit" => {
            app.set_input(last);
            app.status = "editing last message — change it and press enter".into();
        }
        "rewind.retry" => {
            if last.trim().is_empty() {
                app.status = "retry · nothing to retry".into();
            } else if turn.is_some() {
                app.enqueue_prompt(last);
            } else {
                match start_prompt(client, app, &last) {
                    Ok(rx) => *turn = Some(rx),
                    Err(error) => {
                        app.entries
                            .push(Entry::Note(format!("retry failed: {error}")));
                        app.status = "retry failed".into();
                    }
                }
            }
        }
        "rewind.composer" => {
            app.set_input(last);
            app.status = "rewound — edit and send again".into();
        }
        _ => {}
    }
}

fn handle_perm_slash(
    args: &[String],
    client: &mut Client,
    app: &mut App,
    slash_rx: &mut Option<Receiver<Result<Value, Error>>>,
) -> Result<(), Error> {
    let mode = match args.first().map(String::as_str) {
        None => app.toggle_ask_mode(),
        Some("ask") => {
            app.ask_mode = true;
            "ask"
        }
        Some("auto") => {
            app.ask_mode = false;
            "auto"
        }
        Some(_) => {
            app.status = "usage: /perm [ask|auto]".into();
            return Ok(());
        }
    };
    *slash_rx = Some(client.request_perm_set(mode)?);
    app.pending_local = Some("perm.set".into());
    app.status = format!("mode: {mode}");
    Ok(())
}

fn start_prompt(
    client: &mut Client,
    app: &mut App,
    text: &str,
) -> Result<Receiver<Result<Value, Error>>, Error> {
    app.entries.push(Entry::user(text));
    app.live.clear();
    app.reset_think_state();
    app.cancelled = false;
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
    use crate::theme::ThemeName;

    fn usage(tokens: u64, window: Option<u64>, pct: Option<f64>) -> ContextUsage {
        ContextUsage {
            tokens,
            context_window: window,
            remaining: window.map(|w| w.saturating_sub(tokens)),
            percent: pct,
        }
    }

    #[test]
    fn steer_plans_idle_usage_and_unsupported() {
        assert_eq!(plan_steer(&[], true, true), SteerPlan::Usage);
        assert_eq!(
            plan_steer(&["focus".into(), "on".into(), "tests".into()], false, true),
            SteerPlan::Idle
        );
        assert_eq!(
            plan_steer(&["focus".into()], true, false),
            SteerPlan::Unsupported
        );
        assert_eq!(
            plan_steer(&["focus".into(), "on".into(), "tests".into()], true, true),
            SteerPlan::Send("focus on tests".into())
        );
        let mut app = App::new(SessionInfo::default());
        app.set_capabilities(&["prompt".into(), "cancel".into()]);
        assert!(!app.supports("steer"));
        assert_eq!(
            plan_steer(&["go".into()], true, app.supports("steer")),
            SteerPlan::Unsupported
        );
        assert_eq!(
            STEER_IDLE,
            "steer · no turn running — just send your message"
        );
        assert!(STEER_USAGE.contains("/steer <guidance>"));
        assert!(STEER_UNSUPPORTED.contains("not supported"));
    }

    #[test]
    fn steer_is_not_a_blocking_rpc_from_the_ui() {
        // A 20s call() here froze the 50ms input loop while a turn ran.
        // Build the needle at runtime so this assertion does not match itself.
        let src = include_str!("app.rs");
        let blocking = format!("client.{}(", "steer");
        assert!(
            !src.contains(&blocking),
            "steer must go through request_steer so the UI stays responsive"
        );
        assert!(src.contains("request_steer"));
        for method in [
            "rewind",
            "model_set",
            "effort_set",
            "perm_set",
            "perm_decide",
        ] {
            let blocking = format!("client.{method}(");
            assert!(
                !src.contains(&blocking),
                "{method} must use the non-blocking request_* path"
            );
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

    fn summary_tools(names: &[&str]) -> Vec<(String, Option<u64>)> {
        names
            .iter()
            .map(|name| ((*name).to_string(), None))
            .collect()
    }

    fn assert_summary_tokens(summary: &str, verbs: &[&str]) {
        for part in summary.split(" · ") {
            if part == "…" {
                continue;
            }
            if let Some((verb, rest)) = part.split_once(" ×") {
                assert!(
                    verbs.iter().any(|v| *v == verb),
                    "unknown verb {verb:?} in {summary:?}"
                );
                assert!(
                    rest.chars().all(|c| c.is_ascii_digit()) && rest != "1" && !rest.is_empty(),
                    "count must be a whole integer > 1, got {rest:?} in {summary:?}"
                );
            } else {
                assert!(
                    verbs.iter().any(|v| *v == part),
                    "mid-token or unknown part {part:?} in {summary:?}"
                );
            }
        }
    }

    #[test]
    fn tool_group_summary_counts_by_tool_in_first_seen_order() {
        let tools = summary_tools(&[
            "bash cargo test",
            "read src/app.rs",
            "grep estimated_entry_lines",
            "bash cargo clippy",
            "read src/render.rs",
        ]);
        assert_eq!(tool_group_summary(&tools), "bash ×2 · read ×2 · grep");
        assert!(tool_group_summary(&tools).contains('×'));
    }

    #[test]
    fn tool_group_summary_single_tool_has_no_count() {
        let tools = summary_tools(&["bash cargo test"]);
        assert_eq!(tool_group_summary(&tools), "bash");
        assert!(
            !tool_group_summary(&tools).contains('×'),
            "a lone verb must not grow a ×1"
        );
    }

    #[test]
    fn tool_group_summary_truncates_gracefully() {
        let tools = summary_tools(&[
            "bash cargo test",
            "bash cargo clippy",
            "grep flaky",
            "read src/app.rs",
            "read src/render.rs",
        ]);
        let full = tool_group_summary(&tools);
        assert_eq!(full, "bash ×2 · grep · read ×2");

        let verbs = ["bash", "grep", "read"];
        for max in 0..=full.width() + 4 {
            let fitted = tool_group_summary_for_width(&tools, max);
            assert!(
                fitted.width() <= max || fitted.is_empty(),
                "overran {max}: {fitted:?} width {}",
                fitted.width()
            );
            if !fitted.is_empty() {
                assert_summary_tokens(&fitted, &verbs);
            }
            assert!(
                !fitted.chars().any(|c| c == '×')
                    || fitted.split(" · ").any(|p| p.contains(" ×")
                        && p.split_once(" ×").is_some_and(|(v, n)| {
                            verbs.contains(&v) && n.chars().all(|c| c.is_ascii_digit())
                        })),
                "× must stay attached to a count: {fitted:?}"
            );
            assert!(
                !fitted.contains("bas ") && !fitted.ends_with("bas") && !fitted.contains("gre "),
                "mid-token cut: {fitted:?}"
            );
        }

        let hybrid = tool_group_summary_for_width(&tools, "bash ×2 · grep · …".width());
        assert_eq!(hybrid, "bash ×2 · grep · …");
        let verbs_only = tool_group_summary_for_width(&tools, "bash · …".width());
        assert_eq!(verbs_only, "bash · …");
    }

    #[test]
    fn activity_tool_labels_use_phase_verbs() {
        assert_eq!(activity_tool_label("grep"), "searching · grep");
        assert_eq!(
            activity_tool_label("read src/app.rs"),
            "searching · read · src/app.rs"
        );
        assert_eq!(
            activity_tool_label("write dest.rs"),
            "shaping · write · dest.rs"
        );
        assert_eq!(
            activity_tool_label("bash echo hi"),
            "running · bash · echo hi"
        );
        assert_eq!(
            activity_tool_label("acp_delegate"),
            "delegating · acp_delegate"
        );
        assert_eq!(tool_activity_state("mcp"), "connecting");
        assert_eq!(tool_activity_state("unknown_tool"), "working");
    }

    #[test]
    fn long_tool_names_do_not_eat_the_user_prompt() {
        // A tool row whose name is a whole shell blob wraps to several lines.
        // The transcript window math must agree with what is painted, or the
        // overflow scrolls the user's own prompt off the top of the pane.
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.entries.push(Entry::user("summarise the repo"));
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.last_view_w = 40;
        let entries = vec![
            Entry::user("a short prompt"),
            Entry::user("a prompt that is quite a lot longer than forty columns"),
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
        app.theme = Theme::plain(ThemeName::CatppuccinMocha);
        app.entries.push(Entry::user("hello there"));
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
            let line = decision_line(
                Theme::colored(ThemeName::CatppuccinMocha),
                width,
                Some("bash"),
            );
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.effort = "medium".into();
        app.entries.push(Entry::user("hi"));
        app.live.push_str("hello");

        let out = render(&mut app, 80, 14);
        assert!(out.contains("mowi"), "{out}");
        assert!(out.contains("gpt-5-mini (medium)"), "{out}");
        let header = out.lines().next().expect("header");
        assert!(
            !header.contains("abcdef0123456789"),
            "session id is not a header chip: {header}"
        );
        assert!(
            !out.contains("abcdef0123456789"),
            "session id is not status-bar chrome: {out}"
        );
        assert!(out.contains("hi"), "{out}");
        assert!(!out.contains("❯ hi"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("enter send"), "{out}");
        // Safety chips are always painted.
        assert!(out.contains("read-only"), "{out}");
        assert!(out.contains("ask"), "{out}");
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn find_row(buf: &ratatui::buffer::Buffer, width: u16, height: u16, needle: &str) -> u16 {
        for y in 0..height {
            let row = row_text(buf, y, width);
            if row.contains(needle) {
                return y;
            }
        }
        panic!(
            "row containing {needle:?} not painted:\n{}",
            (0..height)
                .map(|y| row_text(buf, y, width))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn composer_sits_on_base_without_a_top_rule() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.input.push_str("draft");
        app.cursor = app.input.chars().count();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let composer_y = find_row(buf, 60, 16, "draft");
        let above = row_text(buf, composer_y - 1, 60);
        assert!(
            above.chars().filter(|c| *c == '─').count() < 50,
            "composer must not have a top rule: {above}"
        );
        assert!(
            !above.contains('╭')
                && !above.contains('╮')
                && !above.contains('╰')
                && !above.contains('╯'),
            "composer must not be a rounded box: {above}"
        );

        let draft_x = row_text(buf, composer_y, 60).find("draft").expect("draft") as u16;
        assert!(
            draft_x >= 2,
            "composer keeps a horizontal inset: {}",
            row_text(buf, composer_y, 60)
        );
        assert_eq!(
            buf[(draft_x, composer_y)].bg,
            crate::theme::mocha::BASE,
            "composer text sits on the document ground"
        );
        // Side edges of the composer row are also base, not a raised wash.
        assert_eq!(buf[(0, composer_y)].bg, crate::theme::mocha::BASE);
        assert_eq!(buf[(59, composer_y)].bg, crate::theme::mocha::BASE);
        assert_eq!(buf[(draft_x, composer_y + 1)].symbol(), "─");
    }

    #[test]
    fn footer_has_its_own_top_rule_and_keeps_status_text() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.input.push_str("draft");
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let footer_y = find_row(buf, 60, 16, "idle");
        assert!(
            row_text(buf, footer_y, 60).contains("enter send"),
            "status text must remain visible under its own rule"
        );
        let rule = row_text(buf, footer_y - 1, 60);
        assert!(
            rule.chars().filter(|c| *c == '─').count() >= 50,
            "footer top rule: {rule}"
        );
        let composer_y = find_row(buf, 60, 16, "draft");
        assert_eq!(
            footer_y - 1,
            composer_y + 1,
            "footer rule sits on the row below the composer text"
        );
        assert!(
            row_text(buf, composer_y - 1, 60)
                .chars()
                .filter(|c| *c == '─')
                .count()
                < 50,
            "only the status bar keeps a top rule"
        );
    }

    #[test]
    fn busy_clock_lives_on_the_status_bar() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.busy = true;
        app.animate = false;
        app.activity_started = Some(Instant::now());
        app.status = "calling model".into();
        app.live.push_str("hello");
        app.input.push_str("queued draft");
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();

        let composer_y = find_row(buf, 80, 18, "queued draft");
        let footer_y = find_row(buf, 80, 18, "enter queue");
        let footer = row_text(buf, footer_y, 80);
        assert!(
            footer.contains("calling model"),
            "status bar owns the busy verb: {footer}"
        );
        assert!(
            footer.contains("0.0s"),
            "elapsed lives on the status bar: {footer}"
        );
        assert!(
            footer.contains("···"),
            "typing pulse lives on the status bar: {footer}"
        );
        assert!(
            !footer.contains("idle"),
            "idle must not sit beside the clock: {footer}"
        );
        assert!(
            !footer.contains("busy"),
            "coarse busy is replaced by the verb: {footer}"
        );
        // No second clock above the composer — the band is gone.
        let above = row_text(buf, composer_y.saturating_sub(1), 80);
        assert!(
            !above.contains("calling model") && !above.contains("0.0s"),
            "activity band must not sit above the composer: {above}"
        );
        assert_eq!(
            footer_y - 1,
            composer_y + 1,
            "footer rule sits on the row below the composer text"
        );
    }

    #[test]
    fn header_has_horizontal_inset_matching_composer() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), " ", "header left inset");
        assert_eq!(buf[(79, 0)].symbol(), " ", "header right inset");
        let header = row_text(buf, 0, 80);
        assert!(header.trim_start().starts_with("mowi"), "{header}");
    }

    #[test]
    fn activity_line_styles_the_live_clock() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.busy = true;
        app.animate = false;
        app.activity_started = Some(Instant::now());
        app.status = "calling model".into();
        app.live.push_str("hello");

        let line = app.activity_line();
        let spinner = line
            .spans
            .iter()
            .find(|span| span.content.contains('●'))
            .expect("spinner");
        assert_eq!(spinner.style.fg, app.theme.spinner().fg);
        let elapsed = line
            .spans
            .iter()
            .find(|span| {
                span.content.ends_with('s') && span.content.chars().any(|c| c.is_ascii_digit())
            })
            .expect("elapsed");
        assert_eq!(elapsed.style.fg, app.theme.timing().fg);
        let status = line
            .spans
            .iter()
            .find(|span| span.content.contains("calling"))
            .expect("status");
        assert_eq!(status.style.fg, app.theme.text().fg);
        let pulse = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "···")
            .expect("typing pulse");
        assert_eq!(pulse.style.fg, app.theme.typing().fg);
    }

    #[test]
    fn footer_says_queue_while_busy() {
        let mut app = App::new(SessionInfo::default());
        app.busy = true;
        let wide = footer_text(&app, 80);
        assert!(wide.contains("enter queue"), "{wide}");
        assert!(!wide.contains("enter send"), "{wide}");
        app.busy = false;
        let idle = footer_text(&app, 80);
        assert!(idle.contains("enter send"), "{idle}");
    }

    #[test]
    fn header_and_footer_use_terminal_background() {
        let mut app = App::new(SessionInfo {
            session_id: "abcdef0123456789".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.input.push_str("draft");
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let footer_y = find_row(buf, 100, 14, "idle");
        for x in 0..100 {
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Reset,
                "header cell ({x},0) bg={:?} symbol={:?}",
                buf[(x, 0)].bg,
                buf[(x, 0)].symbol()
            );
            assert_eq!(
                buf[(x, footer_y)].bg,
                Color::Reset,
                "footer cell ({x},{footer_y}) bg={:?}",
                buf[(x, footer_y)].bg
            );
        }
        let composer_y = find_row(buf, 100, 14, "draft");
        assert_eq!(
            buf[(2, composer_y)].bg,
            crate::theme::mocha::BASE,
            "composer stays on the document ground"
        );
    }

    #[test]
    fn user_entry_is_a_filled_band() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.entries.push(Entry::user("hi"));
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
        assert_eq!(
            buf[(x, y)].fg,
            crate::theme::mocha::PEACH,
            "user text is peach, not blue"
        );
        assert_ne!(buf[(x, y)].fg, crate::theme::mocha::BLUE);
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
    fn user_prompt_stamp_is_inline_when_recorded() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        let at = UNIX_EPOCH + Duration::from_secs(4 * 3600 + 2 * 60);
        app.entries.push(Entry::User {
            text: "hello there".into(),
            at: Some(at),
        });
        let out = render(&mut app, 80, 16);
        let row = out
            .lines()
            .find(|line| line.contains("hello there"))
            .expect("prompt row");
        assert!(row.contains("04:02"), "inline stamp missing: {row}");
        assert_eq!(format_user_stamp(at), "04:02");
        let painted = app.entry_lines(&app.entries[0]).len();
        let estimated = app.estimated_entry_lines(&app.entries[0]) - 1;
        assert!(
            estimated >= painted,
            "stamp must be in the wrap estimate: estimated {estimated}, painted {painted}"
        );
    }

    #[test]
    fn resumed_user_prompt_has_no_invented_stamp() {
        let mut app = App::from_transcript(
            SessionInfo::default(),
            vec![TranscriptMessage {
                timestamp: None,
                role: "user".into(),
                content: "old prompt".into(),
            }],
        );
        match &app.entries[0] {
            Entry::User { text, at } => {
                assert_eq!(text, "old prompt");
                assert_eq!(*at, None, "engine transcript has no per-message time");
            }
            other => panic!("{other:?}"),
        }
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        let out = render(&mut app, 80, 16);
        let row = out
            .lines()
            .find(|line| line.contains("old prompt"))
            .expect("prompt row");
        assert!(
            !row.contains("00:") && !row.contains("04:"),
            "resumed prompt must not invent a clock: {row}"
        );
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
        assert!(wide.contains("path"), "{wide}");
        assert!(
            !wide.contains("/very/long/workspace/path"),
            "header uses the workspace basename: {wide}"
        );
        assert!(
            !wide.contains("abcdef01"),
            "session id is not a header chip: {wide}"
        );

        let narrow: String = app
            .header_line(42)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(!narrow.contains("/very/long"), "{narrow}");
        assert!(narrow.contains("write+shell"), "{narrow}");
        assert!(narrow.contains("ask"), "{narrow}");
    }

    fn header_text(app: &App, width: u16) -> String {
        app.header_line(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn footer_text(app: &App, width: u16) -> String {
        app.footer_line(width)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn header_renders_model_with_dimmed_effort() {
        let mut app = App::new(SessionInfo {
            session_id: "20260814abcdef".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.effort = "medium".into();

        let line = app.header_line(120);
        let text = header_text(&app, 120);
        assert!(text.contains("gpt-5-mini (medium)"), "{text}");
        assert!(
            !text.contains("20260814"),
            "date-like session id stays out of the header: {text}"
        );

        let effort = line
            .spans
            .iter()
            .find(|span| span.content == "medium")
            .expect("effort word");
        assert_eq!(
            effort.style.fg,
            app.theme.note().fg,
            "only the effort word is dimmed"
        );
        let open = line
            .spans
            .iter()
            .find(|span| span.content == " (")
            .expect("opening paren");
        let close = line
            .spans
            .iter()
            .find(|span| span.content == ")")
            .expect("closing paren");
        assert_eq!(open.style.fg, app.theme.header().fg);
        assert_eq!(close.style.fg, app.theme.header().fg);
    }

    #[test]
    fn header_drops_effort_before_model_and_keeps_safety() {
        let mut app = App::new(SessionInfo {
            session_id: "20260814abcdef".into(),
            workspace: "/very/long/workspace/path/that/eats/columns".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.effort = "medium".into();
        app.usage.input_tokens = 41_500;

        let wide = header_text(&app, 160);
        assert!(wide.contains("gpt-5-mini (medium)"), "{wide}");
        assert!(wide.contains("columns"), "{wide}");
        assert!(
            !wide.contains("/very/long/workspace/path"),
            "header uses the workspace basename: {wide}"
        );

        // Peel tokens and workspace first; effort goes before the model name.
        // `mowi · gpt-5-mini (medium)` + safety is 41 cols; 40 forces the
        // parenthetical off and keeps the model.
        let mid = header_text(&app, 40);
        assert!(mid.contains("gpt-5-mini"), "width 40: {mid}");
        assert!(!mid.contains("medium"), "effort peels before model: {mid}");
        assert!(mid.contains("read-only"), "width 40: {mid}");
        assert!(mid.contains("ask"), "width 40: {mid}");

        let tight = header_text(&app, 42);
        assert!(tight.contains("read-only"), "width 42: {tight}");
        assert!(tight.contains("ask"), "width 42: {tight}");
        assert!(!tight.contains("20260814"), "{tight}");
    }

    #[test]
    fn header_left_is_identity_only_and_metrics_sit_right() {
        let mut app = App::new(SessionInfo {
            session_id: "01J8ZK4M7Q2XN5V9".into(),
            workspace: "/home/dev/src/mow".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.effort = "medium".into();
        app.usage.input_tokens = 41_500;
        app.usage.output_tokens = 3_200;
        app.apply_context(&usage(41_500, Some(200_000), Some(21.0)));

        let wide = header_text(&app, 120);
        assert!(wide.contains("mowi"), "{wide}");
        assert!(wide.contains("mow"), "{wide}");
        assert!(!wide.contains("/home/dev/src/mow"), "basename only: {wide}");
        assert!(wide.contains("gpt-5-mini (medium)"), "{wide}");
        assert!(wide.contains("44.7k tok"), "{wide}");
        assert!(wide.contains('▰'), "{wide}");
        assert!(!wide.contains("01J8ZK4M"), "session id stays out: {wide}");
        // Identity left, safety, then tokens immediately before the gauge.
        let mowi = wide.find("mowi").expect("mowi");
        let gauge = wide.find('▰').expect("gauge");
        let tokens = wide.find("44.7k tok").expect("tokens");
        let safety = wide.find("read-only").expect("safety");
        assert!(mowi < safety && safety < tokens && tokens < gauge, "{wide}");
        assert!(
            wide.contains("ask · 44.7k tok"),
            "safety joins metrics with · : {wide}"
        );
        assert!(
            wide.trim_end().ends_with('%'),
            "gauge sits at the right edge: {wide}"
        );

        let mid = header_text(&app, 80);
        assert!(mid.contains("mow"), "{mid}");
        assert!(mid.contains("gpt-5-mini (medium)"), "{mid}");
        assert!(!mid.contains('▰'), "gauge waits for a wide row: {mid}");
        let mid_tokens = mid.find("44.7k tok").expect("tokens");
        let mid_safety = mid.find("read-only").expect("safety");
        assert!(
            mid_safety < mid_tokens,
            "tokens stay right of safety: {mid}"
        );
        assert!(
            mid.contains("ask · 44.7k tok"),
            "safety joins tokens when the gauge is gated: {mid}"
        );
        assert!(
            mid.trim_end().ends_with("44.7k tok"),
            "tokens sit at the right edge when the gauge is gated: {mid}"
        );

        let tight = header_text(&app, 40);
        assert!(tight.contains("gpt-5-mini"), "{tight}");
        assert!(tight.contains("read-only"), "{tight}");
        assert!(
            !tight.contains("44.7k"),
            "tokens drop before identity: {tight}"
        );
        assert!(
            tight.trim_end().ends_with("ask"),
            "no trailing separator when metrics are hidden: {tight}"
        );
        assert!(
            !tight.contains("ask ·"),
            "no trailing · after ask when metrics are hidden: {tight}"
        );
    }

    #[test]
    fn workspace_basename_is_the_last_component() {
        assert_eq!(workspace_basename("/home/dev/src/mow"), "mow");
        assert_eq!(workspace_basename("/home/dev/src/mow/"), "mow");
        assert_eq!(workspace_basename("mow"), "mow");
        assert_eq!(workspace_basename(""), "");
    }

    #[test]
    fn footer_busy_owns_the_live_clock() {
        let mut app = App::new(SessionInfo::default());
        app.busy = true;
        app.animate = false;
        app.activity_started = Some(Instant::now());
        app.status = "calling model".into();
        let text = footer_text(&app, 80);
        assert!(text.contains("calling model"), "{text}");
        assert!(
            text.contains("0.0s"),
            "elapsed belongs on the status bar: {text}"
        );
        assert!(
            !text.contains("idle"),
            "idle must not sit beside the clock: {text}"
        );
        assert!(!text.contains("busy"), "coarse busy is replaced: {text}");
    }

    #[test]
    fn footer_never_shows_session_id() {
        let mut app = App::new(SessionInfo {
            session_id: "20260814abcdef".into(),
            workspace: "/w".into(),
            model: "gpt-5-mini".into(),
            wire: "openai-responses".into(),
        });

        let wide = footer_text(&app, 80);
        assert!(wide.contains("idle"), "{wide}");
        assert!(wide.contains("?"), "{wide}");
        assert!(
            !wide.contains("20260814abcdef"),
            "session id is not status-bar chrome: {wide}"
        );

        app.busy = true;
        app.activity_started = Some(Instant::now());
        app.status = "calling model".into();
        let busy = footer_text(&app, 80);
        assert!(busy.contains("calling model"), "{busy}");
        assert!(busy.contains('?'), "{busy}");
        assert!(
            !busy.contains("20260814abcdef"),
            "session id stays off the busy status bar: {busy}"
        );
        app.busy = false;
        app.status.clear();

        let tight = footer_text(&app, 16);
        assert!(tight.contains("idle"), "{tight}");
        assert!(
            !tight.contains("20260814"),
            "session id is gone at every width: {tight}"
        );

        app.status = "copied locally".into();
        let news = footer_text(&app, 30);
        assert!(news.contains("copied locally"), "{news}");
        assert!(news.contains('?'), "{news}");
        assert!(!news.contains("20260814"), "{news}");

        app.overlay = Overlay::help();
        let help = render(&mut app, 80, 24);
        assert!(
            help.contains("20260814abcdef"),
            "full session id belongs in the help overlay: {help}"
        );
    }

    #[test]
    fn tiny_terminal_paints_a_warning_not_a_broken_frame() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::user("hi"));
        let out = render(&mut app, 30, 8);
        assert!(out.contains("too small"), "{out}");
        assert!(!out.contains("> hi"), "{out}");
    }

    #[test]
    fn welcome_banner_paints_and_clears_without_swallowing_a_key() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.welcome = true;
        let out = render(&mut app, 60, 16);
        assert!(out.contains("type to begin"), "{out}");
        assert!(out.contains("workspace"), "{out}");
        assert!(out.contains("read-only"), "{out}");
        assert!(!out.contains("any key"), "{out}");

        // Min-size first-run: access and the start hint must survive. The
        // tagline and effort are the first things a short pane may drop.
        app.welcome = true;
        app.effort = "medium".into();
        app.allow_write = true;
        app.allow_shell = true;
        let tight = render(&mut app, 40, 14);
        assert!(tight.contains("type to begin"), "{tight}");
        assert!(tight.contains("write+shell"), "{tight}");
        assert!(tight.contains("ask"), "{tight}");

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
        // 60 cols of frame → 58 of textarea inner width (1-col pad each
        // side, no box), so 120 chars need 3 rows.
        assert!(input_height(&app, 58) >= 3, "{}", input_height(&app, 58));
        let out = render(&mut app, 60, 20);
        for row in out.lines() {
            assert!(row.chars().count() <= 60, "{row}");
        }
        let rows = prompt_rows(&app, 58);
        assert!(rows.len() >= 3, "{rows:?}");
        assert!(rows[0].0 && !rows[1].0, "{rows:?}");
    }

    #[test]
    fn help_overlay_lists_local_keys_and_slash_commands() {
        let mut app = App::new(SessionInfo {
            session_id: "01J8ZK4M7Q2XN5V9".into(),
            ..Default::default()
        });
        app.slash_commands.push(SlashCommand {
            name: "review".into(),
            summary: "review changes".into(),
            exclusive: true,
            aliases: vec![],
        });
        app.overlay = Overlay::help();
        let out = render(&mut app, 80, 24);
        assert!(out.contains("help") || out.contains("keyboard"), "{out}");
        assert!(
            out.contains("01J8ZK4M7Q2XN5V9"),
            "full session id belongs on the help card: {out}"
        );
        assert!(out.contains("ctrl+j"), "{out}");
        assert!(
            out.contains("engine history kept)"),
            "action column must not shear the kept) close: {out}"
        );
        assert!(
            !out.contains("ctrl+u"),
            "ctrl+u is not a scroll binding: {out}"
        );
        assert!(
            !out.contains("t (empty"),
            "plain t is not a tool-group shortcut: {out}"
        );
        assert!(out.contains("pgup / pgdn"), "{out}");
        assert!(out.contains("↑ / ↓"), "{out}");
        assert!(
            out.contains("/edit"),
            "last-prompt recall is /edit, not arrow keys: {out}"
        );
        assert!(
            out.contains("/steer"),
            "steer must be advertised as a local command: {out}"
        );
        assert!(
            !out.contains("browse prior"),
            "arrow keys must not be advertised as prompt history: {out}"
        );
        // Extra slash rows push the table past a short pane: the tail is
        // reachable by scrolling, not by being painted at once.
        for _ in 0..app.help_rows().len() {
            app.overlay_move(1);
        }
        let tail = render(&mut app, 70, 24);
        assert!(tail.contains("/review"), "{tail}");
        assert!(tail.contains("/model"), "{tail}");
        assert!(tail.contains("/quit"), "{tail}");
        assert!(tail.contains("/lsp"), "{tail}");
        assert!(tail.contains("/status"), "{tail}");
        assert!(!out.contains("ctrl+s"), "{out}");

        assert!(app.dismiss_overlay());
        assert_eq!(app.overlay, Overlay::None);
        let out = render(&mut app, 70, 20);
        assert!(!out.contains("ctrl+j"), "{out}");
    }

    #[test]
    fn help_overlay_hugs_its_rows_and_survives_narrow() {
        let mut app = App::new(SessionInfo {
            session_id: "01J8ZK4M7Q2XN5V9".into(),
            ..Default::default()
        });
        app.overlay = Overlay::help();
        let n = app.help_rows().len() as u16;

        // A 30-row frame has room for the default table: the card must close
        // after the last row instead of filling the document with empty cells.
        let tall = render(&mut app, 100, 30);
        let lines: Vec<&str> = tall.lines().collect();
        let top = lines
            .iter()
            .position(|line| line.contains('╭'))
            .expect("help top border");
        let bot = lines
            .iter()
            .rposition(|line| line.contains('╰'))
            .expect("help bottom border");
        let overlay_h = (bot - top + 1) as u16;
        assert_eq!(
            overlay_h,
            (n + HELP_CHROME_H).min(HELP_MAX_HEIGHT),
            "help card should hug or cap its rows, got {overlay_h}:\n{tall}"
        );
        assert!(
            overlay_h < 25,
            "help card filled the document pane:\n{tall}"
        );
        assert!(
            tall.contains("/quit"),
            "default rows must fit at 100x30:\n{tall}"
        );
        assert!(
            tall.contains("01J8ZK4M7Q2XN5V9"),
            "session id must not grow the card into a slab:\n{tall}"
        );
        assert!(
            tall.contains("engine history kept)"),
            "action column must not shear the kept) close: {tall}"
        );

        // MIN_WIDTH still paints keys, a usable action stub, and the chrome
        // underneath — a sheared-to-nothing action column is a layout bug.
        let narrow = render(&mut app, MIN_WIDTH, 20);
        assert!(narrow.contains("enter"), "{narrow}");
        assert!(narrow.contains("ctrl+j"), "{narrow}");
        assert!(
            narrow.contains("send") || narrow.contains("queue"),
            "action column vanished at MIN_WIDTH:\n{narrow}"
        );
        assert!(narrow.contains("idle"), "{narrow}");
        assert!(
            narrow.contains("01J8ZK4M7Q2XN5V9"),
            "full session id survives MIN_WIDTH:\n{narrow}"
        );
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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

        app.entries.push(Entry::user("hi"));
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
    fn peer_overlay_hugs_content_and_scrolls_from_the_tail() {
        let mut app = App::new(SessionInfo::default());
        app.peers.insert(
            "peer-agent".into(),
            (1..=30).map(|n| format!("line {n}\n")).collect(),
        );
        assert!(app.toggle_peer_expand());
        app.overlay = Overlay::Peer;
        let tail = render(&mut app, 80, 30);
        assert!(tail.contains("line 30"), "{tail}");
        assert!(!tail.contains("line 1 "), "{tail}");
        assert!(!tail.contains("line 10"), "{tail}");

        app.overlay_move(-1);
        let older = render(&mut app, 80, 30);
        assert!(
            older.contains("line 14") || older.contains("line 13"),
            "one step toward older output:\n{older}"
        );
        assert!(
            !older
                .lines()
                .any(|line| line.contains('│') && line.contains("line 30")),
            "newest card line should leave after scrolling up:\n{older}"
        );

        let short = render(
            &mut {
                app.peers.insert("peer-agent".into(), "one\ntwo".into());
                app.peer_scroll = 0;
                app
            },
            80,
            30,
        );
        let filled_rows = short.lines().filter(|line| line.contains('│')).count();
        assert!(filled_rows <= 5, "peer card stayed too tall:\n{short}");
    }

    #[test]
    fn footer_styles_keys_apart_from_actions() {
        let app = App::new(SessionInfo::default());
        let line = app.footer_line(100);
        let enter = line
            .spans
            .iter()
            .find(|span| span.content.trim() == "enter")
            .expect("enter key span");
        let send = line
            .spans
            .iter()
            .find(|span| span.content.trim() == "send")
            .expect("send action span");
        assert_eq!(enter.style, app.theme.chip());
        assert_eq!(send.style, app.theme.note());
        assert_ne!(enter.style, send.style);
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
    fn status_summary_is_human_readable() {
        let mut app = App::new(SessionInfo {
            session_id: "01J8ZK4M7Q2XN5V9".into(),
            workspace: "/home/dev/mowi".into(),
            model: "composer-2".into(),
            ..Default::default()
        });
        app.apply_status(&serde_json::json!({
            "allow_write": true,
            "allow_shell": true,
            "ask_mode": "ask",
            "effort": "high",
        }));
        app.apply_context(&usage(12_300, Some(200_000), Some(6.15)));
        app.usage.input_tokens = 800;
        app.usage.output_tokens = 400;
        app.usage.peer_tokens = 250;
        let summary = app.status_summary();
        assert!(summary.contains("composer-2"), "{summary}");
        assert!(summary.contains("mowi"), "{summary}");
        assert!(summary.contains("perm ask"), "{summary}");
        assert!(summary.contains("write+shell"), "{summary}");
        assert!(summary.contains("effort high"), "{summary}");
        assert!(summary.contains("context: 12.3k / 200k (6%)"), "{summary}");
        assert!(summary.contains("⇄"), "{summary}");
        assert!(summary.contains("01J8ZK4M7Q2XN5V9"), "{summary}");
        assert!(!summary.contains('{'), "raw JSON leaked: {summary}");
        assert!(
            !summary.contains("allow_write"),
            "raw JSON leaked: {summary}"
        );
    }

    #[test]
    fn lsp_events_stay_to_one_summary_note() {
        let mut app = App::new(SessionInfo::default());
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "harness.lsp.diagnostics",
                "tool": "edit",
                "path": "src/app.rs",
                "count": 8,
                "diagnostics": [
                    {"severity": "error", "message": "undefined: foo", "line": 42, "source": "compiler"},
                    {"severity": "warning", "message": "unused", "line": 8},
                    {"severity": "hint", "message": "prefer", "line": 9},
                ]
            }),
        });
        let notes: Vec<_> = app
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Note(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec!["lsp · src/app.rs · 8 problem(s)"]);
        assert_eq!(app.lsp_problems.len(), 1);
        assert!(!notes.iter().any(|n| n.contains("undefined: foo")));

        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "harness.lsp.diagnostics",
                "path": "clean.go",
                "count": 0
            }),
        });
        assert_eq!(app.lsp_problems.len(), 1);

        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({
                "type": "harness.lsp.diagnostics",
                "path": "src/app.rs",
                "count": 1,
                "diagnostics": [
                    {"severity": "error", "message": "newest", "line": 9}
                ]
            }),
        });
        assert_eq!(app.lsp_problems.len(), 1);
        assert_eq!(app.lsp_problems[0].count, 1);
        assert_eq!(app.lsp_problems[0].diagnostics[0].message, "newest");

        app.show_lsp_problems();
        let last = match app.entries.last() {
            Some(Entry::Note(text)) => text,
            other => panic!("expected detail note, got {other:?}"),
        };
        assert!(last.contains("newest"), "{last}");
        assert!(last.contains("src/app.rs:9"), "{last}");
    }

    #[test]
    fn compact_defers_while_busy_unless_control() {
        assert_eq!(plan_compact(true, false), CompactPlan::Defer);
        assert_eq!(plan_compact(false, false), CompactPlan::Send);
        assert_eq!(plan_compact(true, true), CompactPlan::Send);

        let mut app = App::new(SessionInfo::default());
        assert!(!app.compact_is_control());
        app.busy = true;
        app.pending_compact = Some(0);
        let msg = "compact · applies when the turn finishes";
        app.status = msg.into();
        app.entries.push(Entry::Note(msg.into()));
        assert_eq!(app.pending_compact, Some(0));

        app.set_control_methods(&["compact".into()]);
        assert!(app.compact_is_control());
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
            "effort", "clear", "lsp", "perm", "compact",
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
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
    fn busy_does_not_steal_a_transcript_row() {
        let mut idle = App::new(SessionInfo::default());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut idle)).unwrap();
        let idle_h = idle.last_view_h;

        let mut busy = App::new(SessionInfo::default());
        busy.busy = true;
        busy.activity_started = Some(Instant::now());
        busy.status = "calling model".into();
        terminal.draw(|frame| draw(frame, &mut busy)).unwrap();
        assert_eq!(
            busy.last_view_h, idle_h,
            "busy must not take a transcript row for a clock band"
        );
        assert!(busy.last_view_h > 10, "height was {}", busy.last_view_h);
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
        assert_eq!(app.status, "searching · grep");
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
        let Entry::Tools { tools, .. } = &app.entries[0] else {
            panic!("expected a tool group, got {:?}", app.entries[0]);
        };
        assert_eq!(tool_group_summary(tools), "read · grep · bash");
    }

    #[test]
    fn tool_group_collapsed_paints_one_row_expanded_paints_all() {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.entries.push(Entry::user("run the suite"));
        app.entries.push(Entry::Tools {
            tools: vec![
                ("bash cargo test".into(), Some(940)),
                ("grep estimated_entry_lines".into(), Some(40)),
                ("bash cargo clippy".into(), Some(510)),
            ],
            expanded: false,
        });
        let out = render(&mut app, 80, 20);
        let collapsed_rows: Vec<&str> = out.lines().filter(|l| l.contains("bash ×2")).collect();
        assert_eq!(collapsed_rows.len(), 1, "collapsed group is one row");
        assert!(
            collapsed_rows[0].contains("bash ×2 · grep"),
            "compact counts: {}",
            collapsed_rows[0]
        );
        assert!(!out.contains("3 tool calls"), "{out}");
        assert!(!out.contains("grep estimated_entry_lines"), "{out}");

        assert!(app.toggle_tool_group(), "helper expands the group");
        let out = render(&mut app, 80, 20);
        assert!(out.contains("grep estimated_entry_lines"), "{out}");
        assert!(
            out.contains("3 tool calls"),
            "expanded header keeps the count"
        );
        let tool_rows = out
            .lines()
            .filter(|l| l.contains("bash cargo test") || l.contains("bash cargo clippy"))
            .count();
        assert_eq!(tool_rows, 2, "expanded group paints every call");

        assert!(app.toggle_tool_group(), "helper collapses again");
        let out = render(&mut app, 80, 20);
        assert!(out.contains("bash ×2"), "{out}");
        assert!(!out.contains("grep estimated_entry_lines"), "{out}");
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_view_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn press_ctrl(app: &mut App, c: char) {
        handle_view_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    /// Rightmost column of the transcript pane, and the same column under
    /// the composer + footer (those must stay free of bar glyphs).
    fn scrollbar_columns(app: &mut App, w: u16, h: u16) -> (String, String) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let x = w.saturating_sub(1);
        // header + hairline, then transcript, then composer, then 2-row footer.
        let input_h = input_height(app, w.saturating_sub(2));
        let transcript_top = 2u16;
        let transcript_bot = h.saturating_sub(2 + input_h + 1);
        let bar: String = (transcript_top..=transcript_bot)
            .map(|y| buf[(x, y)].symbol().to_string())
            .collect();
        let below: String = (transcript_bot.saturating_add(1)..h)
            .map(|y| buf[(x, y)].symbol().to_string())
            .collect();
        (bar, below)
    }

    fn long_scroll_app() -> App {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::plain(ThemeName::CatppuccinMocha);
        app.welcome = false;
        for i in 0..40 {
            app.entries.push(Entry::user(format!("user-line-{i}")));
            app.entries
                .push(Entry::Assistant(format!("assistant-line-{i}")));
        }
        app
    }

    #[test]
    fn scrollbar_state_uses_scroll_offsets_not_line_count() {
        let follow = format!("{:?}", transcript_scrollbar_state(100, 20, 0, true));
        assert!(
            follow.contains("content_length: 81"),
            "follow length is max_scroll+1, not 100: {follow}"
        );
        assert!(
            follow.contains("position: 80"),
            "follow position is max_scroll, not 99: {follow}"
        );
        let top = format!("{:?}", transcript_scrollbar_state(100, 20, 0, false));
        assert!(top.contains("position: 0"), "{top}");
        let mid = format!("{:?}", transcript_scrollbar_state(100, 20, 40, false));
        assert!(mid.contains("position: 40"), "{mid}");
    }

    fn assert_scrollbar_track(bar: &str, below: &str, where_: &str) {
        assert!(
            bar.starts_with('↑') && bar.ends_with('↓'),
            "{where_}: track must run ↑…↓ across the transcript, got {bar:?}"
        );
        assert!(
            !below.chars().any(|c| matches!(c, '↑' | '↓' | '║' | '█')),
            "{where_}: bar leaked into composer/footer {below:?}"
        );
    }

    #[test]
    fn transcript_scrollbar_reaches_the_pane_bottom() {
        for height in [12u16, 24] {
            let mut app = long_scroll_app();
            app.follow = false;
            app.scroll = 0;
            let (top, below) = scrollbar_columns(&mut app, 80, height);
            assert_scrollbar_track(&top, &below, &format!("h={height} top"));
            assert!(
                top.chars().nth(1) == Some('█'),
                "h={height} top: thumb should sit under ↑, got {top}"
            );

            let max = max_scroll(&app);
            assert!(max > 4, "fixture must overflow, max={max}");
            app.scroll = max / 2;
            let (mid, below) = scrollbar_columns(&mut app, 80, height);
            assert_scrollbar_track(&mid, &below, &format!("h={height} mid"));
            let mid_thumb = mid.find('█').expect("mid thumb");
            let mid_thumb_end = mid.rfind('█').expect("mid thumb end");
            assert!(
                mid_thumb > 1 && mid_thumb_end + 1 < mid.len() - 1,
                "h={height} mid: thumb should sit in the track, got {mid}"
            );

            app.follow = true;
            app.scroll = 0;
            let (bot, below) = scrollbar_columns(&mut app, 80, height);
            assert_scrollbar_track(&bot, &below, &format!("h={height} bottom"));
            assert!(
                bot.chars().rev().nth(1) == Some('█'),
                "h={height} bottom: thumb should sit on ↓, got {bot}"
            );
        }
    }

    fn tall_transcript(app: &mut App) {
        app.last_view_h = 6;
        app.last_view_w = 40;
        for i in 0..20 {
            app.entries.push(Entry::user(format!("user-{i}")));
            app.entries.push(Entry::Assistant(format!("asst-{i}")));
        }
        app.follow = true;
        app.scroll = 0;
    }

    #[test]
    fn plain_t_always_types_into_the_prompt() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::Tools {
            tools: vec![("read a".into(), Some(1)), ("read b".into(), Some(2))],
            expanded: false,
        });
        assert!(app.input.is_empty());
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.input, "t", "t types even as the first character");
        let still_collapsed = app.entries.iter().any(|entry| {
            matches!(
                entry,
                Entry::Tools {
                    expanded: false,
                    ..
                }
            )
        });
        assert!(still_collapsed, "t must not expand the tool group");
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.input, "tt");
    }

    #[test]
    fn arrow_keys_scroll_transcript_and_never_rewrite_composer() {
        let mut app = App::new(SessionInfo::default());
        tall_transcript(&mut app);
        app.entries.push(Entry::user("last prompt"));
        assert!(app.input.is_empty());

        press(&mut app, KeyCode::Up);
        assert_eq!(
            app.input, "",
            "arrow-up must scroll, not recall the last prompt"
        );
        assert!(!app.follow, "arrow-up leaves follow");

        app.set_input("draft".into());
        let scroll = app.scroll;
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "draft");
        assert!(app.scroll < scroll || scroll == 0);
        assert!(!app.follow);

        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "draft", "arrow-down must not rewrite the draft");
    }

    #[test]
    fn scroll_keys_never_rewrite_the_composer() {
        let mut app = App::new(SessionInfo::default());
        tall_transcript(&mut app);
        app.set_input("keep me".into());

        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.input, "keep me");
        assert!(!app.follow, "pgup scrolls the transcript");

        app.follow = true;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.input, "keep me");
        assert!(app.follow, "pgdn at the bottom stays in follow");

        app.follow = true;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "keep me");
        assert!(app.follow, "arrow-down at the bottom stays in follow");

        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "keep me");
        assert!(!app.follow, "arrow-up scrolls the transcript");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "keep me");

        app.follow = true;
        press_ctrl(&mut app, 'u');
        press_ctrl(&mut app, 'd');
        assert_eq!(app.input, "keep me", "ctrl+u/d must not type or recall");
        assert!(app.follow, "ctrl+u/d must not scroll");
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_only() {
        let mut app = App::new(SessionInfo::default());
        tall_transcript(&mut app);
        app.set_input("keep me".into());

        handle_mouse(&mut app, MouseEventKind::ScrollUp);
        assert_eq!(app.input, "keep me");
        assert!(!app.follow, "wheel up scrolls the transcript");
        // Regression (post-scroll freeze): scrolling back down to the tail
        // must re-enter follow so streaming auto-scroll resumes.
        for _ in 0..1000 {
            handle_mouse(&mut app, MouseEventKind::ScrollDown);
            if app.follow {
                break;
            }
        }
        assert!(app.follow, "scrolling back to the bottom re-enters follow");

        handle_mouse(&mut app, MouseEventKind::ScrollDown);
        assert_eq!(app.input, "keep me", "wheel down must not rewrite input");

        app.overlay = Overlay::help();
        app.follow = true;
        handle_mouse(&mut app, MouseEventKind::ScrollUp);
        assert!(app.follow, "wheel is ignored while an overlay is open");
        assert_eq!(app.input, "keep me");
    }

    #[test]
    fn tool_group_toggle_has_nothing_to_do_without_a_group() {
        let mut app = App::new(SessionInfo::default());
        assert!(!app.toggle_tool_group(), "no group: nothing to toggle");
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
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        let collapsed = Entry::Tools {
            tools: vec![
                (
                    "bash echo ----; cat AGENTS.md; ls -la; git log --oneline".into(),
                    Some(90),
                ),
                ("write docs/roadmap.md".into(), Some(60)),
                ("read src/render.rs".into(), Some(30)),
                ("bash cargo test".into(), Some(940)),
            ],
            expanded: false,
        };
        let mut expanded = collapsed.clone();
        if let Entry::Tools { expanded: e, .. } = &mut expanded {
            *e = true;
        }
        for width in [20u16, 32, 40, 60, 80] {
            app.last_view_w = width;
            let painted = app.entry_lines(&collapsed).len();
            let estimated = app.estimated_entry_lines(&collapsed) - 1; // minus separator
            assert_eq!(
                estimated, painted,
                "collapsed w={width}: estimated {estimated}, painted {painted}"
            );
            let text: String = app.entry_lines(&collapsed)[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            let body = text.strip_prefix("⚙ ").unwrap_or(&text);
            let summary = match body.rsplit_once(" · ") {
                Some((head, tail))
                    if tail.ends_with('s') && tail.trim_end_matches('s').parse::<f64>().is_ok() =>
                {
                    head
                }
                _ => body,
            };
            assert_summary_tokens(summary, &["bash", "write", "read"]);
            assert!(painted >= 1, "collapsed group must paint the summary row");

            let painted = app.entry_lines(&expanded).len();
            let estimated = app.estimated_entry_lines(&expanded) - 1;
            assert!(
                estimated >= painted,
                "expanded w={width}: estimated {estimated}, painted {painted}"
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
        app.theme = Theme::plain(ThemeName::CatppuccinMocha);
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
                    timestamp: None,
                    role: "user".into(),
                    content: "hi".into(),
                },
                TranscriptMessage {
                    timestamp: None,
                    role: "assistant".into(),
                    content: "hello".into(),
                },
                TranscriptMessage {
                    timestamp: None,
                    role: "tool".into(),
                    content: "grep".into(),
                },
            ],
        );
        assert_eq!(
            app.entries,
            vec![
                Entry::User {
                    text: "hi".into(),
                    at: None,
                },
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
        app.entries.push(Entry::user("find this"));
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
        assert!(app.footer().contains("copied"), "{}", app.footer());
        let seq = app.take_osc52().expect("OSC52 queued for the next draw");
        assert!(seq.starts_with("\x1b]52;c;"), "{seq:?}");
        assert!(seq.ends_with('\x07'), "{seq:?}");
        assert!(!seq.contains("answer"), "payload is base64, not raw text");
        assert_eq!(osc52_sequence("hi"), Some("\x1b]52;c;aGk=\x07".into()));
    }

    #[test]
    fn cancel_drops_queue_and_cancelled_completion_does_not_drain() {
        let mut app = App::new(SessionInfo::default());
        assert!(app.enqueue_prompt("follow-up".into()));
        app.request_cancel();
        assert!(app.queue.is_empty());
        assert!(
            app.entries
                .iter()
                .any(|entry| matches!(entry, Entry::Note(text) if text.contains("dropped 1"))),
            "{:?}",
            app.entries
        );
        assert!(app.take_queued_after_turn(true).is_none());
        assert!(app.queue.is_empty());

        let mut app = App::new(SessionInfo::default());
        assert!(app.enqueue_prompt("next".into()));
        assert_eq!(app.take_queued_after_turn(true).as_deref(), Some("next"));

        let mut app = App::new(SessionInfo::default());
        assert!(app.enqueue_prompt("keep".into()));
        assert!(app.take_queued_after_turn(false).is_none());
        assert_eq!(app.queue.len(), 1);
    }

    #[test]
    fn rewind_followup_replaces_the_old_turn_then_edits() {
        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::user("old prompt"));
        app.entries.push(Entry::Assistant("old answer".into()));
        app.rewind_user = Some("old prompt".into());
        app.load_transcript(vec![TranscriptMessage {
            timestamp: None,
            role: "user".into(),
            content: "earlier".into(),
        }]);
        let last = app.rewind_user.take().unwrap();
        app.set_input(last);
        assert_eq!(app.input, "old prompt");
        assert!(
            !app.entries
                .iter()
                .any(|entry| matches!(entry, Entry::Assistant(text) if text == "old answer")),
            "rewound transcript must not keep the discarded answer: {:?}",
            app.entries
        );
        assert_eq!(app.last_user_prompt().as_deref(), Some("earlier"));

        let mut app = App::new(SessionInfo::default());
        app.entries.push(Entry::user("keep"));
        app.entries.push(Entry::Assistant("keep-a".into()));
        app.entries.push(Entry::user("drop"));
        app.entries.push(Entry::Assistant("drop-a".into()));
        app.drop_last_turn_entries();
        assert_eq!(app.last_user_prompt().as_deref(), Some("keep"));
        assert!(
            !app.entries
                .iter()
                .any(|entry| matches!(entry, Entry::Assistant(text) if text == "drop-a"))
        );
    }

    #[test]
    fn reasoning_and_think_tags_never_paint() {
        let mut app = App::new(SessionInfo::default());
        app.busy = true;
        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type":"loop.reasoning","delta":"secret plan details"}),
        });
        assert_eq!(app.status, "thinking");
        assert!(app.thinking);
        assert!(app.live.is_empty(), "{}", app.live);
        let painted = render(&mut app, 80, 18);
        assert!(
            !painted.contains("secret plan details"),
            "reasoning body leaked:\n{painted}"
        );
        assert!(painted.contains("thinking"), "{painted}");

        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type":"loop.token","delta":"<think>do not show this"}),
        });
        assert!(app.live.is_empty(), "unclosed think must hide answer");
        assert!(
            !render(&mut app, 80, 18).contains("do not show this"),
            "open think leaked"
        );

        app.on_notification(&Notification {
            method: "event".into(),
            params: serde_json::json!({"type":"loop.token","delta":"</think>## Hello"}),
        });
        assert_eq!(app.live, "## Hello");
        assert!(!app.live.contains("do not show"));
        let painted = render(&mut app, 80, 18);
        assert!(
            painted.contains("## Hello") || painted.contains("Hello"),
            "{painted}"
        );
        assert!(!painted.contains("do not show this"), "{painted}");
        assert!(!painted.contains("secret plan"), "{painted}");

        app.finish_turn(Ok(serde_json::json!({
            "text": "<think>still secret</think>Final answer."
        })));
        assert!(
            app.entries.iter().any(|entry| matches!(
                entry,
                Entry::Assistant(text) if text.contains("Final answer") && !text.contains("still secret")
            )),
            "{:?}",
            app.entries
        );
    }

    #[test]
    fn think_strip_does_not_weld_surrounding_prose() {
        let mut app = App::new(SessionInfo::default());
        app.push_visible_token("key files.<think>plan the approach</think>Let me go");
        assert_eq!(app.live, "key files. Let me go");
        assert!(!app.live.contains("plan the approach"));
    }

    fn huge_markdown(sections: usize) -> String {
        let mut text = String::new();
        for i in 0..sections {
            text.push_str(&format!("## Heading {i}\n\n"));
            text.push_str(
                "This is a paragraph with **bold** and `code` and a [link](https://example.com).\n\n",
            );
            text.push_str("```rust\n");
            for j in 0..20 {
                text.push_str(&format!(
                    "    fn item_{i}_{j}() {{ println!(\"{i}:{j}\"); }}\n"
                ));
            }
            text.push_str("```\n\n");
        }
        text
    }

    fn large_session_with_buried_answer() -> App {
        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.animate = false;
        app.last_view_w = 80;
        app.last_view_h = 24;
        for i in 0..80 {
            app.entries.push(Entry::user(format!("q{i}")));
            app.entries
                .push(Entry::Assistant(format!("short answer {i}")));
        }
        app.entries.push(Entry::user("explain the whole file"));
        app.entries.push(Entry::Assistant(huge_markdown(80)));
        for i in 0..8 {
            app.entries.push(Entry::user(format!("followup {i}")));
            app.entries.push(Entry::Assistant(format!("ok {i}")));
        }
        app.follow = true;
        app.scroll = 0;
        app
    }

    fn window_line_bound(viewport: usize) -> usize {
        viewport + TRANSCRIPT_OVERSCAN * 2 + 8
    }

    #[test]
    fn live_height_is_counted_in_the_document() {
        let mut app = App::new(SessionInfo::default());
        app.last_view_w = 80;
        app.live = "hello\nworld\n".repeat(50);
        let n = app.estimated_total_lines();
        assert!(
            n > 50,
            "live must not collapse to one estimated row, got {n}"
        );
    }

    #[test]
    fn scroll_up_virtualizes_large_transcript_and_live() {
        use std::time::Instant;

        let mut app = large_session_with_buried_answer();
        let bound = window_line_bound(24);

        let follow_n = app.visible_transcript_lines().0.len();
        assert!(
            follow_n <= bound,
            "follow materialized {follow_n} lines (bound {bound})"
        );

        leave_follow(&mut app, 5);
        for _ in 0..60 {
            leave_follow(&mut app, 5);
        }
        assert!(!app.follow);
        let scroll_n = app.visible_transcript_lines().0.len();
        assert!(
            scroll_n <= bound,
            "scroll-up materialized {scroll_n} lines (bound {bound})"
        );
        let out = render(&mut app, 80, 24);
        assert!(
            out.contains("Heading") || out.contains("short answer") || out.contains("followup"),
            "scrolled frame painted nothing recognizable:\n{out}"
        );

        // A streaming turn must not rematerialize the whole live buffer when
        // the operator has already paged into earlier history.
        app.live = huge_markdown(200);
        app.busy = true;
        let with_live = app.visible_transcript_lines().0.len();
        assert!(
            with_live <= bound,
            "scroll-up with live materialized {with_live} lines (bound {bound})"
        );

        app.follow = true;
        app.scroll = 0;
        let follow_live = app.visible_transcript_lines().0.len();
        assert!(
            follow_live <= bound,
            "follow with live materialized {follow_live} lines (bound {bound})"
        );

        // Hang detector: a handful of scroll+draw steps on this session used
        // to rematerialize thousands of lines per frame.
        let started = Instant::now();
        for _ in 0..8 {
            leave_follow(&mut app, 5);
            let _ = render(&mut app, 80, 24);
        }
        let ms = started.elapsed().as_millis();
        assert!(
            ms < 800,
            "scroll+draw loop took {ms}ms on a virtualized window"
        );
    }

    fn line_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn height_cache_reuses_estimates_on_large_session() {
        use std::time::Instant;

        let app = large_session_with_buried_answer();
        let first = app.estimated_total_lines();
        assert!(
            first > 1_000,
            "buried answer should dominate height, got {first}"
        );
        let started = Instant::now();
        for _ in 0..40 {
            assert_eq!(app.estimated_total_lines(), first);
        }
        let ms = started.elapsed().as_millis();
        assert!(
            ms < 40,
            "cached estimated_total_lines x40 took {ms}ms (was ~80ms uncached)"
        );
    }

    #[test]
    fn live_height_incremental_matches_full_recompute() {
        let mut app = App::new(SessionInfo::default());
        app.last_view_w = 80;
        app.live = "hello\nworld\n".repeat(80);
        let after_seed = app.estimated_live_lines();

        app.live.push_str("more tokens **bold**\n");
        app.live.push_str(&"tail line\n".repeat(20));
        let incremental = app.estimated_live_lines();

        let mut fresh = App::new(SessionInfo::default());
        fresh.last_view_w = 80;
        fresh.live = app.live.clone();
        assert_eq!(incremental, fresh.estimated_live_lines());
        assert!(incremental > after_seed);

        // Wholesale replace must not be treated as an append.
        app.live = huge_markdown(12);
        let replaced = app.estimated_live_lines();
        fresh.live = app.live.clone();
        fresh.height_cache.borrow_mut().clear();
        assert_eq!(replaced, fresh.estimated_live_lines());
    }

    #[test]
    fn transcript_memory_ceiling_keeps_recent_entries_and_reload_hint() {
        let mut app = App::new(SessionInfo::default());
        for i in 0..(TRANSCRIPT_ENTRY_CEILING + 200) {
            app.entries.push(Entry::Assistant(format!("answer {i}")));
        }
        app.enforce_transcript_memory_ceiling();
        assert!(app.entries.len() <= TRANSCRIPT_TRIM_TARGET + 1);
        assert!(
            matches!(app.entries.first(), Some(Entry::Note(note)) if note == EARLIER_ENTRIES_NOTE)
        );
        assert!(
            matches!(app.entries.last(), Some(Entry::Assistant(text)) if text == &format!("answer {}", TRANSCRIPT_ENTRY_CEILING + 199))
        );
    }

    #[test]
    fn large_entry_index_jumps_to_scrolled_window() {
        let mut app = App::new(SessionInfo::default());
        app.last_view_w = 80;
        app.last_view_h = 24;
        for i in 0..20_000 {
            app.entries.push(Entry::Note(format!("row {i}")));
        }
        let _ = app.estimated_total_lines();
        app.follow = false;
        app.scroll = max_scroll(&app).saturating_sub(10);
        let started = Instant::now();
        for _ in 0..30 {
            let lines = app.visible_transcript_lines().0;
            assert!(lines.len() <= window_line_bound(24));
        }
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "indexed scroll window regressed: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn scrolled_paint_reuses_cached_entry_without_full_clone() {
        use std::time::Instant;

        let mut app = large_session_with_buried_answer();
        for _ in 0..80 {
            leave_follow(&mut app, 5);
        }
        let first = app.visible_transcript_lines().0.len();
        let bound = window_line_bound(24);
        assert!(first <= bound, "first scroll window {first} > {bound}");

        let started = Instant::now();
        for _ in 0..30 {
            let n = app.visible_transcript_lines().0.len();
            assert!(n <= bound, "cached scroll window {n} > {bound}");
        }
        let ms = started.elapsed().as_millis();
        assert!(
            ms < 40,
            "cached scrolled visible_transcript_lines x30 took {ms}ms"
        );
    }

    #[test]
    fn live_follow_parses_visible_tail_not_whole_stream() {
        use std::time::Instant;

        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.animate = false;
        app.last_view_w = 80;
        app.last_view_h = 24;
        app.follow = true;
        app.busy = true;
        app.live = huge_markdown(80);

        let (lines, _) = app.visible_transcript_lines();
        let text = line_text(&lines);
        assert!(
            text.contains("Heading 79"),
            "follow tail must show the latest heading:\n{text}"
        );
        assert!(
            !text.contains("Heading 0"),
            "follow tail must not rematerialize the start of a 2k-line stream:\n{text}"
        );
        assert!(
            lines.len() <= window_line_bound(24),
            "live follow materialized {} lines",
            lines.len()
        );

        let started = Instant::now();
        for i in 0..20 {
            app.live.push_str(&format!("\nstreaming token {i} **x**\n"));
            let (lines, _) = app.visible_transcript_lines();
            assert!(
                lines.len() <= window_line_bound(24),
                "live tick materialized {} lines",
                lines.len()
            );
        }
        let ms = started.elapsed().as_millis();
        assert!(
            ms < 200,
            "live follow tail stream x20 took {ms}ms (full markdown was ~8ms/tick)"
        );
        let text = line_text(&app.visible_transcript_lines().0);
        assert!(
            text.contains("streaming token 19"),
            "latest tokens must remain visible:\n{text}"
        );
    }

    #[test]
    fn live_tail_source_extends_to_open_fence() {
        let mut body = String::from("intro\n\n```rust\n");
        body.push_str(&"    fn buried() {}\n".repeat(40));
        body.push_str("    fn visible() {}\n```\n");
        let suffix = live_tail_source(&body, 8, 80);
        assert!(
            suffix.contains("```rust"),
            "odd fence count must pull in the opening fence:\n{suffix}"
        );
        assert!(suffix.contains("fn visible"), "{suffix}");
    }

    #[test]
    fn needs_paint_skips_idle_and_keeps_busy() {
        let mut app = App::new(SessionInfo::default());
        assert!(needs_paint(&app, true));
        assert!(!needs_paint(&app, false));
        app.busy = true;
        assert!(needs_paint(&app, false));
    }

    #[test]
    fn two_thousand_entry_session_stays_virtualized() {
        use std::time::Instant;

        let mut app = App::new(SessionInfo::default());
        app.theme = Theme::colored(ThemeName::CatppuccinMocha);
        app.animate = false;
        app.last_view_w = 80;
        app.last_view_h = 24;
        for i in 0..2_000 {
            app.entries.push(Entry::user(format!("q{i}")));
            app.entries
                .push(Entry::Assistant(format!("short answer {i}")));
        }
        app.follow = true;
        let bound = window_line_bound(24);
        let first = app.visible_transcript_lines().0.len();
        assert!(first <= bound, "2k-entry follow materialized {first}");
        let _ = app.estimated_total_lines();

        let started = Instant::now();
        for _ in 0..20 {
            let n = app.visible_transcript_lines().0.len();
            assert!(n <= bound, "2k-entry window {n} > {bound}");
        }
        let ms = started.elapsed().as_millis();
        assert!(ms < 50, "2k-entry visible_transcript_lines x20 took {ms}ms");
    }
}
