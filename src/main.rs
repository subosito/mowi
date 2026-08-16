//! mowi - mow with interface.
//!
//! Built using Ratatui. The Engine runs in a child process (`mow rpc`); this
//! binary only paints and
//! sends host-protocol requests. It never embeds an Engine and never speaks ACP
//! to peers — peer management belongs to mow.

mod app;
mod config;
mod render;
mod rpc;
mod slash;
mod snapshot;
mod theme;

use std::io::{self, Write};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;
use config::{
    MowiConfig, UserSources, cli_permission_mode, decode_mowi_config, env_permission_mode,
    env_theme, resolve_config,
};
use rpc::Client;
use theme::{Theme, ThemeName};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Parser)]
#[command(name = "mowi", version, about = "mowi - mow with interface")]
struct Cli {
    /// mow binary to spawn (`mow rpc …`).
    #[arg(long, env = "MOW_BIN", default_value = "mow")]
    mow_bin: String,

    /// UI theme name (overrides `$MOW_THEME` and `extensions.mowi`).
    #[arg(long)]
    theme: Option<ThemeName>,

    /// Resume a session id (engine flag).
    #[arg(long)]
    session: Option<String>,

    /// Continue the most recent session (engine flag).
    #[arg(long = "continue")]
    continue_session: bool,

    /// Allow write/edit tools (engine flag).
    #[arg(long)]
    allow_write: bool,

    /// Allow shell tools (engine flag).
    #[arg(long)]
    allow_shell: bool,

    /// Model id (engine flag).
    #[arg(long)]
    model: Option<String>,

    /// Reasoning effort (engine flag).
    #[arg(long)]
    effort: Option<String>,

    /// Load a named skill unconditionally (repeatable; engine flag).
    #[arg(long = "skill", action = clap::ArgAction::Append)]
    skill: Vec<String>,

    /// Ask before power tools (RPC mode; overrides `$MOW_PERMISSION_MODE`
    /// and `extensions.mowi`).
    #[arg(long)]
    ask: bool,

    /// Run power tools without asking (RPC mode; overrides
    /// `$MOW_PERMISSION_MODE` and `extensions.mowi`).
    #[arg(long)]
    auto: bool,

    /// Extra FS root for path jail (repeatable; PATH, PATH:ro, or explicit PATH:rw)
    #[arg(
        long = "extra-root",
        value_name = "PATH",
        action = clap::ArgAction::Append,
        value_parser = parse_extra_root
    )]
    extra_root: Vec<String>,

    /// Handshake and exit instead of starting the UI.
    #[arg(long)]
    no_tui: bool,

    /// Trust this workspace for project `.mow/config` and skills.
    /// Delegates to `mow trust` (same store as the host).
    #[arg(long)]
    trust: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Spawn mow rpc, handshake, print session/model, exit.
    Ping,
    /// Paint a scripted UI state to stdout as ANSI (no Engine needed).
    Snapshot {
        /// Scene name, or `all` for every scene.
        #[arg(long, default_value = "all")]
        scene: String,
        #[arg(long, default_value_t = 100)]
        width: u16,
        #[arg(long, default_value_t = 30)]
        height: u16,
    },
    /// Allow project `.mow/config` and skills (delegates to `mow trust`).
    Trust {
        /// Workspace to trust or revoke (default: `.`).
        path: Option<String>,
        /// List trusted workspaces.
        #[arg(long)]
        list: bool,
        /// Revoke trust instead of granting it.
        #[arg(long)]
        revoke: bool,
    },
}

