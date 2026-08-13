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
pub const RPC_VERSION: &str = "3";

#[derive(Debug)]
pub enum Error {
    /// The mow binary could not be started.
    Spawn(String),
    Io(io::Error),
    /// JSON-RPC error envelope from the server.
    Rpc { code: i64, message: String },
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
}

/// Validate a `version` result: `rpc` must be `"3"`.
pub fn check_version(v: &Value) -> Result<VersionInfo, Error> {
    let rpc = v
        .get("rpc")
        .and_then(|r| r.as_str())
        .ok_or_else(|| Error::Protocol("mow rpc: version result has no \"rpc\" field".into()))?;
    if rpc != RPC_VERSION {
        return Err(Error::Protocol(format!(
            "mow rpc protocol {rpc:?}, need {RPC_VERSION:?}: rebuild mow with a current ext/rpc"
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
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
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
    let kind = params.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind.starts_with("harness.delegate") {
        return None;
    }
    let delta = params.get("delta").and_then(|d| d.as_str());
    if kind.contains("token") {
        return delta.or_else(|| params.get("text").and_then(|t| t.as_str()));
    }
    delta
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, Error>>>>>;

/// A spawned `mow rpc` child plus its reader thread.
pub struct Client {
    child: Child,
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
            .stderr(Stdio::inherit());
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
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ntx, nrx) = channel();

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
        })
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
        if let Err(e) = self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()) {
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
        let line = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"loop.token","delta":"hi"}}"#;
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
    fn handshake_requires_rpc_3() {
        let ok = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"3"});
        assert_eq!(check_version(&ok).unwrap().rpc, "3");

        let old = serde_json::json!({"name":"mow","version":"0.1.0","rpc":"2"});
        let err = check_version(&old).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
        assert!(err.to_string().contains("\"2\""), "{err}");

        let missing = serde_json::json!({"name":"mow"});
        assert!(matches!(check_version(&missing), Err(Error::Protocol(_))));
    }

    #[test]
    fn delegate_chunks_are_not_host_tokens() {
        let peer = serde_json::json!({"type":"harness.delegate.chunk","agent":"peer-agent","delta":"x"});
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
}
