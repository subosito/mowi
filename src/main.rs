//! mowi — Ratatui client for `mow rpc`.
//!
//! The Engine runs in a child process (`mow rpc`); this binary only paints and
//! sends host-protocol requests. It never embeds an Engine and never speaks ACP
//! to peers — peer management belongs to mow.

mod app;
mod render;
mod rpc;
mod slash;
mod snapshot;
mod theme;

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;
use rpc::Client;
use theme::{Theme, ThemeName};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Parser)]
#[command(
    name = "mowi",
    version,
    about = "Ratatui client for the mow harness (mow rpc)"
)]
struct Cli {
    /// mow binary to spawn (`mow rpc …`).
    #[arg(long, env = "MOW_BIN", default_value = "mow")]
    mow_bin: String,

    /// UI theme name.
    #[arg(long, env = "MOW_THEME", default_value = "catppuccin-mocha")]
    theme: ThemeName,

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

    /// Ask before power tools (engine flag).
    #[arg(long)]
    ask: bool,

    /// Run power tools without asking (engine flag).
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
        if self.ask {
            out.push("--ask".into());
        }
        if self.auto {
            out.push("--auto".into());
        }
        for spec in &self.extra_root {
            out.push("--extra-root".into());
            out.push(spec.clone());
        }
        out
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
        print_snapshots(scene, *width, *height, Theme::new(cli.theme));
        return ExitCode::SUCCESS;
    }
    let ping_only = cli.no_tui || matches!(cli.command, Some(Command::Ping));
    let res = if ping_only { ping(&cli) } else { tui(&cli) };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mowi: {e}");
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

fn tui(cli: &Cli) -> Result<(), rpc::Error> {
    let (mut client, session, version) = connect(cli)?;
    let mode = if cli.auto { "auto" } else { "ask" };
    client.perm_set(mode, HANDSHAKE_TIMEOUT)?;
    let resuming = cli.session.is_some() || cli.continue_session;
    let mut app = if resuming {
        let messages = client.transcript(HANDSHAKE_TIMEOUT)?;
        App::from_transcript(session, messages)
    } else {
        App::new(session)
    };
    app.theme = Theme::new(cli.theme);
    // Feature-detect once from the handshake instead of probing for -32601.
    app.set_capabilities(&version.methods);
    app.ask_mode = !cli.auto;
    app.allow_write = cli.allow_write;
    app.allow_shell = cli.allow_shell;
    if let Ok(status) = client.status(HANDSHAKE_TIMEOUT) {
        app.apply_status(&status);
    }
    // Splash only for a fresh session (no transcript seed).
    app.welcome = app.entries.is_empty();
    app.slash_commands = client.slash_list(HANDSHAKE_TIMEOUT)?;
    if let Ok(list) = client.effort_list(HANDSHAKE_TIMEOUT) {
        app.effort = list.current;
    }

    enable_raw_mode().map_err(rpc::Error::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(rpc::Error::Io)?;
    execute!(stdout, EnableBracketedPaste).map_err(rpc::Error::Io)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(rpc::Error::Io)?;

    let res = app::run(&mut terminal, &mut client, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    client.shutdown();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_verifies() {
        Cli::command().debug_assert();
    }

    #[test]
    fn theme_flag_accepts_full_names() {
        let cli = Cli::parse_from(["mowi", "--theme", "gruvbox-dark"]);
        assert_eq!(cli.theme, ThemeName::GruvboxDark);
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
                "format",
                "--ask"
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
