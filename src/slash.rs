//! Slash-command dispatch.
//!
//! Commands fall into three classes:
//!
//! - **Core local** — always offered (`/help`, `/quit`, `/clear`, …).
//! - **RPC-method-gated** — offered only when `version` / `capabilities`
//!   advertised the backing method or feature (`/compact`, `/steer`, …).
//! - **Pack-discovered** — offered only from the cached `slash.list`
//!   (`/goal`, `/review`, `/sec`, …). Never inferred from a stock build.
//!
//! Unknown names must never be forwarded as RPC `slash` — the server
//! answers `-32601`.

use std::collections::BTreeMap;

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

/// Advertised host surface used to decide Help / completion / behavior.
#[derive(Debug, Clone, Copy)]
pub struct HostOffer<'a> {
    pub methods: &'a [String],
    pub features: &'a BTreeMap<String, bool>,
    pub lsp_seen: bool,
}

impl HostOffer<'_> {
    pub fn method(self, name: &str) -> bool {
        self.methods
            .iter()
            .any(|method| method.eq_ignore_ascii_case(name))
    }

    pub fn feature(self, name: &str) -> bool {
        self.features.get(name).copied().unwrap_or(false)
    }
}

/// What must be advertised before a local-routed command is offered.
#[derive(Debug, Clone, Copy)]
enum Availability {
    Always,
    Method(&'static str),
    AnyMethod(&'static [&'static str]),
    Feature(&'static str),
    /// `/lsp` needs diagnostics events or an explicit lsp feature.
    Lsp,
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
    ("compact", Builtin::Local),
    ("context", Builtin::Local),
    ("rewind", Builtin::Local),
    ("undo", Builtin::Local),
    ("skills", Builtin::Local),
    ("steer", Builtin::Local),
    ("btw", Builtin::Local),
    ("perm", Builtin::Local),
    ("lsp", Builtin::Local),
];

/// Offer table: core vs RPC-gated. Pack commands are not listed here.
const LOCAL_OFFER: &[(&str, Availability)] = &[
    ("help", Availability::Always),
    ("clear", Availability::Always),
    ("search", Availability::Always),
    ("copy", Availability::Always),
    ("status", Availability::Always),
    ("quit", Availability::Always),
    ("exit", Availability::Always),
    (
        "model",
        Availability::AnyMethod(&["model.list", "model.set"]),
    ),
    (
        "effort",
        Availability::AnyMethod(&["effort.list", "effort.set"]),
    ),
    ("sessions", Availability::Method("sessions")),
    ("resume", Availability::Method("sessions")),
    ("transcript", Availability::Method("transcript")),
    ("compact", Availability::Method("compact")),
    ("context", Availability::Method("context")),
    ("steer", Availability::Method("steer")),
    (
        "skills",
        Availability::AnyMethod(&["skill.list", "skill.activate"]),
    ),
    ("edit", Availability::Method("rewind")),
    ("retry", Availability::Method("rewind")),
    ("rewind", Availability::Method("rewind")),
    ("undo", Availability::Method("rewind")),
    ("btw", Availability::Feature("ephemeral_prompt")),
    ("perm", Availability::AnyMethod(&["perm.set"])),
    ("lsp", Availability::Lsp),
];

/// Canonical names offered by autocomplete, in display order.
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
    "context",
    "compact",
    "rewind",
    "undo",
    "skills",
    "steer",
    "btw",
    "perm",
    "lsp",
    "quit",
    "exit",
];

/// Help text for offered local commands.
pub const LOCAL_HELP: &[(&str, &str)] = &[
    ("/edit", "rewind last turn into the composer"),
    ("/steer", "guide the running turn (while busy)"),
    ("/btw", "aside — not added to context"),
    ("/model", "list models, or /model <id> to set"),
    ("/effort", "list efforts, or /effort high to set"),
    ("/clear", "clear transcript (engine history kept)"),
    ("/quit", "quit"),
    ("/status", "session summary"),
    ("/lsp", "recent diagnostics"),
    ("/perm", "set ask / auto mode"),
    ("/compact", "compact history"),
    ("/context", "context window usage"),
    ("/sessions", "list resumable sessions"),
    ("/transcript", "reload engine history"),
    ("/skills", "list or activate skills"),
    ("/rewind", "drop the last exchange"),
];

