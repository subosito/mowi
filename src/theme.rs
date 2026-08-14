//! Catppuccin Mocha palette and the semantic roles the UI paints with.
//!
//! Rule: widgets never name raw colors. They ask for a *role* (`header`,
//! `spinner`, `badge_ok`, ...) so a future flavor swap is one table away.

use ratatui::style::{Color, Modifier, Style};

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
    fn color(self) -> Color {
        match self {
            Tone::Muted => mocha::OVERLAY1,
            Tone::Active => mocha::BLUE,
            Tone::Ok => mocha::GREEN,
            Tone::Warn => mocha::YELLOW,
            Tone::Error => mocha::RED,
            Tone::Peer => mocha::MAUVE,
        }
    }
}

/// Braille spinner — one cell wide, no layout jitter.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Frame used when animation is off (`MOW_NO_ANIM=1`, or a non-TTY capture).
/// Deliberately not `SPINNER[0]`: a braille frame reads as a stalled spinner,
/// while a filled dot reads as a steady "busy" light. The elapsed counter is
/// what conveys progress in that mode.
pub const SPINNER_STATIC: &str = "●";
/// Typing indicator: a pulsing three-dot cycle for streaming assistant text.
pub const TYPING: [&str; 4] = ["·  ", "·· ", "···", " ··"];

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // md_* roles are consumed as render.rs markdown lands
pub struct Theme {
    pub colored: bool,
}

