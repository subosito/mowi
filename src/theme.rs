//! Named palettes and the semantic roles the UI paints with.
//!
//! Rule: widgets never name raw colors. They ask for a *role* (`header`,
//! `spinner`, `badge_ok`, ...) so a future flavor swap is one table away.

use ratatui::style::{Color, Modifier, Style};
use std::fmt;
use std::str::FromStr;

/// Raw Catppuccin Mocha ramp. https://catppuccin.com/palette
///
/// The full ramp is defined even where unused today, so a new role picks the
/// right hue instead of inventing one.
#[allow(dead_code)]
pub mod mocha {
    use ratatui::style::Color;

    pub const ROSEWATER: Color = Color::Rgb(0xf5, 0xe0, 0xdc);
    pub const FLAMINGO: Color = Color::Rgb(0xf2, 0xcd, 0xcd);
    pub const PINK: Color = Color::Rgb(0xf5, 0xc2, 0xe7);
    pub const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
    pub const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
    pub const MAROON: Color = Color::Rgb(0xeb, 0xa0, 0xac);
    pub const PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
    pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
    pub const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
    pub const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
    pub const SKY: Color = Color::Rgb(0x89, 0xdc, 0xeb);
    pub const SAPPHIRE: Color = Color::Rgb(0x74, 0xc7, 0xec);
    pub const BLUE: Color = Color::Rgb(0x89, 0xb4, 0xfa);
    pub const LAVENDER: Color = Color::Rgb(0xb4, 0xbe, 0xfe);

    pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
    pub const SUBTEXT1: Color = Color::Rgb(0xba, 0xc2, 0xde);
    pub const SUBTEXT0: Color = Color::Rgb(0xa6, 0xad, 0xc8);
    pub const OVERLAY2: Color = Color::Rgb(0x93, 0x99, 0xb2);
    pub const OVERLAY1: Color = Color::Rgb(0x7f, 0x84, 0x9c);
    pub const OVERLAY0: Color = Color::Rgb(0x6c, 0x70, 0x86);
    pub const SURFACE2: Color = Color::Rgb(0x58, 0x5b, 0x70);
    pub const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
    pub const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
    pub const BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
    pub const MANTLE: Color = Color::Rgb(0x18, 0x18, 0x25);
    pub const CRUST: Color = Color::Rgb(0x11, 0x11, 0x1b);

    /// Diff bands: deeper accent hue mixed into `base` so +/− rows pop more.
    pub const ADD_BAND: Color = Color::Rgb(0x26, 0x4f, 0x3d);
    pub const DEL_BAND: Color = Color::Rgb(0x5e, 0x2d, 0x3a);
}

/// Full theme identifiers accepted by the CLI and `MOW_THEME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    CatppuccinMocha,
    CatppuccinLatte,
    GruvboxDark,
    Monokai,
}

impl ThemeName {
    pub const ALL: [&'static str; 4] = [
        "catppuccin-mocha",
        "catppuccin-latte",
        "gruvbox-dark",
        "monokai",
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CatppuccinMocha => "catppuccin-mocha",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::GruvboxDark => "gruvbox-dark",
            Self::Monokai => "monokai",
        }
    }

    pub const fn palette(self) -> Palette {
        match self {
            Self::CatppuccinMocha => Palette::mocha(),
            Self::CatppuccinLatte => Palette::latte(),
            Self::GruvboxDark => Palette::gruvbox(),
            Self::Monokai => Palette::monokai(),
        }
    }
}

impl fmt::Display for ThemeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThemeName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "catppuccin-mocha" => Ok(Self::CatppuccinMocha),
            "catppuccin-latte" => Ok(Self::CatppuccinLatte),
            "gruvbox-dark" => Ok(Self::GruvboxDark),
            "monokai" => Ok(Self::Monokai),
            other => Err(format!(
                "unknown theme {other:?}; available themes: {}",
                Self::ALL.join(", ")
            )),
        }
    }
}

/// Colors consumed by semantic theme methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub overlay: Color,
    pub surface: Color,
    pub surface_deep: Color,
    pub base: Color,
    pub mantle: Color,
    pub crust: Color,
    pub accent: Color,
    pub rail: Color,
    pub blue: Color,
    pub cyan: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub red: Color,
    pub add_band: Color,
    pub del_band: Color,
}

