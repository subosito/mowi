//! JSON-lines client for `mow rpc` (compatibility epoch 1).
//!
//! The Engine is a child process: mowi writes requests to its stdin and reads
//! responses plus notifications from its stdout. Stderr is Engine logging and
//! is never parsed.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// Protocol version this client speaks.
pub const RPC_COMPATIBILITY_EPOCH: u32 = 1;

#[derive(Debug)]
pub enum Error {
    /// The mow binary could not be started.
    Spawn(String),
    Io(io::Error),
    /// JSON-RPC error envelope from the server.
    Rpc {
        code: i64,
        message: String,
    },
    /// Handshake / shape problem.
    Protocol(String),
    /// Child exited or the reader thread stopped.
    Closed,
    Timeout,
    /// Operator asked to resume another session; the TUI should respawn.
    ResumeSession(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn(m) => write!(f, "{m}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            Error::Protocol(m) => write!(f, "{m}"),
            Error::Closed => write!(f, "mow rpc connection closed"),
            Error::Timeout => write!(f, "timed out waiting for mow rpc"),
            Error::ResumeSession(id) => write!(f, "resume session {id}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// A server-pushed notification (`event`, `perm.ask`, …): method, no id.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// A resumable session returned by `sessions`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub updated: String,
    pub preview: String,
}

/// One stored transcript message.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptMessage {
    pub role: String,
    pub content: String,
    /// RFC 3339 timestamp when the host/session format has one.
    pub timestamp: Option<String>,
}

/// One model from `model.list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub current: bool,
    pub wire: String,
}

/// `model.list` result.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
    pub current: String,
}

/// One effort from `effort.list`.
#[derive(Debug, Clone, PartialEq)]
pub struct EffortInfo {
    pub id: String,
    pub current: bool,
}

/// `context` result — drives the header used/window chip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub context_window: Option<u64>,
    pub remaining: Option<u64>,
    pub percent: Option<f64>,
}

impl ContextUsage {
    pub fn from_value(value: &Value) -> Self {
        Self {
            tokens: value.get("tokens").and_then(Value::as_u64).unwrap_or(0),
            context_window: value.get("context_window").and_then(Value::as_u64),
            remaining: value.get("remaining").and_then(Value::as_u64),
            percent: value.get("percent").and_then(Value::as_f64),
        }
    }
}

/// `compact` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactReport {
    pub layer: String,
    pub chars_saved: i64,
    pub chars_before: i64,
    pub chars_after: i64,
    pub messages_before: i64,
    pub messages_after: i64,
    pub over_budget: bool,
    pub tokens: u64,
}

/// `effort.list` result.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EffortList {
    pub efforts: Vec<EffortInfo>,
    pub current: String,
    pub default: String,
}

/// A slash command advertised by `slash.list`.
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub summary: String,
    pub exclusive: bool,
    pub aliases: Vec<String>,
}

/// A permission request notification.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub tool_call_id: String,
}

impl Notification {
    /// Decode a `perm.ask` notification, if this notification is one.
    pub fn permission_request(&self) -> Option<PermissionRequest> {
        if self.method != "perm.ask" {
            return None;
        }
        Some(PermissionRequest {
            id: self.params.get("id")?.as_str()?.to_string(),
            name: self.params.get("name")?.as_str()?.to_string(),
            args: self.params.get("args").cloned().unwrap_or(Value::Null),
            tool_call_id: self
                .params
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }
}

/// One decoded stdout line.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Response {
        id: u64,
        result: Result<Value, (i64, String)>,
    },
    Notify(Notification),
}

/// Decode one stdout line. Unknown / unparsable lines yield `None` (the
/// server may emit blank lines; stderr never reaches here).
pub fn parse_message(line: &str) -> Option<Message> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    if let Some(id) = obj.get("id").and_then(|i| i.as_u64()) {
        if let Some(err) = obj.get("error").and_then(|e| e.as_object()) {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32603);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Some(Message::Response {
                id,
                result: Err((code, message)),
            });
        }
        let result = obj.get("result").cloned().unwrap_or(Value::Null);
        return Some(Message::Response {
            id,
            result: Ok(result),
        });
    }
    let method = obj.get("method")?.as_str()?.to_string();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Some(Message::Notify(Notification { method, params }))
}

/// `version` / `capabilities` result.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
    pub rpc: String,
    /// Methods this server advertises. Empty means "not advertised" — the
    /// client must not infer a full stock build.
    pub methods: Vec<String>,
    /// Subset answered while a prompt is in flight.
    pub control_methods: Vec<String>,
    /// Boolean features a method name cannot express (`ephemeral_prompt`, …).
    pub features: BTreeMap<String, bool>,
}

/// Pull a `[String]` field, tolerating absence and non-string members.
fn string_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pull `features` object values that look boolean.
pub fn decode_features(v: &Value) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if let Some(obj) = v.get("features").and_then(Value::as_object) {
        for (key, val) in obj {
            let flag = match val {
                Value::Bool(flag) => *flag,
                Value::String(text) => matches!(text.as_str(), "true" | "1" | "yes"),
                Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
                _ => continue,
            };
            out.insert(key.clone(), flag);
        }
    }
    if let Some(rows) = v.pointer("/optional/features").and_then(Value::as_array) {
        for row in rows {
            if row.get("linked").and_then(Value::as_bool).unwrap_or(true)
                && let Some(id) = row.get("id").and_then(Value::as_str)
            {
                out.insert(id.to_string(), true);
            }
        }
    }
    out
}

/// Merge a `capabilities` result onto a `version` snapshot.
pub fn merge_capabilities(mut version: VersionInfo, v: &Value) -> VersionInfo {
    let methods = string_list(v, "methods");
    if !methods.is_empty() {
        version.methods = methods;
    }
    let control = string_list(v, "control_methods");
    if !control.is_empty() {
        version.control_methods = control;
    }
    let features = decode_features(v);
    if !features.is_empty() {
        version.features = features;
    }
    version
}

/// Validate a `version` result.
///
/// The compatibility epoch is an exact match, not a floor: a future epoch
/// means an incompatible wire contract. Additive methods stay on epoch 1 and
/// are discovered through `methods` / `control_methods` / `features`.
pub fn check_version(v: &Value) -> Result<VersionInfo, Error> {
    let rpc = v
        .get("rpc")
        .and_then(|r| r.as_str())
        .ok_or_else(|| Error::Protocol("mow rpc: version result has no \"rpc\" field".into()))?;
    let n: u32 = rpc
        .trim()
        .parse()
        .map_err(|_| Error::Protocol(format!("mow rpc: unrecognized protocol version {rpc:?}")))?;
    if n != RPC_COMPATIBILITY_EPOCH {
        return Err(Error::Protocol(format!(
            "mow rpc compatibility epoch {rpc:?}, need {RPC_COMPATIBILITY_EPOCH}; use compatible mow and mowi builds"
        )));
    }
    Ok(VersionInfo {
        name: v
            .get("name")
            .and_then(|s| s.as_str())
            .unwrap_or("mow")
            .to_string(),
        version: v
            .get("version")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        rpc: rpc.to_string(),
        methods: string_list(v, "methods"),
        control_methods: string_list(v, "control_methods"),
        features: decode_features(v),
    })
}

/// `session` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub wire: String,
    /// Present only when the host sent `extra_roots` / `extra_root_count`.
    pub extra_roots: Vec<ExtraRoot>,
}