impl Theme {
    pub fn detect() -> Self {
        Self {
            colored: std::env::var_os("NO_COLOR").is_none(),
        }
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
            Style::default().bg(mocha::BASE).fg(mocha::TEXT)
        } else {
            Style::default()
        }
    }

    /// Header / sunk chrome (`mantle`).
    pub fn mantle(self) -> Style {
        self.bg(mocha::MANTLE)
    }

    /// Deepest ground, for the modal scrim (`crust`).
    pub fn crust(self) -> Style {
        self.bg(mocha::CRUST)
    }

    /// Raised chrome: user bands, input (`surface0`).
    pub fn surface(self) -> Style {
        self.bg(mocha::SURFACE0)
    }

    /// Overlay fill (`surface1`).
    pub fn overlay(self) -> Style {
        self.bg(mocha::SURFACE1)
    }

    pub fn header_bg(self) -> Style {
        self.mantle()
    }

    /// Status bar ground. Same sunk tone as the header so the frame reads as a
    /// document held between two rails.
    pub fn footer_bg(self) -> Style {
        if self.colored {
            Style::default().bg(mocha::MANTLE).fg(mocha::SUBTEXT0)
        } else {
            Style::default()
        }
    }

    /// Scrim painted under a modal: the document stays visible but recedes, so
    /// the overlay reads as "in front of" rather than "instead of".
    pub fn scrim(self) -> Style {
        if self.colored {
            Style::default().bg(mocha::CRUST).fg(mocha::SURFACE1)
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
            Style::default().fg(mocha::LAVENDER).bg(mocha::SURFACE0)
        } else {
            Style::default()
        }
    }

    /// Selected row in a list/table overlay.
    pub fn selected(self) -> Style {
        if self.colored {
            Style::default()
                .bg(mocha::SURFACE2)
                .fg(mocha::TEXT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    // ---- text ----------------------------------------------------------

    pub fn text(self) -> Style {
        self.fg(mocha::TEXT)
    }

    pub fn header(self) -> Style {
        if self.colored {
            Style::default()
                .fg(mocha::MAUVE)
                .bg(mocha::MANTLE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn user(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::BLUE).patch(self.user_bg())
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn assistant(self) -> Style {
        self.text()
    }

    pub fn note(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::SUBTEXT0)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Quiet chrome: block borders, rules, hints.
    pub fn chrome(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::SURFACE2)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Border of the focused surface (input, active overlay).
    pub fn chrome_focus(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::LAVENDER)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Accent used for overlay titles and the welcome splash.
    pub fn accent(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::MAUVE)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Overlay title: accent on the overlay ground, so the title does not sit
    /// in a differently-coloured hole in the border.
    pub fn overlay_title(self) -> Style {
        if self.colored {
            Style::default()
                .fg(mocha::MAUVE)
                .bg(mocha::SURFACE1)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Something needs a decision or the frame cannot be drawn.
    pub fn warn(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::RED).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    // ---- indicators ----------------------------------------------------

    /// Safety chips (capabilities, ask/auto) — always legible.
    pub fn chip(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::YELLOW)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A status badge on the header/status ground.
    pub fn badge(self, tone: Tone) -> Style {
        if self.colored {
            Style::default()
                .fg(tone.color())
                .bg(mocha::MANTLE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Inverted badge: for the one thing that must be seen (busy, denied).
    pub fn badge_solid(self, tone: Tone) -> Style {
        if self.colored {
            Style::default()
                .fg(mocha::CRUST)
                .bg(tone.color())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// The animated braille spinner while a turn is in flight.
    pub fn spinner(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::SKY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Streaming-token "typing" pulse.
    pub fn typing(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::TEAL)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Delegated ACP peer work.
    pub fn peer(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::MAUVE)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// A tool call row in the transcript.
    pub fn tool(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::SAPPHIRE)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Elapsed / duration text next to a tool or task.
    pub fn timing(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::OVERLAY1)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Search match highlight.
    pub fn match_hit(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::CRUST).bg(mocha::YELLOW)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    // ---- markdown ------------------------------------------------------

    /// `# heading` rows, by depth (1-based; deeper levels cool down).
    pub fn md_heading(self, level: u8) -> Style {
        let color = match level {
            1 => mocha::MAUVE,
            2 => mocha::BLUE,
            3 => mocha::SAPPHIRE,
            _ => mocha::TEAL,
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
            Style::default().fg(mocha::PEACH).bg(mocha::SURFACE0)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Fenced block body.
    pub fn md_code_block(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::TEXT).bg(mocha::MANTLE)
        } else {
            Style::default()
        }
    }

    /// The language tag on a fence.
    pub fn md_code_lang(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::OVERLAY1).bg(mocha::MANTLE)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// List bullets / ordered markers.
    pub fn md_bullet(self) -> Style {
        self.fg(mocha::LAVENDER)
    }

    /// Blockquote bar and text.
    pub fn md_quote(self) -> Style {
        if self.colored {
            Style::default()
                .fg(mocha::SUBTEXT0)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Link text / URL.
    pub fn md_link(self) -> Style {
        if self.colored {
            Style::default()
                .fg(mocha::SAPPHIRE)
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
                .fg(mocha::LAVENDER)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    // ---- diff ----------------------------------------------------------

    pub fn add(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::GREEN).bg(mocha::ADD_BAND)
        } else {
            Style::default()
        }
    }

    pub fn del(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::RED).bg(mocha::DEL_BAND)
        } else {
            Style::default()
        }
    }

    pub fn context(self) -> Style {
        self.fg(mocha::SUBTEXT0)
    }

    /// Diff meta rows: `@@` hunk headers and `---` / `+++` file headers.
    pub fn diff_meta(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::OVERLAY1)
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
            Style::default().fg(mocha::BASE).bg(mocha::GREEN)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Word-diff chip inside a removed line: inverted against the band.
    pub fn del_chip(self) -> Style {
        if self.colored {
            Style::default().fg(mocha::BASE).bg(mocha::RED)
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
        let t = Theme { colored: false };
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
    }

    #[test]
    fn colored_theme_grounds_the_frame() {
        let t = Theme { colored: true };
        assert_eq!(t.base().bg, Some(mocha::BASE));
        assert_eq!(t.header().bg, Some(mocha::MANTLE));
    }

    #[test]
    fn tones_are_distinct() {
        let t = Theme { colored: true };
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
}