impl Palette {
    pub const fn mocha() -> Self {
        Self {
            text: mocha::TEXT,
            muted: mocha::SUBTEXT0,
            overlay: mocha::SURFACE1,
            surface: mocha::SURFACE0,
            surface_deep: mocha::SURFACE2,
            base: mocha::BASE,
            mantle: mocha::MANTLE,
            crust: mocha::CRUST,
            accent: mocha::MAUVE,
            rail: mocha::LAVENDER,
            blue: mocha::BLUE,
            cyan: mocha::TEAL,
            peach: mocha::PEACH,
            yellow: mocha::YELLOW,
            green: mocha::GREEN,
            red: mocha::RED,
            add_band: mocha::ADD_BAND,
            del_band: mocha::DEL_BAND,
        }
    }

    pub const fn latte() -> Self {
        Self {
            text: Color::Rgb(0x4c, 0x4f, 0x69),
            muted: Color::Rgb(0x6c, 0x6f, 0x85),
            overlay: Color::Rgb(0xcc, 0xd0, 0xda),
            surface: Color::Rgb(0xe6, 0xe9, 0xef),
            surface_deep: Color::Rgb(0xbc, 0xc0, 0xcc),
            base: Color::Rgb(0xef, 0xf1, 0xf5),
            mantle: Color::Rgb(0xe6, 0xe9, 0xef),
            crust: Color::Rgb(0xdc, 0xde, 0xe4),
            accent: Color::Rgb(0x88, 0x39, 0x9b),
            rail: Color::Rgb(0x72, 0x62, 0xc6),
            blue: Color::Rgb(0x1e, 0x66, 0xf5),
            cyan: Color::Rgb(0x17, 0x93, 0xa5),
            peach: Color::Rgb(0xfe, 0x64, 0x0b),
            yellow: Color::Rgb(0xdf, 0x8e, 0x1d),
            green: Color::Rgb(0x40, 0xa0, 0x2b),
            red: Color::Rgb(0xd2, 0x0f, 0x39),
            add_band: Color::Rgb(0xd8, 0xed, 0xd2),
            del_band: Color::Rgb(0xf5, 0xd5, 0xdc),
        }
    }

    pub const fn gruvbox() -> Self {
        Self {
            text: Color::Rgb(0xeb, 0xdb, 0xb2),
            muted: Color::Rgb(0xa8, 0x99, 0x84),
            overlay: Color::Rgb(0x50, 0x49, 0x45),
            surface: Color::Rgb(0x3c, 0x38, 0x36),
            surface_deep: Color::Rgb(0x66, 0x5c, 0x54),
            base: Color::Rgb(0x28, 0x28, 0x28),
            mantle: Color::Rgb(0x1d, 0x20, 0x21),
            crust: Color::Rgb(0x14, 0x14, 0x14),
            accent: Color::Rgb(0xd7, 0x99, 0x21),
            rail: Color::Rgb(0xd7, 0x99, 0x21),
            blue: Color::Rgb(0x83, 0xa5, 0x98),
            cyan: Color::Rgb(0x8e, 0xc0, 0x7c),
            peach: Color::Rgb(0xfe, 0x80, 0x19),
            yellow: Color::Rgb(0xfa, 0xbd, 0x2f),
            green: Color::Rgb(0xb8, 0xbb, 0x26),
            red: Color::Rgb(0xfb, 0x49, 0x34),
            add_band: Color::Rgb(0x3b, 0x4a, 0x2d),
            del_band: Color::Rgb(0x52, 0x2f, 0x2b),
        }
    }

    pub const fn monokai() -> Self {
        Self {
            text: Color::Rgb(0xf8, 0xf8, 0xf2),
            muted: Color::Rgb(0x75, 0x71, 0x5e),
            overlay: Color::Rgb(0x49, 0x46, 0x3e),
            surface: Color::Rgb(0x3e, 0x3d, 0x32),
            surface_deep: Color::Rgb(0x66, 0x65, 0x5f),
            base: Color::Rgb(0x27, 0x28, 0x22),
            mantle: Color::Rgb(0x1e, 0x1f, 0x1c),
            crust: Color::Rgb(0x15, 0x16, 0x13),
            accent: Color::Rgb(0xae, 0x81, 0xff),
            rail: Color::Rgb(0x66, 0xd9, 0xef),
            blue: Color::Rgb(0x66, 0xd9, 0xef),
            cyan: Color::Rgb(0xa6, 0xe2, 0x2e),
            peach: Color::Rgb(0xfd, 0x97, 0x1f),
            yellow: Color::Rgb(0xe6, 0xdb, 0x74),
            green: Color::Rgb(0xa6, 0xe2, 0x2e),
            red: Color::Rgb(0xf9, 0x26, 0x72),
            add_band: Color::Rgb(0x35, 0x4a, 0x2a),
            del_band: Color::Rgb(0x55, 0x2c, 0x3a),
        }
    }
}

