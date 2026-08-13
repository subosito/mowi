//! Slash-command dispatch.
//!
//! Pack commands (`review`, `sec`, …) come from a cached `slash.list`.
//! Everything else the operator types is owned by the UI. Unknown names
//! must never be forwarded as RPC `slash` — the server answers `-32601`.

use crate::rpc::SlashCommand;

/// Where a typed `/name` is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashRoute {
    /// Quit the UI (cancelling an in-flight turn first).
    Quit,
    /// Handled by the UI; never sent to the host.
    Local,
    /// Forwarded to the host as an RPC `slash` call.
    Rpc,
    /// Not a local command and not in the cached pack list.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    Quit,
    Local,
}

/// Explicit UI-owned names. Packs cannot steal these.
const DISPATCH: &[(&str, Builtin)] = &[
    ("quit", Builtin::Quit),
    ("exit", Builtin::Quit),
    ("q", Builtin::Quit),
    ("help", Builtin::Local),
    ("?", Builtin::Local),
    ("clear", Builtin::Local),
    ("model", Builtin::Local),
    ("effort", Builtin::Local),
    ("sessions", Builtin::Local),
    ("transcript", Builtin::Local),
    ("resume", Builtin::Local),
    ("search", Builtin::Local),
    ("find", Builtin::Local),
    ("copy", Builtin::Local),
    ("yank", Builtin::Local),
    ("edit", Builtin::Local),
    ("retry", Builtin::Local),
    ("regen", Builtin::Local),
    ("status", Builtin::Local),
    ("steer", Builtin::Local),
];

/// Canonical names offered by autocomplete (no one-letter aliases).
const COMPLETIONS: &[&str] = &[
    "help",
    "clear",
    "model",
    "effort",
    "sessions",
    "transcript",
    "resume",
    "search",
    "copy",
    "edit",
    "retry",
    "status",
    "steer",
    "quit",
    "exit",
];

/// Route a slash command name (without a required leading `/`).
///
/// `pack_commands` is the cached `slash.list` result from handshake.
pub fn slash_route(name: &str, pack_commands: &[SlashCommand]) -> SlashRoute {
    let name = name.trim_start_matches('/');
    if name.is_empty() {
        return SlashRoute::Unknown;
    }
    for (token, dest) in DISPATCH {
        if *token == name {
            return match dest {
                Builtin::Quit => SlashRoute::Quit,
                Builtin::Local => SlashRoute::Local,
            };
        }
    }
    if pack_matches(name, pack_commands) {
        return SlashRoute::Rpc;
    }
    SlashRoute::Unknown
}

/// Map aliases onto the handler name (`find` → `search`).
pub fn canonical_slash(name: &str) -> &str {
    match name.trim_start_matches('/') {
        "find" => "search",
        "yank" => "copy",
        "regen" => "retry",
        "?" => "help",
        other => other,
    }
}

fn pack_matches(name: &str, pack_commands: &[SlashCommand]) -> bool {
    pack_commands.iter().any(|command| {
        let cmd = command.name.trim_start_matches('/');
        if cmd == name {
            return true;
        }
        command
            .aliases
            .iter()
            .any(|alias| alias.trim_start_matches('/') == name)
    })
}

/// Local + cached pack names matching `prefix` (no leading slash on either).
pub fn slash_completions(prefix: &str, pack_commands: &[SlashCommand]) -> Vec<String> {
    let prefix = prefix.trim_start_matches('/');
    let mut out: Vec<String> = COMPLETIONS
        .iter()
        .filter(|name| name.starts_with(prefix))
        .map(|name| (*name).to_string())
        .collect();
    for command in pack_commands {
        let name = command.name.trim_start_matches('/');
        if name.starts_with(prefix) && !out.iter().any(|existing| existing == name) {
            out.push(name.to_string());
        }
        for alias in &command.aliases {
            let alias = alias.trim_start_matches('/');
            if alias.starts_with(prefix) && !out.iter().any(|existing| existing == alias) {
                out.push(alias.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Friendly local error: never send this name to the host.
pub fn unknown_slash_message(name: &str, pack_commands: &[SlashCommand]) -> String {
    let mut names: Vec<String> = COMPLETIONS.iter().map(|n| format!("/{n}")).collect();
    for command in pack_commands {
        let n = format!("/{}", command.name.trim_start_matches('/'));
        if !names.contains(&n) {
            names.push(n);
        }
    }
    format!(
        "unknown /{} — try {}",
        name.trim_start_matches('/'),
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            summary: format!("{name} pack"),
            exclusive: true,
            aliases: vec![],
        }
    }

    #[test]
    fn effort_is_local_never_rpc() {
        let packs = [pack("review"), pack("sec")];
        assert_eq!(slash_route("effort", &packs), SlashRoute::Local);
        assert_eq!(slash_route("effort", &[]), SlashRoute::Local);
        assert_eq!(canonical_slash("effort"), "effort");
    }

    #[test]
    fn review_routes_to_rpc_only_when_cached() {
        let packs = [pack("review"), pack("sec")];
        assert_eq!(slash_route("review", &packs), SlashRoute::Rpc);
        assert_eq!(slash_route("sec", &packs), SlashRoute::Rpc);
        assert_eq!(slash_route("review", &[]), SlashRoute::Unknown);
    }

    #[test]
    fn bogus_is_a_local_error() {
        let packs = [pack("review")];
        assert_eq!(slash_route("bogus", &packs), SlashRoute::Unknown);
        assert_eq!(slash_route("bogus", &[]), SlashRoute::Unknown);
        let msg = unknown_slash_message("bogus", &packs);
        assert!(msg.contains("unknown /bogus"), "{msg}");
        assert!(msg.contains("/effort"), "{msg}");
        assert!(msg.contains("/review"), "{msg}");
        assert!(!msg.contains("unknown slash command"), "{msg}");
    }

    #[test]
    fn quit_aliases_and_local_table() {
        for name in ["quit", "exit", "q"] {
            assert_eq!(slash_route(name, &[]), SlashRoute::Quit, "/{name}");
        }
        for name in [
            "help",
            "clear",
            "model",
            "effort",
            "sessions",
            "transcript",
            "resume",
            "search",
            "copy",
            "edit",
            "retry",
            "status",
            "steer",
            "find",
            "yank",
        ] {
            assert_eq!(slash_route(name, &[]), SlashRoute::Local, "/{name}");
        }
    }

    #[test]
    fn completions_offer_local_and_pack() {
        let packs = [pack("review")];
        let all = slash_completions("", &packs);
        assert!(all.contains(&"effort".into()), "{all:?}");
        assert!(all.contains(&"model".into()), "{all:?}");
        assert!(all.contains(&"review".into()), "{all:?}");
        assert_eq!(slash_completions("eff", &packs), vec!["effort".to_string()]);
        assert!(slash_completions("bogus", &packs).is_empty());
    }
}