impl SessionInfo {
    pub fn from_value(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        SessionInfo {
            session_id: s("session_id"),
            workspace: s("workspace"),
            model: s("model"),
            wire: s("wire"),
            extra_roots: decode_extra_roots(v).unwrap_or_default(),
        }
    }

    /// Short id for the status bar, when leftover columns allow it.
    #[cfg(test)]
    pub fn short_id(&self) -> String {
        let id = self.session_id.as_str();
        match id.char_indices().nth(8) {
            Some((i, _)) => id[..i].to_string(),
            None => id.to_string(),
        }
    }
}

/// Live assistant text carried by an `event` notification, if any.
///
/// Peer chunks (`harness.delegate.*`) are deliberately excluded: they are not
/// the host answer. Reasoning (`loop.reasoning`) is also excluded: the UI
/// may arm a thinking indicator, but the body must never paint.
pub fn token_delta(params: &Value) -> Option<&str> {
    let kind = event_type(params);
    if kind.contains("delegate") {
        return None;
    }
    // Host answer stream only. Reasoning / tool deltas must not weld into live text.
    if kind != "loop.token" {
        return None;
    }
    params
        .get("delta")
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
}

/// Reasoning-channel delta. Presence-only for the UI: the text is never painted.
pub fn reasoning_delta(params: &Value) -> Option<&str> {
    let kind = event_type(params);
    if kind != "loop.reasoning" && kind != "reasoning" {
        return None;
    }
    params
        .get("delta")
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
}