/// Semantic surface for a status/severity badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // full tone set; markdown/overlay work lands the rest
pub enum Tone {
    /// Neutral chrome (idle, counts).
    Muted,
    /// Work in flight.
    Active,
    /// Finished cleanly.
    Ok,
    /// Needs a decision.
    Warn,
    /// Failed / denied.
    Error,
    /// Delegated (ACP peer) work.
    Peer,
}

impl Tone {
    fn color(self, palette: Palette) -> Color {
        match self {
            Tone::Muted => palette.muted,
            Tone::Active => palette.blue,
            Tone::Ok => palette.green,
            Tone::Warn => palette.yellow,
            Tone::Error => palette.red,
            Tone::Peer => palette.accent,
        }
    }
}

/// Braille spinner — one cell wide, no layout jitter.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Tool activity uses a mechanical rotating arc, distinct from model work.
pub const TOOL_SPINNER: [&str; 4] = ["◜", "◝", "◞", "◟"];
pub const TOOL_SPINNER_STATIC: &str = "◆";

/// Frame used when animation is off (`MOW_NO_ANIM=1`, or a non-TTY capture).
/// Deliberately not `SPINNER[0]`: a braille frame reads as a stalled spinner,
/// while a filled dot reads as a steady "busy" light. The elapsed counter is
/// what conveys progress in that mode.
pub const SPINNER_STATIC: &str = "●";
/// Typing indicator: a pulsing three-dot cycle for streaming assistant text.
pub const TYPING: [&str; 4] = ["·  ", "·· ", "···", " ··"];

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: ThemeName,
    pub colored: bool,
    pub palette: Palette,
}

#[allow(dead_code)] // semantic roles are staged ahead of their UI consumers
impl Theme {
    pub fn new(name: ThemeName) -> Self {
        Self {
            name,
            colored: std::env::var_os("NO_COLOR").is_none(),
            palette: name.palette(),
        }
    }

    pub fn colored(name: ThemeName) -> Self {
        Self {
            name,
            colored: true,
            palette: name.palette(),
        }
    }

    pub fn plain(name: ThemeName) -> Self {
        Self {
            name,
            colored: false,
            palette: name.palette(),
        }
    }

    pub const fn name(self) -> ThemeName {
        self.name
    }

    pub fn detect() -> Self {
        let name = std::env::var("MOW_THEME")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(ThemeName::CatppuccinMocha);
        Self::new(name)
    }

    pub fn from_name(name: &str) -> Result<Self, String> {
        Ok(Self::new(name.parse()?))
    }

