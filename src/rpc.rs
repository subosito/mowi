//! JSON-lines client for `mow rpc` (host protocol v3).
//!
//! The Engine is a child process: mowi writes requests to its stdin and reads
//! responses plus notifications from its stdout. Stderr is Engine logging and
//! is never parsed.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// Protocol version this client speaks.
pub const RPC_MIN_VERSION: u32 = 3;

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

/// `context` result — drives the context gauge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub context_window: Option<u64>,
    pub remaining: Option<u64>,
    pub percent: Option<f64>,
}

/// `compact` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactReport {
    pub layer: String,
    pub chars_saved: i64,
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

/// `version` result.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
    pub rpc: String,
    /// Methods this server advertises (empty when it predates `capabilities`).
    pub methods: Vec<String>,
    /// Subset answered while a prompt is in flight.
    pub control_methods: Vec<String>,
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

/// Validate a `version` result. The protocol is additive: a server newer than
/// `RPC_MIN_VERSION` still speaks every method we send, so accept `>=` rather
/// than pinning equality (which made each new method a breaking change).
pub fn check_version(v: &Value) -> Result<VersionInfo, Error> {
    let rpc = v
        .get("rpc")
        .and_then(|r| r.as_str())
        .ok_or_else(|| Error::Protocol("mow rpc: version result has no \"rpc\" field".into()))?;
    let n: u32 = rpc
        .trim()
        .parse()
        .map_err(|_| Error::Protocol(format!("mow rpc: unrecognized protocol version {rpc:?}")))?;
    if n < RPC_MIN_VERSION {
        return Err(Error::Protocol(format!(
            "mow rpc protocol {rpc:?}, need >= {RPC_MIN_VERSION}: rebuild mow with a current ext/rpc"
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
    })
}

/// `session` result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub wire: String,
}

impl SessionInfo {
    pub fn from_value(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        SessionInfo {
            session_id: s("session_id"),
            workspace: s("workspace"),
            model: s("model"),
            wire: s("wire"),
        }
    }

    /// Short id for the header chip.
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
/// the host answer.
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

fn event_type(params: &Value) -> &str {
    params.get("type").and_then(|t| t.as_str()).unwrap_or("")
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

    /// version → session → status. Refuses a non-v3 server.
    pub fn handshake(&mut self, timeout: Duration) -> Result<(VersionInfo, SessionInfo), Error> {
        let v = self.call("version", None, timeout)?;
        let version = check_version(&v)?;
        let s = self.call("session", None, timeout)?;
        let session = SessionInfo::from_value(&s);
        let _ = self.call("status", None, timeout)?;
        Ok((version, session))
    }

    pub fn ping(&mut self, timeout: Duration) -> Result<Value, Error> {
        self.call("ping", None, timeout)
    }

    /// Return the resumable sessions known to the host.
    pub fn sessions(&mut self, timeout: Duration) -> Result<Vec<SessionSummary>, Error> {
        let value = self.call("sessions", None, timeout)?;
        decode_sessions(&value)
    }

    /// Return the stored transcript.
    pub fn transcript(&mut self, timeout: Duration) -> Result<Vec<TranscriptMessage>, Error> {
        let value = self.call("transcript", None, timeout)?;
        decode_transcript(&value)
    }

    /// Redirect the active turn.
    pub fn steer(&mut self, text: &str, timeout: Duration) -> Result<Value, Error> {
        if text.trim().is_empty() {
            return Err(Error::Protocol("steer text must not be empty".into()));
        }
        self.call("steer", Some(json!({ "text": text })), timeout)
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

    /// Set the host permission mode.
    pub fn perm_set(&mut self, mode: &str, timeout: Duration) -> Result<Value, Error> {
        if !matches!(mode, "ask" | "auto") {
            return Err(Error::Protocol(format!("invalid permission mode: {mode}")));
        }
        self.call("perm.set", Some(json!({ "mode": mode })), timeout)
    }

    /// Resolve a pending permission request.
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
    pub fn status(&mut self, timeout: Duration) -> Result<Value, Error> {
        self.call("status", None, timeout)
    }

    /// List models the host can switch to. Control method: answered while busy.
    pub fn model_list(&mut self, timeout: Duration) -> Result<ModelList, Error> {
        let value = self.call("model.list", None, timeout)?;
        decode_model_list(&value)
    }

    /// Switch the session model. Control method: answered while busy.
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

    /// Context-window usage for the gauge. Control method: answered while busy.
    pub fn context(&mut self, timeout: Duration) -> Result<ContextUsage, Error> {
        let value = self.call("context", None, timeout)?;
        Ok(ContextUsage {
            tokens: value.get("tokens").and_then(Value::as_u64).unwrap_or(0),
            context_window: value.get("context_window").and_then(Value::as_u64),
            remaining: value.get("remaining").and_then(Value::as_u64),
            percent: value.get("percent").and_then(Value::as_f64),
        })
    }

    /// Compact the engine transcript. `max_chars <= 0` lets the engine choose.
    pub fn compact(&mut self, max_chars: i64, timeout: Duration) -> Result<CompactReport, Error> {
        let params = if max_chars > 0 {
            Some(json!({ "max_chars": max_chars }))
        } else {
            None
        };
        let value = self.call("compact", params, timeout)?;
        Ok(CompactReport {
            layer: value
                .get("layer")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            chars_saved: value
                .get("chars_saved")
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
        })
    }

    /// Drop the last exchange; returns the user text so the UI can refill the
    /// input box for an edit-and-resend.
    pub fn rewind(&mut self, timeout: Duration) -> Result<Option<String>, Error> {
        let value = self.call("rewind", None, timeout)?;
        if !value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(
            value
                .get("last_user")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ))
    }

    /// Skills available in this workspace.
    pub fn skill_list(&mut self, timeout: Duration) -> Result<Vec<String>, Error> {
        let value = self.call("skill.list", None, timeout)?;
        Ok(value
            .get("skills")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Activate skills by name; returns `(activated, unknown)`.
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
        Ok((pick("activated"), pick("unknown")))
    }

    /// Start a turn. The result arrives later (the channel stays open while
    /// `event` notifications stream).
    pub fn prompt(&mut self, text: &str) -> Result<Receiver<Result<Value, Error>>, Error> {
        self.send("prompt", Some(json!({ "text": text })))
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

fn decode_sessions(value: &Value) -> Result<Vec<SessionSummary>, Error> {
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

fn decode_transcript(value: &Value) -> Result<Vec<TranscriptMessage>, Error> {
    let rows = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("transcript result has no messages array".into()))?;
    rows.iter()
        .map(|row| {
            Ok(TranscriptMessage {
                role: string_field(row, "role")?,
                content: string_field(row, "content")?,
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
        let m = parse_message(r#"{"jsonrpc":"2.0","id":1,"result":{"rpc":"3"}}"#).unwrap();
        match m {
            Message::Response { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result.unwrap()["rpc"], "3");
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
    fn handshake_requires_rpc_3() {
        let ok = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"3"});
        let info = check_version(&ok).unwrap();
        assert_eq!(info.rpc, "3");
        // No capability list (older server): App::supports treats this as
        // "assume everything" rather than hiding features it cannot prove
        // absent.
        assert!(info.methods.is_empty());

        let modern = serde_json::json!({
            "name":"mow","version":"0.1.0","rpc":"4",
            "methods":["prompt","context","compact"],
            "control_methods":["context"],
        });
        let info = check_version(&modern).unwrap();
        assert_eq!(info.methods, vec!["prompt", "context", "compact"]);
        assert_eq!(info.control_methods, vec!["context".to_string()]);

        // Additive protocol: a newer server is fine, an older one is not.
        let newer = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"4"});
        assert_eq!(check_version(&newer).unwrap().rpc, "4");
        let older = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"2"});
        assert!(check_version(&older).is_err());

        let old = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"2"});
        let err = check_version(&old).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
        assert!(err.to_string().contains("\"2\""), "{err}");

        let missing = serde_json::json!({"name":"mow"});
        assert!(matches!(check_version(&missing), Err(Error::Protocol(_))));
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
    fn session_short_id() {
        let s = SessionInfo::from_value(&serde_json::json!({
            "session_id":"0123456789abcdef","workspace":"/w","model":"gpt-5-mini"
        }));
        assert_eq!(s.short_id(), "01234567");
        assert_eq!(s.model, "gpt-5-mini");
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
}