/// Tool name on `harness.tool.start` / `harness.tool.end` (also bare `tool.*`).
///
/// Confirmed mow `Event` field: `tool`. Older hosts may send `name`.
pub fn tool_name(params: &Value) -> Option<&str> {
    params
        .get("tool")
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Raw tool args object/string from a tool start/end event.
pub fn tool_args(params: &Value) -> Option<&Value> {
    params.get("args")
}

/// Tool result body on `harness.tool.end` (may already be truncated by the host).
pub fn tool_result(params: &Value) -> Option<&str> {
    params
        .get("result")
        .and_then(Value::as_str)
        .filter(|result| !result.is_empty())
}

/// `denied` on `harness.tool.end`.
pub fn tool_denied(params: &Value) -> bool {
    params
        .get("denied")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `error` on `harness.tool.end`. Empty string is treated as absent.
pub fn tool_error(params: &Value) -> Option<&str> {
    params
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty())
}

/// Display label `verb detail` from a tool event, matching mow `FormatToolProgress`.
///
/// A name that already carries its argument (`read src/app.rs`) is left alone
/// so tests and older hosts that inline the path keep working. Prefer
/// [`tool_progress_label_for`] when the session workspace is known so in-root
/// files paint as relative paths.
pub fn tool_progress_label(tool: &str, args: Option<&Value>) -> String {
    tool_progress_label_for(tool, args, "", &[])
}

/// Like [`tool_progress_label`], but workspace files are shown relative to
/// `workspace`. Extra-root and any other absolute path stay full.
pub fn tool_progress_label_for(
    tool: &str,
    args: Option<&Value>,
    workspace: &str,
    extra_roots: &[ExtraRoot],
) -> String {
    let tool = tool.trim();
    if tool.is_empty() {
        return String::new();
    }
    if tool.contains(char::is_whitespace) {
        return rewrite_composed_tool_label(tool, workspace, extra_roots);
    }
    let detail = args
        .map(|args| tool_progress_detail(tool, args, workspace, extra_roots))
        .unwrap_or_default();
    if detail.is_empty() {
        tool.to_string()
    } else {
        format!("{tool} {detail}")
    }
}

fn rewrite_composed_tool_label(label: &str, workspace: &str, extra_roots: &[ExtraRoot]) -> String {
    let mut parts = label.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    if rest.is_empty() {
        return label.to_string();
    }
    match verb.to_ascii_lowercase().as_str() {
        "read" | "write" | "edit" | "delete" => {
            format!("{verb} {}", display_jail_path(rest, workspace, extra_roots))
        }
        _ => label.to_string(),
    }
}

fn tool_progress_detail(
    tool: &str,
    args: &Value,
    workspace: &str,
    extra_roots: &[ExtraRoot],
) -> String {
    let get = |key: &str| -> String {
        match args {
            Value::Object(map) => map
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            Value::String(raw) => raw.trim().to_string(),
            _ => String::new(),
        }
    };
    match tool.to_ascii_lowercase().as_str() {
        "read" | "write" | "edit" | "delete" => {
            clip_runes(&display_jail_path(&get("path"), workspace, extra_roots), 72)
        }
        "glob" => clip_runes(&get("pattern"), 72),
        "grep" => {
            let pat = clip_runes(&get("pattern"), 40);
            if pat.is_empty() {
                return String::new();
            }
            let path = get("path");
            if !path.is_empty() && path != "." {
                format!(
                    "{pat} in {}",
                    clip_runes(&display_jail_path(&path, workspace, extra_roots), 40)
                )
            } else {
                pat
            }
        }
        "bash" => clip_runes(&get("command"), 64),
        _ => ["path", "pattern", "command", "query", "name", "file", "url"]
            .into_iter()
            .map(get)
            .find(|value| !value.is_empty())
            .map(|value| {
                clip_runes(&display_jail_path(&value, workspace, extra_roots), 64)
            })
            .unwrap_or_default(),
    }
}

/// How a jail path should look in the TUI.
///
/// Files under the session workspace are relative (`src/app.rs`). Extra-root
/// files and anything else stay absolute so it is obvious they are not in this
/// tree. Already-relative paths are left alone.
pub fn display_jail_path(path: &str, workspace: &str, extra_roots: &[ExtraRoot]) -> String {
    let path = path.trim();
    if path.is_empty() {
        return String::new();
    }
    if !path.starts_with('/') {
        return path.to_string();
    }
    if let Some(rel) = strip_dir_prefix(path, workspace) {
        // A workspace-relative hit wins even if an extra root is a child of
        // the workspace (unusual). Extra roots that sit *beside* the
        // workspace keep their full path because this branch does not fire.
        let _ = extra_roots;
        return if rel.is_empty() {
            ".".into()
        } else {
            rel
        };
    }
    path.to_string()
}

fn strip_dir_prefix(path: &str, root: &str) -> Option<String> {
    let root = root.trim().trim_end_matches('/');
    if root.is_empty() || !root.starts_with('/') {
        return None;
    }
    if path == root {
        return Some(String::new());
    }
    let prefix = format!("{root}/");
    path.strip_prefix(&prefix).map(str::to_string)
}

fn clip_runes(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if max == 0 || s.chars().count() <= max {
        return s;
    }
    if max < 2 {
        return s.chars().take(max).collect();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn event_type(params: &Value) -> &str {
    params.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

/// Known CoT wrappers, matched case-insensitively on ASCII tags.
const THINK_TAG_PAIRS: &[(&str, &str)] = &[
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<redacted_thinking>", "</redacted_thinking>"),
    ("<thought>", "</thought>"),
    ("<reasoning>", "</reasoning>"),
    ("◁think▷", "◁/think▷"),
    ("<|thinking|>", "<|/thinking|>"),
    ("<|begin_of_thought|>", "<|end_of_thought|>"),
    ("```thinking", "```"),
    ("```think", "```"),
    ("```reasoning", "```"),
];

/// Split answer text into visible prose and hidden thinking.
///
/// Complete open/close pairs are stripped. An unclosed open tag (still
/// streaming) hides the remainder so a partial CoT cannot leak as glued
/// tokens. `unclosed` is true while a think block is still open.
pub fn extract_thinking(s: &str) -> (String, String, bool) {
    if s.is_empty() {
        return (String::new(), String::new(), false);
    }
    let mut vis = String::new();
    let mut think = String::new();
    let mut rest = s;
    let mut unclosed = false;
    while !rest.is_empty() {
        let Some((open_idx, open_len, close_tag)) = earliest_think_open(rest) else {
            vis.push_str(rest);
            break;
        };
        vis.push_str(&rest[..open_idx]);
        let mut after_open = &rest[open_idx + open_len..];
        if let Some(stripped) = after_open.strip_prefix("\r\n") {
            after_open = stripped;
        } else if let Some(stripped) = after_open.strip_prefix('\n') {
            after_open = stripped;
        }
        match index_close_tag(after_open, close_tag) {
            None => {
                push_think_chunk(&mut think, after_open);
                unclosed = true;
                break;
            }
            Some(close_idx) => {
                push_think_chunk(&mut think, &after_open[..close_idx]);
                rest = &after_open[close_idx + close_tag.len()..];
                if let Some(stripped) = rest.strip_prefix("\r\n") {
                    rest = stripped;
                } else if let Some(stripped) = rest.strip_prefix('\n') {
                    rest = stripped;
                }
                if !vis.is_empty()
                    && !rest.is_empty()
                    && !vis
                        .as_bytes()
                        .last()
                        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                    && !rest
                        .as_bytes()
                        .first()
                        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                {
                    vis.push(' ');
                }
            }
        }
    }
    (vis, think, unclosed)
}

fn push_think_chunk(think: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if !think.is_empty()
        && !think
            .as_bytes()
            .last()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        && !chunk
            .as_bytes()
            .first()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    {
        think.push(' ');
    }
    think.push_str(chunk);
}

fn earliest_think_open(s: &str) -> Option<(usize, usize, &'static str)> {
    let lower = ascii_lower(s);
    let mut best: Option<(usize, usize, &'static str)> = None;
    for (open, close) in THINK_TAG_PAIRS {
        if let Some(i) = lower.find(&ascii_lower(open))
            && best.is_none_or(|(idx, _, _)| i < idx)
        {
            best = Some((i, open.len(), close));
        }
    }
    best
}

fn index_close_tag(s: &str, close_tag: &str) -> Option<usize> {
    if close_tag == "```" {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let rel = s[i..].find("```")?;
            let j = i + rel;
            let rest = &s[j + 3..];
            let line = rest.split_once('\n').map(|(l, _)| l).unwrap_or(rest);
            if line.trim().is_empty() {
                return Some(j);
            }
            let k = rest.find("```")?;
            i = j + 3 + k + 3;
        }
        return None;
    }
    ascii_lower(s).find(&ascii_lower(close_tag))
}

/// ASCII-only fold so find indices stay valid on the original UTF-8 string.
fn ascii_lower(s: &str) -> String {
    let mut bytes = s.as_bytes().to_vec();
    for b in &mut bytes {
        if b.is_ascii_uppercase() {
            *b += 32;
        }
    }
    String::from_utf8(bytes).expect("ASCII fold keeps UTF-8")
}

/// Frozen host event types for in-session Goal progress (`graph.goal.*`).
pub const EVENT_COMPACT_START: &str = "loop.compact.start";
pub const EVENT_COMPACT: &str = "loop.compact";
pub const EVENT_GOAL_START: &str = "graph.goal.start";
pub const EVENT_GOAL_STEP: &str = "graph.goal.step";
pub const EVENT_GOAL_DONE: &str = "graph.goal.done";
pub const EVENT_GOAL_FAIL: &str = "graph.goal.fail";
pub const EVENT_GOAL_PARTIAL: &str = "graph.goal.partial";
pub const EVENT_GOAL_BLOCKED: &str = "graph.goal.blocked";

/// One extra jail root from `status` / `session`. The header chip is a
/// count; paths stay on the host side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraRoot {
    pub path: String,
    pub read_only: bool,
}

/// Goal payload from a confirmed `graph.goal.*` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalInfo {
    pub id: String,
    pub status: String,
    pub step: u64,
    pub max_steps: u64,
}

impl GoalInfo {
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed")
    }
}

/// True when `v` includes `extra_roots` or `extra_root_count`.
pub fn has_extra_roots_field(v: &Value) -> bool {
    v.get("extra_roots").is_some() || v.get("extra_root_count").is_some()
}

/// Decode extra jail roots. Prefers `extra_roots` (objects or path strings);
/// falls back to `extra_root_count` when the host only exposes a count.
pub fn decode_extra_roots(v: &Value) -> Option<Vec<ExtraRoot>> {
    if let Some(items) = v.get("extra_roots").and_then(Value::as_array) {
        let roots = items
            .iter()
            .filter_map(|item| {
                if let Some(path) = item.as_str() {
                    let path = path.trim();
                    if path.is_empty() {
                        return None;
                    }
                    return Some(ExtraRoot {
                        path: path.to_string(),
                        read_only: false,
                    });
                }
                let path = item.get("path").and_then(Value::as_str)?.trim();
                if path.is_empty() {
                    return None;
                }
                Some(ExtraRoot {
                    path: path.to_string(),
                    read_only: item
                        .get("read_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect();
        return Some(roots);
    }
    let count = v.get("extra_root_count").and_then(Value::as_u64)?;
    Some(
        (0..count)
            .map(|_| ExtraRoot {
                path: String::new(),
                read_only: false,
            })
            .collect(),
    )
}

/// Decode a `graph.goal.*` notification. Event type wins for terminal /
/// blocked so a stale payload status cannot leave a running chip.
pub fn decode_goal_event(params: &Value) -> Option<GoalInfo> {
    let kind = event_type(params);
    if !kind.starts_with("graph.goal.") {
        return None;
    }
    let goal = params.get("goal")?;
    let id = goal.get("id").and_then(Value::as_str).unwrap_or("").trim();
    if id.is_empty() {
        return None;
    }
    let payload = goal
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let status = match kind {
        EVENT_GOAL_DONE => "done",
        EVENT_GOAL_FAIL => "failed",
        EVENT_GOAL_BLOCKED => "blocked",
        EVENT_GOAL_START | EVENT_GOAL_STEP | EVENT_GOAL_PARTIAL => match payload.as_str() {
            "done" => "done",
            "fail" | "failed" => "failed",
            "blocked" => "blocked",
            _ => "running",
        },
        _ => match payload.as_str() {
            "done" => "done",
            "fail" | "failed" => "failed",
            "blocked" => "blocked",
            _ => "running",
        },
    };
    let as_u64 = |key: &str| {
        goal.get(key)
            .and_then(|n| n.as_u64().or_else(|| n.as_i64().map(|i| i.max(0) as u64)))
            .unwrap_or(0)
    };
    Some(GoalInfo {
        id: id.to_string(),
        status: status.to_string(),
        step: as_u64("step"),
        max_steps: as_u64("max_steps"),
    })
}

/// Frozen host event type for language-server findings after write/edit.
pub const EVENT_LSP_DIAGNOSTICS: &str = "harness.lsp.diagnostics";
/// Host cap on findings that ride along a tool result or LSP event.
pub const MAX_LSP_DIAGNOSTICS: usize = 10;

/// One language-server finding from `harness.lsp.diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub severity: String,
    pub message: String,
    pub line: i64,
    pub column: i64,
    pub source: String,
}

/// Newest diagnostics batch for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspProblems {
    pub path: String,
    pub count: i64,
    pub diagnostics: Vec<LspDiagnostic>,
}

fn lsp_severity_rank(severity: &str) -> i32 {
    match severity {
        "error" => 4,
        "warning" => 3,
        "information" => 2,
        _ => 1,
    }
}

/// Parse a `harness.lsp.diagnostics` payload. `count <= 0` is a no-op.
pub fn decode_lsp_diagnostics(params: &Value) -> Option<LspProblems> {
    let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
    if !kind.is_empty() && kind != EVENT_LSP_DIAGNOSTICS && !kind.ends_with("lsp.diagnostics") {
        return None;
    }
    let count = params.get("count").and_then(Value::as_i64).unwrap_or(0);
    if count <= 0 {
        return None;
    }
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut diagnostics = Vec::new();
    if let Some(items) = params.get("diagnostics").and_then(Value::as_array) {
        for item in items.iter().take(MAX_LSP_DIAGNOSTICS) {
            diagnostics.push(LspDiagnostic {
                severity: item
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                message: item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                line: item.get("line").and_then(Value::as_i64).unwrap_or(0),
                column: item.get("column").and_then(Value::as_i64).unwrap_or(0),
                source: item
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    diagnostics.sort_by_key(|b| std::cmp::Reverse(lsp_severity_rank(&b.severity)));
    Some(LspProblems {
        path,
        count,
        diagnostics,
    })
}

/// `rewind` result: `Some(last_user)` when the host dropped the last exchange.
pub fn decode_rewind(value: &Value) -> Option<String> {
    if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    Some(
        value
            .get("last_user")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, Error>>>>>;

/// Lines of child stderr retained for diagnostics (bounded: a stuck child
/// must not grow this without limit).
const STDERR_TAIL_LINES: usize = 50;

/// A spawned `mow rpc` child plus its reader thread.
pub struct Client {
    child: Child,
    /// Bounded tail of the child's stderr, for diagnosing an early exit.
    errlog: Arc<Mutex<Vec<String>>>,
    stdin: ChildStdin,
    next_id: u64,
    pending: Pending,
    notifications: Receiver<Notification>,
}

impl Client {
    /// Spawn `mow_bin rpc <engine_flags…>` with piped stdio.
    pub fn spawn(mow_bin: &str, engine_flags: &[String]) -> Result<Client, Error> {
        let mut cmd = Command::new(mow_bin);
        cmd.arg("rpc")
            .args(engine_flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Never inherit: the child shares our terminal, so anything it
            // writes to stderr is painted straight onto the screen, outside
            // the TUI frame. Capture it and keep only the tail for diagnostics.
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Error::Spawn(format!(
                    "{mow_bin} not found; set MOW_BIN or build the sibling mow repo (just build)"
                ))
            } else {
                Error::Spawn(format!("failed to start {mow_bin} rpc: {e}"))
            }
        })?;
        let stdin = child.stdin.take().ok_or(Error::Closed)?;
        let stdout = child.stdout.take().ok_or(Error::Closed)?;
        let stderr = child.stderr.take();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ntx, nrx) = channel();

        // Drain stderr so a chatty child cannot block on a full pipe, keeping
        // a bounded tail for the "child died" message.
        let errlog: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = stderr {
            let sink = Arc::clone(&errlog);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let mut buf = sink.lock().unwrap();
                    if buf.len() == STDERR_TAIL_LINES {
                        buf.remove(0);
                    }
                    buf.push(line);
                }
            });
        }

        let reader_pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                match parse_message(&line) {
                    Some(Message::Response { id, result }) => {
                        let tx = reader_pending.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            let out = match result {
                                Ok(v) => Ok(v),
                                Err((code, message)) => Err(Error::Rpc { code, message }),
                            };
                            let _ = tx.send(out);
                        }
                    }
                    Some(Message::Notify(n)) => {
                        if ntx.send(n).is_err() {
                            break;
                        }
                    }
                    None => {}
                }
            }
            // Child stdout closed: fail every waiter instead of hanging.
            for (_, tx) in reader_pending.lock().unwrap().drain() {
                let _ = tx.send(Err(Error::Closed));
            }
        });

        Ok(Client {
            child,
            stdin,
            next_id: 0,
            pending,
            notifications: nrx,
            errlog,
        })
    }

    /// Tail of the child's stderr. Empty in normal operation — `mow rpc`
    /// keeps stderr quiet precisely so a TUI can own the terminal — so a
    /// non-empty tail is a real diagnostic (bad flag, panic, missing config).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.errlog.lock().unwrap().clone()
    }

    /// Notification stream (events, `perm.ask`).
    pub fn notifications(&self) -> &Receiver<Notification> {
        &self.notifications
    }

    /// Send a request; the reply arrives on the returned channel.
    pub fn send(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.next_id += 1;
        let id = self.next_id;
        let mut req = json!({"jsonrpc":"2.0","id":id,"method":method});
        if let Some(p) = params {
            req["params"] = p;
        }
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_string(&req).map_err(|e| Error::Protocol(e.to_string()))?;
        line.push('\n');
        if let Err(e) = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.flush())
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(Error::Io(e));
        }
        Ok(rx)
    }

    /// Send and block for the reply.
    pub fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let rx = self.send(method, params)?;
        match rx.recv_timeout(timeout) {
            Ok(res) => res,
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    /// version → session → status. Requires compatibility epoch 1.
    ///
    /// When `version` omits `methods`, try `capabilities` once. An empty
    /// list after that means the host did not advertise a surface — do not
    /// infer a stock build.
    pub fn handshake(&mut self, timeout: Duration) -> Result<(VersionInfo, SessionInfo), Error> {
        let v = self.call("version", None, timeout)?;
        let mut version = check_version(&v)?;
        if version.methods.is_empty()
            && let Ok(caps) = self.call("capabilities", None, timeout)
        {
            version = merge_capabilities(version, &caps);
        }
        let s = self.call("session", None, timeout)?;
        let session = SessionInfo::from_value(&s);
        let _ = self.call("status", None, timeout)?;
        Ok((version, session))
    }

    pub fn ping(&mut self, timeout: Duration) -> Result<Value, Error> {
        self.call("ping", None, timeout)
    }

    /// Return the resumable sessions known to the host.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn sessions(&mut self, timeout: Duration) -> Result<Vec<SessionSummary>, Error> {
        let value = self.call("sessions", None, timeout)?;
        decode_sessions(&value)
    }

    /// Return the stored transcript.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn transcript(&mut self, timeout: Duration) -> Result<Vec<TranscriptMessage>, Error> {
        let value = self.call("transcript", None, timeout)?;
        decode_transcript(&value)
    }

    /// Redirect the active turn.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn steer(&mut self, text: &str, timeout: Duration) -> Result<Value, Error> {
        let rx = self.request_steer(text)?;
        match rx.recv_timeout(timeout) {
            Ok(res) => res,
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    /// Non-blocking `steer` — the 50ms loop polls the reply instead of stalling.
    pub fn request_steer(&mut self, text: &str) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("steer", Some(steer_params(text)?))
    }

    /// List slash commands registered by the host.
    pub fn slash_list(&mut self, timeout: Duration) -> Result<Vec<SlashCommand>, Error> {
        let value = self.call("slash.list", None, timeout)?;
        let rows = value
            .get("commands")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Protocol("slash.list result has no commands array".into()))?;
        rows.iter()
            .map(|row| {
                let aliases = row
                    .get("aliases")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(SlashCommand {
                    name: string_field(row, "name")?,
                    summary: string_field(row, "summary")?,
                    exclusive: row
                        .get("exclusive")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    aliases,
                })
            })
            .collect()
    }

    /// Run a host slash command.
    pub fn slash(
        &mut self,
        name: &str,
        args: &[String],
        color: bool,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send(
            "slash",
            Some(json!({ "name": name, "args": args, "color": color })),
        )
    }

    /// Fetch `extensions.<name>` when the host advertises `extension.config`.
    pub fn extension_config(&mut self, name: &str, timeout: Duration) -> Result<Value, Error> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Protocol("extension name must not be empty".into()));
        }
        self.call("extension.config", Some(json!({ "name": name })), timeout)
    }

    /// Set the host permission mode.
    pub fn perm_set(&mut self, mode: &str, timeout: Duration) -> Result<Value, Error> {
        if !matches!(mode, "ask" | "auto") {
            return Err(Error::Protocol(format!("invalid permission mode: {mode}")));
        }
        self.call("perm.set", Some(json!({ "mode": mode })), timeout)
    }

    /// Resolve a pending permission request.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn perm_decide(
        &mut self,
        id: &str,
        decision: &str,
        timeout: Duration,
    ) -> Result<Value, Error> {
        if !matches!(decision, "allow" | "deny" | "always") {
            return Err(Error::Protocol(format!(
                "invalid permission decision: {decision}"
            )));
        }
        self.call(
            "perm.decide",
            Some(json!({ "id": id, "decision": decision })),
            timeout,
        )
    }

    /// Return the current status object.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn status(&mut self, timeout: Duration) -> Result<Value, Error> {
        self.call("status", None, timeout)
    }

    /// List models the host can switch to. Control method: answered while busy.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn model_list(&mut self, timeout: Duration) -> Result<ModelList, Error> {
        let value = self.call("model.list", None, timeout)?;
        decode_model_list(&value)
    }

    /// Switch the session model. Control method: answered while busy.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn model_set(&mut self, id: &str, timeout: Duration) -> Result<String, Error> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Protocol("model id must not be empty".into()));
        }
        let value = self.call("model.set", Some(json!({ "id": id })), timeout)?;
        Ok(value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .unwrap_or(id)
            .to_string())
    }

    /// List reasoning efforts the host can switch to.
    pub fn effort_list(&mut self, timeout: Duration) -> Result<EffortList, Error> {
        let value = self.call("effort.list", None, timeout)?;
        decode_effort_list(&value)
    }

    /// Switch the session effort. Control method: answered while busy.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn effort_set(&mut self, id: &str, timeout: Duration) -> Result<String, Error> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Protocol("effort id must not be empty".into()));
        }
        let value = self.call("effort.set", Some(json!({ "id": id })), timeout)?;
        Ok(value
            .get("effort")
            .and_then(Value::as_str)
            .filter(|effort| !effort.is_empty())
            .unwrap_or(id)
            .to_string())
    }

    /// Context-window usage for the header chip. Control method: answered while busy.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn context(&mut self, timeout: Duration) -> Result<ContextUsage, Error> {
        let value = self.call("context", None, timeout)?;
        Ok(ContextUsage::from_value(&value))
    }

    /// Non-blocking `context` — the 50ms loop polls the reply instead of stalling.
    pub fn request_context(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("context", None)
    }

    /// Decode a compact response received through the non-blocking request path.
    pub fn decode_compact(value: &Value) -> CompactReport {
        CompactReport {
            layer: value
                .get("layer")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            chars_saved: value
                .get("chars_saved")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            chars_before: value
                .get("chars_before")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            chars_after: value
                .get("chars_after")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            messages_before: value
                .get("messages_before")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            messages_after: value
                .get("messages_after")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            over_budget: value
                .get("over_budget")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tokens: value.get("tokens").and_then(Value::as_u64).unwrap_or(0),
        }
    }

    /// Compact the engine transcript. `max_chars <= 0` lets the engine choose.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn compact(&mut self, max_chars: i64, timeout: Duration) -> Result<CompactReport, Error> {
        let params = if max_chars > 0 {
            Some(json!({ "max_chars": max_chars }))
        } else {
            None
        };
        let value = self.call("compact", params, timeout)?;
        Ok(Self::decode_compact(&value))
    }

    /// Drop the last exchange; returns the user text so the UI can refill the
    /// input box for an edit-and-resend.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn rewind(&mut self, timeout: Duration) -> Result<Option<String>, Error> {
        let value = self.call("rewind", None, timeout)?;
        Ok(decode_rewind(&value))
    }

    /// Non-blocking `rewind` — the 50ms loop polls the reply instead of stalling.
    pub fn request_rewind(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("rewind", None)
    }

    /// Non-blocking `transcript` refresh after rewind/compact.
    pub fn request_transcript(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("transcript", None)
    }

    /// Non-blocking `sessions` list for the picker overlay.
    pub fn request_sessions(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("sessions", None)
    }

    /// Non-blocking `status` for `/status`.
    pub fn request_status(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("status", None)
    }

    /// Non-blocking `skill.list`.
    pub fn request_skill_list(&mut self) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("skill.list", None)
    }

    /// Non-blocking `skill.activate`.
    pub fn request_skill_activate(
        &mut self,
        names: &[String],
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        if names.is_empty() {
            return Err(Error::Protocol(
                "skill.activate needs at least one name".into(),
            ));
        }
        self.send("skill.activate", Some(json!({ "names": names })))
    }

    /// Non-blocking `model.set`.
    pub fn request_model_set(&mut self, id: &str) -> Result<Receiver<Result<Value, Error>>, Error> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Protocol("model id must not be empty".into()));
        }
        self.send("model.set", Some(json!({ "id": id })))
    }

    /// Non-blocking `effort.set`.
    pub fn request_effort_set(
        &mut self,
        id: &str,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Protocol("effort id must not be empty".into()));
        }
        self.send("effort.set", Some(json!({ "id": id })))
    }

    /// Non-blocking `perm.set`.
    pub fn request_perm_set(
        &mut self,
        mode: &str,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        if !matches!(mode, "ask" | "auto") {
            return Err(Error::Protocol(format!("invalid permission mode: {mode}")));
        }
        self.send("perm.set", Some(json!({ "mode": mode })))
    }

    /// Non-blocking `perm.decide`.
    pub fn request_perm_decide(
        &mut self,
        id: &str,
        decision: &str,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        if !matches!(decision, "allow" | "deny" | "always") {
            return Err(Error::Protocol(format!(
                "invalid permission decision: {decision}"
            )));
        }
        self.send(
            "perm.decide",
            Some(json!({ "id": id, "decision": decision })),
        )
    }

    /// Skills available in this workspace.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn skill_list(&mut self, timeout: Duration) -> Result<Vec<String>, Error> {
        let value = self.call("skill.list", None, timeout)?;
        Ok(decode_skill_list(&value))
    }

    /// Activate skills by name; returns `(activated, unknown)`.
    #[allow(dead_code)] // synchronous API retained for non-TUI callers
    pub fn skill_activate(
        &mut self,
        names: &[String],
        timeout: Duration,
    ) -> Result<(Vec<String>, Vec<String>), Error> {
        if names.is_empty() {
            return Err(Error::Protocol(
                "skill.activate needs at least one name".into(),
            ));
        }
        let value = self.call("skill.activate", Some(json!({ "names": names })), timeout)?;
        Ok(decode_skill_activate(&value))
    }

    /// Start a turn. The result arrives later (the channel stays open while
    /// `event` notifications stream).
    pub fn prompt(&mut self, text: &str) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.prompt_with(text, false)
    }

    /// Start a prompt that is answered against current context but is not
    /// persisted into session history (`/btw`).
    pub fn prompt_with(
        &mut self,
        text: &str,
        ephemeral: bool,
    ) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send(
            "prompt",
            Some(json!({ "text": text, "ephemeral": ephemeral })),
        )
    }

    /// Abort the running turn (control method: answered while busy).
    pub fn cancel(&mut self) -> Result<(), Error> {
        self.send("cancel", None).map(|_| ())
    }

    /// Terminate the child; called on quit.
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn steer_params(text: &str) -> Result<Value, Error> {
    if text.trim().is_empty() {
        return Err(Error::Protocol("steer text must not be empty".into()));
    }
    Ok(json!({ "text": text }))
}