    /// `fg` only when color is enabled; otherwise fall back to modifiers so the
    /// NO_COLOR frame still has hierarchy.
    fn fg(self, color: Color) -> Style {
        if self.colored {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    fn bg(self, color: Color) -> Style {
        if self.colored {
            Style::default().bg(color)
        } else {
            Style::default()
        }
    }

    // ---- grounds -------------------------------------------------------

    /// Document ground (`base`). Empty cells pick this up so the frame is not
    /// terminal-black.
    pub fn base(self) -> Style {
        if self.colored {
            Style::default().bg(self.palette.base).fg(self.palette.text)
        } else {
            Style::default()
        }
    }

    /// Header / sunk chrome (`mantle`). Not used for the header/status rails —
    /// those sit on the terminal default so a second fill cannot misalign.
    pub fn mantle(self) -> Style {
        self.bg(self.palette.mantle)
    }

    /// Terminal default ground (`Color::Reset`). Clears a previous document
    /// fill so the header and status rails do not keep a mismatched wash.
    pub fn terminal(self) -> Style {
        Style::default().bg(Color::Reset)
    }

    /// Deepest ground, for the modal scrim (`crust`).
    pub fn crust(self) -> Style {
        self.bg(self.palette.crust)
    }

    /// Raised chrome: user bands (`surface0`).
    pub fn surface(self) -> Style {
        self.bg(self.palette.surface)
    }

    /// Overlay fill (`surface1`).
    pub fn overlay(self) -> Style {
        self.bg(self.palette.overlay)
    }

    pub fn header_bg(self) -> Style {
        self.terminal()
    }

    /// Status bar ground. Terminal default, same as the header — a second
    /// fill here is what read as a misaligned top/bottom panel.
    pub fn footer_bg(self) -> Style {
        if self.colored {
            Style::default().bg(Color::Reset).fg(self.palette.muted)
        } else {
            self.terminal()
        }
    }

    /// Scrim painted under a modal: the document stays visible but recedes, so
    /// the overlay reads as "in front of" rather than "instead of".
    pub fn scrim(self) -> Style {
        if self.colored {
            Style::default()
                .bg(self.palette.crust)
                .fg(self.palette.overlay)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn user_bg(self) -> Style {
        self.surface()
    }

    /// The accent rail down the left edge of a user message.
    pub fn user_rail(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.rail)
                .bg(self.palette.surface)
        } else {
            Style::default()
        }
    }

    /// Selected row in a list/table overlay.
    pub fn selected(self) -> Style {
        if self.colored {
            Style::default()
                .bg(self.palette.surface_deep)
                .fg(self.palette.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    // ---- text ----------------------------------------------------------

    pub fn text(self) -> Style {
        self.fg(self.palette.text)
    }

    pub fn header(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Historical user prompt. Peach, not blue: blue collided with tool
    /// chrome and read as a leftover composer artifact on the band.
    pub fn user(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.peach)
                .patch(self.user_bg())
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn assistant(self) -> Style {
        self.text()
    }

    pub fn note(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.muted)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Quiet chrome: block borders, rules, hints.
    pub fn chrome(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.surface_deep)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Border of the focused surface (active overlay).
    pub fn chrome_focus(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.accent)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Accent used for overlay titles and the welcome splash.
    pub fn accent(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.accent)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Overlay title: accent on the overlay ground, so the title does not sit
    /// in a differently-coloured hole in the border.
    pub fn overlay_title(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.accent)
                .bg(self.palette.overlay)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Something needs a decision or the frame cannot be drawn.
    pub fn warn(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.red)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    // ---- indicators ----------------------------------------------------

    /// Safety chips (capabilities, ask/auto) — always legible.
    pub fn chip(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.yellow)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A status badge on the header/status ground (terminal default).
    pub fn badge(self, tone: Tone) -> Style {
        if self.colored {
            Style::default()
                .fg(tone.color(self.palette))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Inverted badge: for the one thing that must be seen (busy, denied).
    pub fn badge_solid(self, tone: Tone) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.crust)
                .bg(tone.color(self.palette))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// The animated braille spinner while a turn is in flight.
    pub fn spinner(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Streaming-token "typing" pulse.
    pub fn typing(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.cyan)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Delegated ACP peer work.
    pub fn peer(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.accent)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A tool call row in the transcript.
    pub fn tool(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.blue)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Elapsed / duration text next to a tool or task.
    pub fn timing(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.muted)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Search match highlight.
    pub fn match_hit(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.crust)
                .bg(self.palette.yellow)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    // ---- markdown ------------------------------------------------------

    /// `# heading` rows, by depth (1-based; deeper levels cool down).
    pub fn md_heading(self, level: u8) -> Style {
        let color = match level {
            1 => self.palette.accent,
            2 => self.palette.blue,
            3 => self.palette.blue,
            _ => self.palette.cyan,
        };
        if self.colored {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// `**bold**`.
    pub fn md_strong(self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }

    /// `_italic_`.
    pub fn md_emphasis(self) -> Style {
        self.text().add_modifier(Modifier::ITALIC)
    }

    /// `~~strike~~`.
    pub fn md_strike(self) -> Style {
        self.note().add_modifier(Modifier::CROSSED_OUT)
    }

    /// `` `inline code` `` — tinted ground so it reads as a chip.
    pub fn md_code(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.peach)
                .bg(self.palette.surface)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Fenced block body.
    pub fn md_code_block(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.text)
                .bg(self.palette.mantle)
        } else {
            Style::default()
        }
    }

    /// The language tag on a fence.
    pub fn md_code_lang(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.muted)
                .bg(self.palette.mantle)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// List bullets / ordered markers.
    pub fn md_bullet(self) -> Style {
        self.fg(self.palette.accent)
    }

    /// Blockquote bar and text.
    pub fn md_quote(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.muted)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Link text / URL.
    pub fn md_link(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.blue)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        }
    }

    /// Horizontal rule.
    pub fn md_rule(self) -> Style {
        self.chrome()
    }

    /// Table header row.
    pub fn md_table_head(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    // ---- diff ----------------------------------------------------------

    pub fn add(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.green)
                .bg(self.palette.add_band)
        } else {
            Style::default()
        }
    }

    pub fn del(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.red)
                .bg(self.palette.del_band)
        } else {
            Style::default()
        }
    }

    pub fn context(self) -> Style {
        self.fg(self.palette.muted)
    }

    /// Diff meta rows: `@@` hunk headers and `---` / `+++` file headers.
    pub fn diff_meta(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.muted)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Sign column of an added line: accent on the add band.
    pub fn add_sign(self) -> Style {
        self.add().add_modifier(Modifier::BOLD)
    }

    /// Sign column of a removed line: accent on the del band.
    pub fn del_sign(self) -> Style {
        self.del().add_modifier(Modifier::BOLD)
    }

    /// Word-diff chip inside an added line: inverted against the band.
    pub fn add_chip(self) -> Style {
        if self.colored {
            Style::default()
                .fg(self.palette.base)
                .bg(self.palette.green)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Word-diff chip inside a removed line: inverted against the band.
    pub fn del_chip(self) -> Style {
        if self.colored {
            Style::default().fg(self.palette.base).bg(self.palette.red)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_theme_never_emits_color() {
        let t = Theme::plain(ThemeName::CatppuccinMocha);
        let styles = [
            t.base(),
            t.header(),
            t.user(),
            t.badge(Tone::Ok),
            t.badge_solid(Tone::Error),
            t.spinner(),
            t.md_heading(1),
            t.md_code(),
            t.add(),
            t.del(),
        ];
        for style in styles {
            assert!(style.fg.is_none(), "fg leaked with NO_COLOR");
            assert!(style.bg.is_none(), "bg leaked with NO_COLOR");
        }
        // Chrome rails use Reset (terminal default), not a palette wash.
        assert!(t.header_bg().fg.is_none());
        assert_eq!(t.header_bg().bg, Some(Color::Reset));
        assert!(t.footer_bg().fg.is_none());
        assert_eq!(t.footer_bg().bg, Some(Color::Reset));
    }

    #[test]
    fn colored_theme_grounds_the_frame() {
        let t = Theme::colored(ThemeName::CatppuccinMocha);
        assert_eq!(t.base().bg, Some(t.palette.base));
        assert_eq!(t.header_bg().bg, Some(Color::Reset));
        assert_eq!(t.footer_bg().bg, Some(Color::Reset));
        assert_eq!(t.header().bg, None);
        assert_eq!(t.user().fg, Some(t.palette.peach));
        assert_ne!(t.user().fg, Some(t.palette.blue));
    }

    #[test]
    fn tones_are_distinct() {
        let t = Theme::colored(ThemeName::CatppuccinMocha);
        let ok = t.badge(Tone::Ok).fg;
        let err = t.badge(Tone::Error).fg;
        let peer = t.badge(Tone::Peer).fg;
        assert_ne!(ok, err);
        assert_ne!(ok, peer);
        assert_ne!(err, peer);
    }

    #[test]
    fn spinner_frames_are_single_width() {
        for frame in SPINNER {
            assert_eq!(frame.chars().count(), 1);
        }
    }

    #[test]
    fn all_named_themes_have_complete_distinct_palettes() {
        let themes: Vec<_> = ThemeName::ALL
            .iter()
            .map(|name| Theme::colored(name.parse().unwrap()))
            .collect();
        assert_eq!(themes.len(), 4);
        assert!(themes.iter().all(|theme| theme.base().bg.is_some()));
        assert!(
            themes
                .windows(2)
                .all(|pair| pair[0].palette != pair[1].palette)
        );
    }

    #[test]
    fn unknown_theme_lists_available_names() {
        let error = Theme::from_name("solarized").unwrap_err();
        for name in ThemeName::ALL {
            assert!(error.contains(name), "{error}");
        }
    }
}