impl Cli {
    /// Engine flags passed straight through to `mow rpc`.
    fn engine_flags(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(s) = &self.session {
            out.push("--session".into());
            out.push(s.clone());
        }
        if self.continue_session {
            out.push("--continue".into());
        }
        if self.allow_write {
            out.push("--allow-write".into());
        }
        if self.allow_shell {
            out.push("--allow-shell".into());
        }
        if let Some(model) = &self.model {
            out.push("--model".into());
            out.push(model.clone());
        }
        if let Some(effort) = &self.effort {
            out.push("--effort".into());
            out.push(effort.clone());
        }
        for skill in &self.skill {
            out.push("--skill".into());
            out.push(skill.clone());
        }
        // Permission mode is a UI/RPC concern. `mow rpc` intentionally has no
        // --ask/--auto engine flags; after the handshake we apply the resolved
        // mode through `perm.set`. Passing these flags makes the child exit
        // before its first response and surfaces as "mow rpc connection closed".
        for spec in &self.extra_root {
            out.push("--extra-root".into());
            out.push(spec.clone());
        }
        out
    }
}

/// CLI `--ask`/`--auto` and `--theme`, then `$MOW_PERMISSION_MODE` / `$MOW_THEME`.
fn resolve_user_sources(cli: &Cli) -> Result<UserSources, String> {
    let permission_mode = match cli_permission_mode(cli.ask, cli.auto) {
        Some(mode) => Some(mode),
        None => env_permission_mode()?,
    };
    let theme = match cli.theme {
        Some(name) => Some(name),
        None => env_theme()?,
    };
    Ok(UserSources {
        permission_mode,
        theme,
    })
}

/// Feature-detect `extension.config`. Missing method or a failed call → defaults.
fn load_mowi_config(client: &mut Client, version: &rpc::VersionInfo) -> MowiConfig {
    let advertised = version
        .methods
        .iter()
        .any(|method| method.eq_ignore_ascii_case("extension.config"));
    if !advertised {
        return MowiConfig::default();
    }
    match client.extension_config("mowi", HANDSHAKE_TIMEOUT) {
        Ok(value) => decode_mowi_config(&value),
        Err(_) => MowiConfig::default(),
    }
}

/// Parse an extra-root spec the same way mow does (`SplitExtraRootSpec`):
/// `PATH:ro` is read-only; `PATH` / `PATH:rw` are read-write. The suffix is
/// case-insensitive. Returns `(path, read_only)`.
fn split_extra_root_spec(raw: &str) -> (String, bool) {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.ends_with(":ro") {
        return (raw[..raw.len() - 3].trim().to_string(), true);
    }
    if lower.ends_with(":rw") {
        return (raw[..raw.len() - 3].trim().to_string(), false);
    }
    (raw.to_string(), false)
}

