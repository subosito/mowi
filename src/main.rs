//! mowi — Ratatui client for `mow rpc`.
//!
//! The Engine runs in a child process (`mow rpc`); this binary only paints and
//! sends host-protocol requests. It never embeds an Engine and never speaks ACP
//! to peers — peer management belongs to mow.

mod app;
mod render;
mod rpc;
mod theme;

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;
use rpc::Client;

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

    /// Ask before power tools (engine flag).
    #[arg(long)]
    ask: bool,

    /// Run power tools without asking (engine flag).
    #[arg(long)]
    auto: bool,

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
        if self.ask {
            out.push("--ask".into());
        }
        if self.auto {
            out.push("--auto".into());
        }
        out
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    let (mut client, session, _version) = connect(cli)?;
    let mode = if cli.auto { "auto" } else { "ask" };
    client.perm_set(mode, HANDSHAKE_TIMEOUT)?;
    let resuming = cli.session.is_some() || cli.continue_session;
    let mut app = if resuming {
        let messages = client.transcript(HANDSHAKE_TIMEOUT)?;
        App::from_transcript(session, messages)
    } else {
        App::new(session)
    };
    app.ask_mode = !cli.auto;
    app.allow_write = cli.allow_write;
    app.allow_shell = cli.allow_shell;
    if let Ok(status) = client.status(HANDSHAKE_TIMEOUT) {
        app.apply_status(&status);
    }
    // Splash only for a fresh session (no transcript seed).
    app.welcome = app.entries.is_empty();
    app.slash_commands = client.slash_list(HANDSHAKE_TIMEOUT)?;

    enable_raw_mode().map_err(rpc::Error::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).map_err(rpc::Error::Io)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(rpc::Error::Io)?;

    let res = app::run(&mut terminal, &mut client, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
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
    fn engine_flags_pass_through() {
        let cli = Cli::parse_from([
            "mowi",
            "--session",
            "abc",
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
    fn ping_subcommand_parses() {
        let cli = Cli::parse_from(["mowi", "ping"]);
        assert!(matches!(cli.command, Some(Command::Ping)));
        assert!(cli.engine_flags().is_empty());
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