fn string_field(value: &Value, field: &str) -> Result<String, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| Error::Protocol(format!("missing string field {field:?}")))
}

pub fn decode_model_list(value: &Value) -> Result<ModelList, Error> {
    let rows = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("model.list result has no models array".into()))?;
    let models: Vec<ModelInfo> = rows
        .iter()
        .map(|row| {
            Ok::<_, Error>(ModelInfo {
                id: string_field(row, "id")?,
                current: row.get("current").and_then(Value::as_bool).unwrap_or(false),
                wire: row
                    .get("wire")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect::<Result<Vec<ModelInfo>, Error>>()?;
    let current = value
        .get("current")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            models
                .iter()
                .find(|model| model.current)
                .map(|model| model.id.clone())
        })
        .unwrap_or_default();
    Ok(ModelList { models, current })
}

pub fn decode_effort_list(value: &Value) -> Result<EffortList, Error> {
    let rows = value
        .get("efforts")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("effort.list result has no efforts array".into()))?;
    let efforts: Vec<EffortInfo> = rows
        .iter()
        .map(|row| {
            Ok::<_, Error>(EffortInfo {
                id: string_field(row, "id")?,
                current: row.get("current").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect::<Result<Vec<EffortInfo>, Error>>()?;
    let current = value
        .get("current")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            efforts
                .iter()
                .find(|effort| effort.current)
                .map(|effort| effort.id.clone())
        })
        .unwrap_or_default();
    let default = value
        .get("default")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(EffortList {
        efforts,
        current,
        default,
    })
}

pub fn decode_sessions(value: &Value) -> Result<Vec<SessionSummary>, Error> {
    let rows = value
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("sessions result has no sessions array".into()))?;
    rows.iter()
        .map(|row| {
            Ok(SessionSummary {
                id: string_field(row, "id")?,
                updated: string_field(row, "updated")?,
                preview: string_field(row, "preview")?,
            })
        })
        .collect()
}

pub fn decode_skill_list(value: &Value) -> Vec<String> {
    value
        .get("skills")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn decode_skill_activate(value: &Value) -> (Vec<String>, Vec<String>) {
    let pick = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (pick("activated"), pick("unknown"))
}

pub fn decode_transcript(value: &Value) -> Result<Vec<TranscriptMessage>, Error> {
    let rows = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("transcript result has no messages array".into()))?;
    rows.iter()
        .map(|row| {
            Ok(TranscriptMessage {
                role: string_field(row, "role")?,
                content: string_field(row, "content")?,
                timestamp: row
                    .get("ts")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            })
        })
        .collect()
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_response_result() {
        let m = parse_message(r#"{"jsonrpc":"2.0","id":1,"result":{"rpc":"1"}}"#).unwrap();
        match m {
            Message::Response { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result.unwrap()["rpc"], "1");
            }
            other => panic!("want response, got {other:?}"),
        }
    }

    #[test]
    fn parses_error_envelope() {
        let m =
            parse_message(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}"#)
                .unwrap();
        match m {
            Message::Response { id, result } => {
                assert_eq!(id, 7);
                let (code, msg) = result.unwrap_err();
                assert_eq!(code, -32601);
                assert_eq!(msg, "nope");
            }
            other => panic!("want response, got {other:?}"),
        }
    }

    #[test]
    fn optional_linked_features_join_capability_flags() {
        let flags = decode_features(&serde_json::json!({
            "features": {"streaming_events": true},
            "optional": {"features": [
                {"id": "goal", "linked": true, "events": ["graph.goal.start"]},
                {"id": "lsp", "linked": false}
            ]}
        }));
        assert_eq!(flags.get("streaming_events"), Some(&true));
        assert_eq!(flags.get("goal"), Some(&true));
        assert_eq!(flags.get("lsp"), None);
    }

    #[test]
    fn parses_notification() {
        let line =
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"loop.token","delta":"hi"}}"#;
        match parse_message(line).unwrap() {
            Message::Notify(n) => {
                assert_eq!(n.method, "event");
                assert_eq!(token_delta(&n.params), Some("hi"));
            }
            other => panic!("want notification, got {other:?}"),
        }
    }

    #[test]
    fn skips_blank_and_garbage_lines() {
        assert!(parse_message("").is_none());
        assert!(parse_message("   ").is_none());
        assert!(parse_message("engine log line, not json").is_none());
    }

    #[test]
    fn child_stderr_is_captured_not_inherited() {
        // Inheriting stderr lets the child paint over the TUI frame: that is
        // exactly how "→ bash cd …" ended up on screen. Assert the spawn
        // policy at the source level, since Stdio has no getter.
        // Build the needle at runtime: a literal would appear in this file
        // and match itself.
        let src = include_str!("rpc.rs");
        let bad = format!(".stderr(Stdio::{}", "inherit");
        assert!(
            !src.contains(&bad),
            "child stderr must be piped, never inherited"
        );
        assert!(src.contains(".stderr(Stdio::piped())"));
    }

    #[test]
    fn handshake_requires_compatibility_epoch_1() {
        let ok = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"1"});
        let info = check_version(&ok).unwrap();
        assert_eq!(info.rpc, "1");
        // No capability list (older server): do not infer a stock method set.
        assert!(info.methods.is_empty());
        assert!(info.features.is_empty());

        let modern = serde_json::json!({
            "name":"mow","version":"0.1.0","rpc":"1",
            "methods":["prompt","context","compact"],
            "control_methods":["context"],
            "features":{"ephemeral_prompt":true,"batch":false},
        });
        let info = check_version(&modern).unwrap();
        assert_eq!(info.methods, vec!["prompt", "context", "compact"]);
        assert_eq!(info.control_methods, vec!["context".to_string()]);
        assert_eq!(info.features.get("ephemeral_prompt"), Some(&true));
        assert_eq!(info.features.get("batch"), Some(&false));

        // Epoch is an exact contract: pre-release numbers and future epochs
        // are both refused (they are not additive floors).
        for bad in ["2", "3", "4", "0", "99"] {
            let incompatible = serde_json::json!({"name":"mow","version":"0.1.0","rpc": bad});
            assert!(
                check_version(&incompatible).is_err(),
                "epoch {bad} must be refused"
            );
        }

        let old = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"2"});
        let err = check_version(&old).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
        assert!(err.to_string().contains("\"2\""), "{err}");
        assert!(
            err.to_string().contains("compatibility epoch"),
            "error should name the epoch gate: {err}"
        );

        let missing = serde_json::json!({"name":"mow"});
        assert!(matches!(check_version(&missing), Err(Error::Protocol(_))));
    }

    #[test]
    fn merge_capabilities_fills_an_empty_version_surface() {
        let version = check_version(&serde_json::json!({
            "name": "mow",
            "version": "0.1.0",
            "rpc": "1"
        }))
        .unwrap();
        assert!(version.methods.is_empty());
        let merged = merge_capabilities(
            version,
            &serde_json::json!({
                "methods": ["prompt", "slash.list"],
                "control_methods": ["status"],
                "features": {"ephemeral_prompt": true}
            }),
        );
        assert_eq!(merged.methods, vec!["prompt", "slash.list"]);
        assert_eq!(merged.control_methods, vec!["status".to_string()]);
        assert_eq!(merged.features.get("ephemeral_prompt"), Some(&true));
    }

    #[test]
    fn delegate_chunks_are_not_host_tokens() {
        let peer =
            serde_json::json!({"type":"harness.delegate.chunk","agent":"peer-agent","delta":"x"});
        assert_eq!(token_delta(&peer), None);

        let tool = serde_json::json!({"type":"tool.start","name":"grep"});
        assert_eq!(token_delta(&tool), None);
    }

    #[test]
    fn reasoning_deltas_are_not_host_tokens() {
        let reason = serde_json::json!({"type":"loop.reasoning","delta":"secret plan"});
        assert_eq!(token_delta(&reason), None);
        assert_eq!(reasoning_delta(&reason), Some("secret plan"));
        assert_eq!(
            reasoning_delta(&serde_json::json!({"type":"loop.token","delta":"hi"})),
            None
        );
    }

    #[test]
    fn harness_tool_events_match_mow_event_shape() {
        // Frozen fields from mow `internal/engine/event.go` Event.
        let start = serde_json::json!({
            "type": "harness.tool.start",
            "run_id": "run-1",
            "tool": "write",
            "tool_call_id": "call-1",
            "args": {"path": "src/app.rs", "content": "fn main() {}"}
        });
        assert_eq!(tool_name(&start), Some("write"));
        assert_eq!(
            tool_progress_label("write", tool_args(&start)),
            "write src/app.rs"
        );
        assert_eq!(
            tool_progress_label("read src/app.rs", tool_args(&start)),
            "read src/app.rs"
        );
        assert_eq!(
            tool_progress_label(
                "bash",
                Some(&serde_json::json!({"command": "cargo test --all"}))
            ),
            "bash cargo test --all"
        );
        assert_eq!(
            tool_progress_label(
                "grep",
                Some(&serde_json::json!({"pattern": "live_tools", "path": "src"}))
            ),
            "grep live_tools in src"
        );
        assert_eq!(
            tool_progress_label("delete", Some(&serde_json::json!({"path": "tmp/out"}))),
            "delete tmp/out"
        );

        let end = serde_json::json!({
            "type": "harness.tool.end",
            "tool": "write",
            "args": {"path": "src/app.rs"},
            "result": "edited src/app.rs\n--- src/app.rs\n+++ src/app.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "denied": false,
            "error": "",
            "duration_ms": 12
        });
        assert_eq!(tool_name(&end), Some("write"));
        assert!(tool_result(&end).unwrap().contains("@@ -1 +1 @@"));
        assert!(!tool_denied(&end));
        assert_eq!(tool_error(&end), None);

        let denied = serde_json::json!({
            "type": "harness.tool.end",
            "tool": "bash",
            "args": {"command": "rm -rf /"},
            "denied": true,
            "error": "policy: write disabled",
            "duration_ms": 4
        });
        assert!(tool_denied(&denied));
        assert_eq!(tool_error(&denied), Some("policy: write disabled"));
    }

    #[test]
    fn workspace_files_display_relative_extra_roots_stay_absolute() {
        let ws = "/home/subosito/Code/runner/mowi";
        let extra = [ExtraRoot {
            path: "/home/subosito/Code/runner/mow".into(),
            read_only: false,
        }];
        assert_eq!(
            display_jail_path(&format!("{ws}/src/app.rs"), ws, &extra),
            "src/app.rs"
        );
        assert_eq!(
            display_jail_path(
                "/home/subosito/Code/runner/mow/internal/engine/event.go",
                ws,
                &extra
            ),
            "/home/subosito/Code/runner/mow/internal/engine/event.go"
        );
        assert_eq!(
            tool_progress_label_for(
                "write",
                Some(&serde_json::json!({"path": format!("{ws}/src/theme.rs")})),
                ws,
                &extra
            ),
            "write src/theme.rs"
        );
        assert_eq!(
            tool_progress_label_for(
                "write /home/subosito/Code/runner/mowi/src/app.rs",
                None,
                ws,
                &extra
            ),
            "write src/app.rs"
        );
        assert_eq!(
            display_jail_path("src/already.rs", ws, &extra),
            "src/already.rs"
        );
        assert_eq!(display_jail_path(ws, ws, &extra), ".");
    }

    #[test]
    fn extract_thinking_strips_closed_and_hides_unclosed() {
        let (vis, think, unclosed) = extract_thinking("<think>secret plan</think>## Hello");
        assert_eq!(vis, "## Hello");
        assert!(think.contains("secret plan"), "{think}");
        assert!(!unclosed);

        let (vis, think, unclosed) = extract_thinking("<think>Let me reason without spaces");
        assert!(vis.is_empty(), "{vis}");
        assert!(think.contains("Let me reason"), "{think}");
        assert!(unclosed);

        let (vis, _, unclosed) =
            extract_thinking("key files.<think>plan the approach</think>Let me go");
        assert_eq!(vis, "key files. Let me go");
        assert!(!unclosed);
        assert!(!vis.contains("plan the approach"), "{vis}");

        let (vis, _, _) = extract_thinking("<THINK>SECRET</THINK>ok");
        assert_eq!(vis, "ok");
        assert!(!vis.to_lowercase().contains("secret"));

        let (_, think, _) =
            extract_thinking("<think>each other.</think>visible<think>I'll finish</think>");
        assert!(think.contains("each other. I'll finish"), "{think}");
        assert!(!think.contains("other.I'll"), "{think}");
    }

    #[test]
    fn decode_lsp_diagnostics_matches_frozen_shape() {
        let event = serde_json::json!({
            "type": "harness.lsp.diagnostics",
            "tool": "edit",
            "path": "internal/x.go",
            "count": 3,
            "diagnostics": [
                {"severity": "warning", "message": "unused", "line": 8, "column": 1},
                {"severity": "error", "message": "undefined: foo", "line": 42, "column": 9, "source": "compiler"},
            ]
        });
        let problems = decode_lsp_diagnostics(&event).expect("shape");
        assert_eq!(problems.path, "internal/x.go");
        assert_eq!(problems.count, 3);
        assert_eq!(problems.diagnostics[0].severity, "error");
        assert_eq!(problems.diagnostics[0].source, "compiler");
        assert_eq!(problems.diagnostics.len(), 2);

        assert!(
            decode_lsp_diagnostics(&serde_json::json!({
                "type": "harness.lsp.diagnostics",
                "path": "clean.go",
                "count": 0
            }))
            .is_none()
        );
        assert!(
            decode_lsp_diagnostics(&serde_json::json!({
                "type": "loop.token",
                "count": 2
            }))
            .is_none()
        );
    }

    #[test]
    fn decode_skill_list_and_activate() {
        assert_eq!(
            decode_skill_list(&serde_json::json!({"skills": ["review", "docs"]})),
            vec!["review".to_string(), "docs".to_string()]
        );
        assert!(decode_skill_list(&serde_json::json!({})).is_empty());
        let (activated, unknown) = decode_skill_activate(&serde_json::json!({
            "activated": ["review"],
            "unknown": ["nope"]
        }));
        assert_eq!(activated, vec!["review".to_string()]);
        assert_eq!(unknown, vec!["nope".to_string()]);
    }

    #[test]
    fn decode_rewind_needs_ok() {
        assert_eq!(
            decode_rewind(&serde_json::json!({"ok": true, "last_user": "hi"})).as_deref(),
            Some("hi")
        );
        assert_eq!(decode_rewind(&serde_json::json!({"ok": false})), None);
        assert_eq!(decode_rewind(&serde_json::json!({})), None);
    }

    #[test]
    fn session_short_id() {
        let s = SessionInfo::from_value(&serde_json::json!({
            "session_id":"0123456789abcdef","workspace":"/w","model":"gpt-5-mini"
        }));
        assert_eq!(s.short_id(), "01234567");
        assert_eq!(s.model, "gpt-5-mini");
        assert!(s.extra_roots.is_empty());
    }

    #[test]
    fn session_ignores_rpc_git_and_decodes_extra_roots() {
        let s = SessionInfo::from_value(&serde_json::json!({
            "session_id": "s1",
            "workspace": "/w",
            "model": "gpt-5-mini",
            "git": { "branch": "main", "dirty": true },
            "extra_roots": [
                { "path": "/opt/shared", "read_only": true },
                "/data"
            ]
        }));
        assert_eq!(
            s.extra_roots,
            vec![
                ExtraRoot {
                    path: "/opt/shared".into(),
                    read_only: true
                },
                ExtraRoot {
                    path: "/data".into(),
                    read_only: false
                },
            ]
        );
    }

    #[test]
    fn decode_extra_roots_accepts_count_only() {
        assert_eq!(
            decode_extra_roots(&serde_json::json!({"extra_root_count": 2}))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            decode_extra_roots(&serde_json::json!({"extra_roots": []}))
                .unwrap()
                .len(),
            0
        );
        assert!(decode_extra_roots(&serde_json::json!({"busy": true})).is_none());
        assert!(!has_extra_roots_field(&serde_json::json!({"busy": true})));
    }

    #[test]
    fn decode_goal_event_uses_confirmed_graph_types() {
        let step = decode_goal_event(&serde_json::json!({
            "type": "graph.goal.step",
            "goal": { "id": "fix-bugs", "status": "running", "step": 2, "max_steps": 10 }
        }))
        .unwrap();
        assert_eq!(step.id, "fix-bugs");
        assert_eq!(step.status, "running");
        assert_eq!(step.step, 2);
        assert_eq!(step.max_steps, 10);

        let done = decode_goal_event(&serde_json::json!({
            "type": "graph.goal.done",
            "goal": { "id": "fix-bugs", "status": "running", "step": 10, "max_steps": 10 }
        }))
        .unwrap();
        assert_eq!(done.status, "done");
        assert!(done.is_terminal());

        let blocked = decode_goal_event(&serde_json::json!({
            "type": "graph.goal.blocked",
            "goal": { "id": "fix-bugs" }
        }))
        .unwrap();
        assert_eq!(blocked.status, "blocked");

        assert!(
            decode_goal_event(&serde_json::json!({
                "type": "loop.token",
                "goal": { "id": "nope" }
            }))
            .is_none()
        );
        assert!(
            decode_goal_event(&serde_json::json!({
                "type": "graph.goal.step",
                "goal": { "id": "" }
            }))
            .is_none()
        );
    }

    #[test]
    fn parses_permission_notification() {
        let notification = Notification {
            method: "perm.ask".into(),
            params: serde_json::json!({
                "id": "perm-1",
                "name": "write",
                "args": {"path": "notes.txt"},
                "tool_call_id": "call-1"
            }),
        };
        let permission = notification.permission_request().unwrap();
        assert_eq!(permission.id, "perm-1");
        assert_eq!(permission.name, "write");
        assert_eq!(permission.args["path"], "notes.txt");
        assert_eq!(permission.tool_call_id, "call-1");
    }

    #[test]
    fn parses_sessions_and_transcript_shapes() {
        let sessions = decode_sessions(&serde_json::json!({
            "sessions": [{"id":"s1","updated":"today","preview":"hello"}]
        }))
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
        assert_eq!(sessions[0].preview, "hello");

        let turns = decode_transcript(&serde_json::json!({
            "messages": [{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}]
        }))
        .unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].content, "hello");
    }

    #[test]
    fn parses_model_list_shape() {
        let list = decode_model_list(&serde_json::json!({
            "models": [
                {"id": "gpt-5-mini", "current": true, "wire": "openai-responses"},
                {"id": "claude-sonnet-4", "current": false}
            ],
            "current": "gpt-5-mini"
        }))
        .unwrap();
        assert_eq!(list.current, "gpt-5-mini");
        assert_eq!(list.models.len(), 2);
        assert!(list.models[0].current);
        assert_eq!(list.models[0].wire, "openai-responses");
        assert_eq!(list.models[1].id, "claude-sonnet-4");
        assert!(list.models[1].wire.is_empty());
    }

    #[test]
    fn parses_effort_list_shape() {
        let list = decode_effort_list(&serde_json::json!({
            "efforts": [
                {"id": "none", "current": false},
                {"id": "high", "current": true}
            ],
            "current": "high",
            "default": "none"
        }))
        .unwrap();
        assert_eq!(list.current, "high");
        assert_eq!(list.default, "none");
        assert_eq!(list.efforts.len(), 2);
        assert!(list.efforts[1].current);
    }

    #[test]
    fn steer_params_reject_empty_text() {
        let err = steer_params("   ").unwrap_err();
        assert!(
            err.to_string().contains("steer text must not be empty"),
            "{err}"
        );
        assert_eq!(
            steer_params("focus on tests").unwrap()["text"],
            "focus on tests"
        );
    }

    #[test]
    fn extension_config_fixture_decodes_as_mowi_section() {
        // Same payload the PTY mock returns for params {name:"mowi"}.
        let value = serde_json::json!({
            "permission_mode": "ask",
            "theme": "catppuccin-mocha",
            "welcome": true,
            "welcome_message": "fixture splash",
            "prompt": "❯"
        });
        let cfg = crate::config::decode_mowi_config(&value);
        assert_eq!(
            cfg.permission_mode,
            Some(crate::config::PermissionMode::Ask)
        );
        assert_eq!(cfg.theme, Some(crate::theme::ThemeName::CatppuccinMocha));
        assert_eq!(cfg.welcome, Some(true));
        assert_eq!(cfg.welcome_message.as_deref(), Some("fixture splash"));
        assert_eq!(cfg.prompt.as_deref(), Some("❯"));
    }

    #[test]
    fn context_usage_from_value() {
        let usage = ContextUsage::from_value(&serde_json::json!({
            "tokens": 12300,
            "context_window": 200000,
            "remaining": 187700,
            "percent": 6.15
        }));
        assert_eq!(usage.tokens, 12_300);
        assert_eq!(usage.context_window, Some(200_000));
        assert_eq!(usage.remaining, Some(187_700));
        assert_eq!(usage.percent, Some(6.15));
    }
}