fn available(rule: Availability, host: &HostOffer<'_>) -> bool {
    match rule {
        Availability::Always => true,
        Availability::Method(method) => host.method(method),
        Availability::AnyMethod(methods) => methods.iter().any(|method| host.method(method)),
        Availability::Feature(feature) => host.feature(feature),
        Availability::Lsp => {
            host.lsp_seen || host.feature("lsp") || host.feature("lsp_diagnostics")
        }
    }
}

/// True when Help / completion should advertise this local name.
pub fn command_offered(name: &str, host: &HostOffer<'_>) -> bool {
    let name = canonical_slash(name);
    if matches!(name, "quit" | "exit" | "q" | "help" | "?") {
        return true;
    }
    LOCAL_OFFER
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .map(|(_, rule)| available(*rule, host))
        .unwrap_or(false)
}

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
pub fn slash_completions(
    prefix: &str,
    pack_commands: &[SlashCommand],
    host: &HostOffer<'_>,
) -> Vec<String> {
    let prefix = prefix.trim_start_matches('/');
    let mut out: Vec<String> = COMPLETIONS
        .iter()
        .filter(|name| name.starts_with(prefix) && command_offered(name, host))
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
pub fn unknown_slash_message(
    name: &str,
    pack_commands: &[SlashCommand],
    host: &HostOffer<'_>,
) -> String {
    let mut names: Vec<String> = COMPLETIONS
        .iter()
        .filter(|n| command_offered(n, host))
        .map(|n| format!("/{n}"))
        .collect();
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

pub fn unavailable_slash_message(name: &str) -> String {
    format!(
        "/{} is not available on this host",
        name.trim_start_matches('/')
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

    fn stock_methods() -> Vec<String> {
        [
            "prompt",
            "cancel",
            "status",
            "session",
            "sessions",
            "transcript",
            "steer",
            "slash",
            "slash.list",
            "perm.set",
            "model.list",
            "model.set",
            "effort.list",
            "effort.set",
            "context",
            "compact",
            "rewind",
            "skill.list",
            "skill.activate",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    fn stock_features() -> BTreeMap<String, bool> {
        BTreeMap::from([
            ("ephemeral_prompt".into(), true),
            ("permission_gate".into(), true),
        ])
    }

    fn stock_host<'a>(
        methods: &'a [String],
        features: &'a BTreeMap<String, bool>,
    ) -> HostOffer<'a> {
        HostOffer {
            methods,
            features,
            lsp_seen: true,
        }
    }

    fn empty_host<'a>(
        methods: &'a [String],
        features: &'a BTreeMap<String, bool>,
    ) -> HostOffer<'a> {
        HostOffer {
            methods,
            features,
            lsp_seen: false,
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
    fn goal_routes_to_generic_rpc_when_advertised() {
        let packs = [pack("goal")];
        let methods = stock_methods();
        let features = stock_features();
        let host = stock_host(&methods, &features);
        assert_eq!(slash_route("/goal", &packs), SlashRoute::Rpc);
        assert!(slash_completions("go", &packs, &host).contains(&"goal".to_string()));
        let none_features = BTreeMap::new();
        let none = empty_host(&[], &none_features);
        assert_eq!(slash_route("goal", &[]), SlashRoute::Unknown);
        assert!(!slash_completions("go", &[], &none).contains(&"goal".to_string()));
    }

    #[test]
    fn bogus_is_a_local_error() {
        let packs = [pack("review")];
        let methods = stock_methods();
        let features = stock_features();
        let host = stock_host(&methods, &features);
        assert_eq!(slash_route("bogus", &packs), SlashRoute::Unknown);
        assert_eq!(slash_route("bogus", &[]), SlashRoute::Unknown);
        let msg = unknown_slash_message("bogus", &packs, &host);
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
            "context",
            "compact",
            "rewind",
            "undo",
            "skills",
            "steer",
            "btw",
            "perm",
            "lsp",
            "find",
            "yank",
        ] {
            assert_eq!(slash_route(name, &[]), SlashRoute::Local, "/{name}");
        }
    }

    #[test]
    fn steer_stays_local_even_if_a_pack_claims_it() {
        let packs = [pack("steer")];
        assert_eq!(slash_route("steer", &packs), SlashRoute::Local);
        assert_eq!(slash_route("steer", &[]), SlashRoute::Local);
    }

    #[test]
    fn local_commands_win_over_registered_pack_names() {
        let packs = [
            pack("context"),
            pack("compact"),
            pack("rewind"),
            pack("undo"),
            pack("skills"),
            pack("perm"),
            pack("lsp"),
        ];
        for name in [
            "context", "compact", "rewind", "undo", "skills", "perm", "lsp",
        ] {
            assert_eq!(slash_route(name, &packs), SlashRoute::Local, "/{name}");
        }
    }

    #[test]
    fn completions_offer_local_and_pack_when_advertised() {
        let packs = [pack("review")];
        let methods = stock_methods();
        let features = stock_features();
        let host = stock_host(&methods, &features);
        let all = slash_completions("", &packs, &host);
        assert!(all.contains(&"effort".into()), "{all:?}");
        assert!(all.contains(&"model".into()), "{all:?}");
        for name in [
            "context", "compact", "rewind", "undo", "skills", "perm", "lsp",
        ] {
            assert!(all.contains(&name.to_string()), "{name}: {all:?}");
        }
        assert!(all.contains(&"review".into()), "{all:?}");
        assert_eq!(
            slash_completions("eff", &packs, &host),
            vec!["effort".to_string()]
        );
        assert!(slash_completions("bogus", &packs, &host).is_empty());
    }

    #[test]
    fn completions_hide_unadvertised_optional_commands() {
        let methods = vec!["prompt".into(), "cancel".into(), "status".into()];
        let features = BTreeMap::new();
        let host = empty_host(&methods, &features);
        let all = slash_completions("", &[], &host);
        assert!(all.contains(&"help".into()), "{all:?}");
        assert!(all.contains(&"status".into()), "{all:?}");
        assert!(all.contains(&"clear".into()), "{all:?}");
        for name in [
            "compact", "steer", "skills", "model", "effort", "lsp", "btw", "goal",
        ] {
            assert!(!all.contains(&name.to_string()), "{name} leaked: {all:?}");
            assert!(!command_offered(name, &host), "{name}");
        }
        let msg = unknown_slash_message("compact", &[], &host);
        assert!(msg.contains("unknown /compact"), "{msg}");
        assert!(!msg.contains("/steer"), "{msg}");
        assert!(!msg.contains("/goal"), "{msg}");
    }

    #[test]
    fn empty_host_surface_does_not_infer_a_stock_build() {
        let features = BTreeMap::new();
        let host = empty_host(&[], &features);
        assert!(command_offered("help", &host));
        assert!(command_offered("quit", &host));
        assert!(!command_offered("compact", &host));
        assert!(!command_offered("steer", &host));
        assert!(!command_offered("skills", &host));
        assert!(!command_offered("model", &host));
        assert!(!command_offered("lsp", &host));
        assert!(!command_offered("btw", &host));
    }

    #[test]
    fn gated_local_command_uses_unavailable_copy() {
        assert_eq!(
            unavailable_slash_message("compact"),
            "/compact is not available on this host"
        );
        assert_eq!(
            unavailable_slash_message("/steer"),
            "/steer is not available on this host"
        );
    }
}
