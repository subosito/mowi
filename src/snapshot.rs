//! Offline frame renderer: paint scripted UI states into a `TestBackend` and
//! dump them to stdout as truecolor ANSI.
//!
//! This exists so the look of the client can be reviewed (and diffed) without
//! a live Engine: `mowi snapshot --scene chat --width 100 --height 30`.
//! It is a design tool, not part of the runtime path.

use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};

use crate::app::{App, Entry, draw};
use crate::rpc::{ContextUsage, SessionInfo};
use crate::theme::Theme;

/// Every scene the snapshot tool can paint.
pub const SCENES: [&str; 7] = [
    "chat",
    "busy",
    "diff",
    "welcome",
    "help",
    "permission",
    "narrow",
];

fn demo_session() -> SessionInfo {
    SessionInfo {
        session_id: "01J8ZK4M7Q2XN5V9".into(),
        workspace: "/home/dev/src/mow".into(),
        model: "gpt-5-mini".into(),
        wire: "openai-responses".into(),
    }
}

fn demo_app() -> App {
    let mut app = App::new(demo_session());
    // Honour NO_COLOR exactly as the real client does, so the design tool can
    // show the monochrome frame instead of always painting truecolor.
    app.theme = Theme::detect();
    app.animate = false;
    app.effort = "medium".into();
    app.allow_write = true;
    app.allow_shell = true;
    app.ask_mode = true;
    app.apply_context(&ContextUsage {
        tokens: 41_500,
        context_window: Some(200_000),
        remaining: Some(158_500),
        percent: Some(20.75),
    });
    app.usage.input_tokens = 41_500;
    app.usage.output_tokens = 3_200;
    app
}

/// Build the app state for `scene`.
///
/// `NO_COLOR` is honoured here exactly as the real client honours it, so the
/// design tool can actually show the monochrome frame instead of always
/// painting the truecolor one.
pub fn scene(name: &str) -> App {
    let mut app = demo_app();
    match name {
        "welcome" => {
            app.welcome = true;
        }
        "help" => {
            app.entries.push(Entry::User("what changed?".into()));
            app.overlay = crate::app::Overlay::help();
        }
        "permission" => {
            app.entries.push(Entry::User("run the tests".into()));
            app.entries.push(Entry::Assistant(
                "Running the suite now — this needs shell access.".into(),
            ));
            app.pending_perm = Some(crate::rpc::PermissionRequest {
                id: "perm-1".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "cargo test --all-features" }),
                tool_call_id: "call-1".into(),
            });
        }
        "busy" => {
            app.entries
                .push(Entry::User("refactor the transcript renderer".into()));
            app.entries.push(Entry::Tool {
                name: "read src/app.rs".into(),
                duration_ms: Some(180),
            });
            app.entries.push(Entry::Tool {
                name: "grep wrap_styled_line".into(),
                duration_ms: None,
            });
            app.busy = true;
            app.status = "calling model".into();
            app.live
                .push_str("Looking at the wrapper now. The current implementation");
        }
        "diff" => {
            app.entries.push(Entry::User("fix the off-by-one".into()));
            app.entries.push(Entry::Assistant(
                "The guard used `<=` where the slice is exclusive.\n\n\
                 ```diff\n--- a/src/app.rs\n+++ b/src/app.rs\n@@ -212,7 +212,7 @@\n \
                 fn clip(text: &str, max: usize) -> String {\n\
                 -    if text.len() <= max {\n\
                 +    if text.len() < max {\n\
                      return text.to_string();\n \
                 }\n```\n\nThat keeps the ellipsis inside the budget."
                    .into(),
            ));
        }
        "narrow" => {
            app.entries.push(Entry::User("status?".into()));
            app.entries
                .push(Entry::Assistant("All green: 102 tests passing.".into()));
        }
        _ => {
            app.entries
                .push(Entry::User("summarise the architecture doc".into()));
            app.entries.push(Entry::Assistant(
                "## Split\n\n\
                 The Engine is **headless** (`mow rpc`); this client only paints.\n\n\
                 - the UI owns no model state\n\
                 - every mutation is a request\n\
                 - peers are managed by `mow`, not here\n\n\
                 See `docs/architecture.md` for the wire format."
                    .into(),
            ));
            app.entries.push(Entry::Tool {
                name: "read docs/architecture.md".into(),
                duration_ms: Some(1240),
            });
            app.input.push_str("now do the same for protocol.md");
            app.cursor = app.input.chars().count();
        }
    }
    app
}

/// Render `scene` at `width`x`height` and return it as an ANSI string.
pub fn render(name: &str, width: u16, height: u16) -> String {
    let mut app = scene(name);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
    ansi(terminal.backend().buffer())
}

fn sgr_color(color: Color, fg: bool) -> Option<String> {
    let base = if fg { 38 } else { 48 };
    match color {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some(format!("{base};2;{r};{g};{b}")),
        Color::Indexed(i) => Some(format!("{base};5;{i}")),
        _ => None,
    }
}

/// Convert a painted buffer into an ANSI block, one escape run per cell change.
fn ansi(buffer: &Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &buffer[(x, y)];
            let mut codes: Vec<String> = vec!["0".into()];
            if let Some(code) = sgr_color(cell.fg, true) {
                codes.push(code);
            }
            if let Some(code) = sgr_color(cell.bg, false) {
                codes.push(code);
            }
            if cell.modifier.contains(Modifier::BOLD) {
                codes.push("1".into());
            }
            if cell.modifier.contains(Modifier::DIM) {
                codes.push("2".into());
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                codes.push("3".into());
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                codes.push("4".into());
            }
            if cell.modifier.contains(Modifier::REVERSED) {
                codes.push("7".into());
            }
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                codes.push("9".into());
            }
            out.push_str(&format!("\x1b[{}m{}", codes.join(";"), cell.symbol()));
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scene_paints_without_panicking() {
        for name in SCENES {
            let out = render(name, 100, 30);
            assert!(!out.is_empty(), "scene {name} painted nothing");
        }
    }
}