/// Clap parser: reject empty specs so a typo does not silently drop a root.
fn parse_extra_root(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("must be PATH, PATH:ro, or PATH:rw".into());
    }
    let (path, _) = split_extra_root_spec(raw);
    if path.is_empty() {
        return Err("must be PATH, PATH:ro, or PATH:rw".into());
    }
    Ok(raw.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Command::Snapshot {
        scene,
        width,
        height,
    }) = &cli.command
    {
        let theme = match resolve_user_sources(&cli) {
            Ok(user) => Theme::new(resolve_config(&user, &MowiConfig::default()).theme),
            Err(e) => {
                eprintln!("mowi: {e}");
                return ExitCode::FAILURE;
            }
        };
        print_snapshots(scene, *width, *height, theme);
        return ExitCode::SUCCESS;
    }
    if cli.trust || matches!(&cli.command, Some(Command::Trust { .. })) {
        return run_trust(&cli);
    }
    let ping_only = cli.no_tui || matches!(cli.command, Some(Command::Ping));
    let res = if ping_only {
        ping(&cli)
    } else {
        tui_loop(&cli)
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mowi: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Grant, list, or revoke workspace trust via `mow trust` (one store).
fn run_trust(cli: &Cli) -> ExitCode {
    let mut args: Vec<String> = Vec::new();
    match &cli.command {
        Some(Command::Trust { path, list, revoke }) => {
            if *list {
                args.push("--list".into());
            }
            if *revoke {
                args.push("--revoke".into());
            }
            if let Some(p) = path {
                args.push(p.clone());
            }
        }
        _ => {}
    }
    match ProcessCommand::new(&cli.mow_bin)
        .arg("trust")
        .args(&args)
        .status()
    {
        Ok(status) => {
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(status.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            eprintln!(
                "mowi: failed to run `{} trust`: {e}\ninstall mow on PATH or set --mow-bin / $MOW_BIN",
                cli.mow_bin
            );
            ExitCode::FAILURE
        }
    }
}

/// Print one scene, or every scene with a caption, as ANSI blocks.
fn print_snapshots(scene: &str, width: u16, height: u16, theme: Theme) {
    let mut out = io::stdout();
    let scenes: Vec<&str> = if scene == "all" {
        snapshot::SCENES.to_vec()
    } else {
        vec![scene]
    };
    for name in scenes {
        let _ = writeln!(out, "\n=== {name} ({width}x{height}) ===");
        let _ = write!(
            out,
            "{}",
            snapshot::render_with_theme(name, width, height, theme)
        );
    }
}

fn connect(cli: &Cli) -> Result<(Client, rpc::SessionInfo, rpc::VersionInfo), rpc::Error> {
    let mut client = Client::spawn(&cli.mow_bin, &cli.engine_flags())?;
    let (version, session) = client.handshake(HANDSHAKE_TIMEOUT)?;
    Ok((client, session, version))
}

fn ping(cli: &Cli) -> Result<(), rpc::Error> {
    let (mut client, session, version) = connect(cli)?;
    client.ping(HANDSHAKE_TIMEOUT)?;
    let mut out = io::stdout();
    let _ = writeln!(
        out,
        "{} {} (rpc {})\nworkspace: {}\nmodel: {}\nsession: {}",
        version.name,
        version.version,
        version.rpc,
        session.workspace,
        session.model,
        session.session_id,
    );
    client.shutdown();
    Ok(())
}

fn tui_loop(cli: &Cli) -> Result<(), rpc::Error> {
    let mut session = cli.session.clone();
    loop {
        match tui(cli, session.as_deref()) {
            Err(rpc::Error::ResumeSession(id)) => session = Some(id),
            other => return other,
        }
    }
}

fn connect_with_session(
    cli: &Cli,
    resume: Option<&str>,
) -> Result<(Client, rpc::SessionInfo, rpc::VersionInfo), rpc::Error> {
    Client::spawn(&cli.mow_bin, &engine_flags_for(cli, resume)).and_then(|mut client| {
        let (version, session) = client.handshake(HANDSHAKE_TIMEOUT)?;
        Ok((client, session, version))
    })
}

fn engine_flags_for(cli: &Cli, resume: Option<&str>) -> Vec<String> {
    let Some(id) = resume else {
        return cli.engine_flags();
    };
    let original = cli.engine_flags();
    let mut flags = Vec::with_capacity(original.len() + 2);
    let mut i = 0;
    while i < original.len() {
        if original[i] == "--session" {
            i += 2;
            continue;
        }
        if original[i] == "--continue" {
            i += 1;
            continue;
        }
        flags.push(original[i].clone());
        i += 1;
    }
    flags.push("--session".into());
    flags.push(id.to_string());
    flags
}

fn tui(cli: &Cli, resume: Option<&str>) -> Result<(), rpc::Error> {
    let (mut client, session, version) = connect_with_session(cli, resume)?;
    let user = resolve_user_sources(cli).map_err(rpc::Error::Protocol)?;
    let pack = load_mowi_config(&mut client, &version);
    let resolved = resolve_config(&user, &pack);
    client.perm_set(resolved.permission_mode.as_str(), HANDSHAKE_TIMEOUT)?;
    let resuming = resume.is_some() || cli.session.is_some() || cli.continue_session;
    let mut app = if resuming {
        let messages = client.transcript(HANDSHAKE_TIMEOUT)?;
        App::from_transcript(session, messages)
    } else {
        App::new(session)
    };
    app.apply_resolved_config(&resolved);
    // Feature-detect from the advertised surface. Empty methods stay empty.
    app.apply_host_surface(&version);
    app.allow_write = cli.allow_write;
    app.allow_shell = cli.allow_shell;
    if let Ok(status) = client.status(HANDSHAKE_TIMEOUT) {
        app.apply_status(&status);
    }
    // Status can lag `perm.set` on `--continue`. Re-apply so `--ask`/`--auto` win.
    app.apply_resolved_config(&resolved);
    // Splash only for a fresh session (no transcript seed) when config allows it.
    app.welcome = app.entries.is_empty() && resolved.welcome;
    if app.supports("slash.list") {
        app.slash_commands = client.slash_list(HANDSHAKE_TIMEOUT)?;
    }
    if app.supports("effort.list")
        && let Ok(list) = client.effort_list(HANDSHAKE_TIMEOUT)
    {
        app.effort = list.current;
    }

    enable_raw_mode().map_err(rpc::Error::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(rpc::Error::Io)?;
    execute!(stdout, EnableBracketedPaste, EnableMouseCapture).map_err(rpc::Error::Io)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(rpc::Error::Io)?;

    let res = app::run(&mut terminal, &mut client, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    client.shutdown();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use config::PermissionMode;

    #[test]
    fn cli_verifies() {
        Cli::command().debug_assert();
    }

    #[test]
    fn theme_flag_accepts_full_names() {
        let cli = Cli::parse_from(["mowi", "--theme", "gruvbox-dark"]);
        assert_eq!(cli.theme, Some(ThemeName::GruvboxDark));
    }

    #[test]
    fn engine_flags_for_replaces_session_and_continue() {
        let cli = Cli::parse_from(["mowi", "--continue", "--allow-write"]);
        assert_eq!(
            engine_flags_for(&cli, Some("s-99")),
            vec!["--allow-write", "--session", "s-99"]
        );
        let cli = Cli::parse_from(["mowi", "--session", "old", "--model", "gpt-5-mini"]);
        assert_eq!(
            engine_flags_for(&cli, Some("new")),
            vec!["--model", "gpt-5-mini", "--session", "new"]
        );
    }

    #[test]
    fn theme_and_ask_auto_absent_are_none() {
        let cli = Cli::parse_from(["mowi"]);
        assert_eq!(cli.theme, None);
        assert!(!cli.ask);
        assert!(!cli.auto);
        assert_eq!(cli_permission_mode(cli.ask, cli.auto), None);
    }

    #[test]
    fn ask_and_auto_flags_are_explicit() {
        let ask = Cli::parse_from(["mowi", "--ask"]);
        assert!(ask.ask);
        assert!(!ask.auto);
        assert_eq!(
            cli_permission_mode(ask.ask, ask.auto),
            Some(PermissionMode::Ask)
        );
        let auto = Cli::parse_from(["mowi", "--auto"]);
        assert!(auto.auto);
        assert!(!auto.ask);
        assert_eq!(
            cli_permission_mode(auto.ask, auto.auto),
            Some(PermissionMode::Auto)
        );
    }

    #[test]
    fn unknown_theme_error_lists_available_names() {
        let err = Cli::try_parse_from(["mowi", "--theme", "solarized"]).unwrap_err();
        for name in ThemeName::ALL {
            assert!(err.to_string().contains(name), "{err}");
        }
    }

    #[test]
    fn engine_flags_pass_through() {
        let cli = Cli::parse_from([
            "mowi",
            "--session",
            "abc",
            "--model",
            "gpt-5-mini",
            "--effort",
            "high",
            "--skill",
            "review",
            "--skill",
            "format",
            "--allow-write",
            "--allow-shell",
            "--ask",
        ]);
        assert_eq!(
            cli.engine_flags(),
            vec![
                "--session",
                "abc",
                "--allow-write",
                "--allow-shell",
                "--model",
                "gpt-5-mini",
                "--effort",
                "high",
                "--skill",
                "review",
                "--skill",
                "format"
            ]
        );
        assert_eq!(cli.mow_bin, "mow");
    }

    #[test]
    fn continue_maps_to_engine_flag() {
        let cli = Cli::parse_from(["mowi", "--continue"]);
        assert_eq!(cli.engine_flags(), vec!["--continue"]);
    }

    #[test]
    fn skill_is_repeatable_and_forwards_in_order() {
        let cli = Cli::parse_from([
            "mowi",
            "--skill",
            "review",
            "--skill=format",
            "--skill",
            "security",
        ]);
        assert_eq!(
            cli.engine_flags(),
            vec![
                "--skill", "review", "--skill", "format", "--skill", "security",
            ]
        );
    }

    #[test]
    fn ping_subcommand_parses() {
        let cli = Cli::parse_from(["mowi", "ping"]);
        assert!(matches!(cli.command, Some(Command::Ping)));
        assert!(cli.engine_flags().is_empty());
    }

    #[test]
    fn trust_flag_and_subcommand_parse() {
        let flag = Cli::parse_from(["mowi", "--trust"]);
        assert!(flag.trust);
        assert!(flag.command.is_none());

        let sub = Cli::parse_from(["mowi", "trust", "--list"]);
        assert!(!sub.trust);
        match sub.command {
            Some(Command::Trust {
                list,
                revoke,
                path,
            }) => {
                assert!(list);
                assert!(!revoke);
                assert!(path.is_none());
            }
            other => panic!("expected Trust, got {other:?}"),
        }

        let revoke = Cli::parse_from(["mowi", "trust", "--revoke", "/tmp/ws"]);
        match revoke.command {
            Some(Command::Trust {
                list,
                revoke,
                path,
            }) => {
                assert!(!list);
                assert!(revoke);
                assert_eq!(path.as_deref(), Some("/tmp/ws"));
            }
            other => panic!("expected Trust, got {other:?}"),
        }
    }

    #[test]
    fn extra_root_repeatable_forwards_rw_ro_specs() {
        let cli = Cli::parse_from([
            "mowi",
            "--extra-root",
            "/rw/one",
            "--extra-root",
            "/ro/one:ro",
            "--extra-root",
            "/rw/two:rw",
        ]);
        assert_eq!(
            cli.engine_flags(),
            vec![
                "--extra-root",
                "/rw/one",
                "--extra-root",
                "/ro/one:ro",
                "--extra-root",
                "/rw/two:rw",
            ]
        );
        assert_eq!(split_extra_root_spec("/rw/one"), ("/rw/one".into(), false));
        assert_eq!(
            split_extra_root_spec("/ro/one:ro"),
            ("/ro/one".into(), true)
        );
        assert_eq!(
            split_extra_root_spec("/rw/two:rw"),
            ("/rw/two".into(), false)
        );
        assert_eq!(split_extra_root_spec(" /tmp:RO "), ("/tmp".into(), true));
    }

    #[test]
    fn extra_root_rejects_empty_path() {
        for spec in [":ro", ":rw", "   ", ":RO"] {
            let err = Cli::try_parse_from(["mowi", "--extra-root", spec]).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("PATH, PATH:ro, or PATH:rw"),
                "spec {spec:?}: {msg}"
            );
        }
    }

    #[test]
    fn extra_root_help_documents_modes() {
        let mut buf = Vec::new();
        Cli::command().write_long_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        assert!(help.contains("--extra-root"), "{help}");
        assert!(help.contains("PATH:ro"), "{help}");
        assert!(help.contains("PATH:rw"), "{help}");
        assert!(help.contains("--skill"), "{help}");
        assert!(help.contains("--trust"), "{help}");
        assert!(help.contains("trust"), "{help}");
    }

    #[test]
    fn missing_binary_is_a_clear_error() {
        let cli = Cli::parse_from(["mowi", "--mow-bin", "mow-does-not-exist-xyz", "ping"]);
        let err = ping(&cli).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("MOW_BIN"), "{msg}");
    }
}
