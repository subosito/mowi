use ratatui::style::{Color, Modifier, Style};

/// Small Catppuccin-mocha-inspired palette used by the document renderer.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub colored: bool,
}

impl Theme {
    pub fn detect() -> Self {
        Self {
            colored: std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn header(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(203, 166, 247))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn user(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(137, 180, 250))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    pub fn assistant(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(205, 214, 244))
        } else {
            Style::default()
        }
    }

    pub fn note(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(166, 173, 200))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn add(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(166, 227, 161))
                .bg(Color::Rgb(51, 65, 56))
        } else {
            Style::default()
        }
    }

    pub fn del(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(243, 139, 168))
                .bg(Color::Rgb(77, 50, 64))
        } else {
            Style::default()
        }
    }

    pub fn context(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(166, 173, 200))
        } else {
            Style::default()
        }
    }

    /// Diff meta rows: `@@` hunk headers and `---` / `+++` file headers.
    pub fn diff_meta(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(127, 132, 156))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Sign column of an added line: accent on the add band.
    pub fn add_sign(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(166, 227, 161))
                .bg(Color::Rgb(51, 65, 56))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Sign column of a removed line: accent on the del band.
    pub fn del_sign(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(243, 139, 168))
                .bg(Color::Rgb(77, 50, 64))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Word-diff chip inside an added line: inverted against the band.
    pub fn add_chip(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(30, 30, 46))
                .bg(Color::Rgb(166, 227, 161))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Word-diff chip inside a removed line: inverted against the band.
    pub fn del_chip(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(30, 30, 46))
                .bg(Color::Rgb(243, 139, 168))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// Quiet chrome: block borders, rules, hints.
    pub fn chrome(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(88, 91, 112))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Safety chips (capabilities, ask/auto) — always legible.
    pub fn chip(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(249, 226, 175))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Something needs a decision or the frame cannot be drawn.
    pub fn warn(self) -> Style {
        if self.colored {
            Style::default()
                .fg(Color::Rgb(243, 139, 168))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }

    /// Accent used for overlay titles and the welcome splash.
    pub fn accent(self) -> Style {
        if self.colored {
            Style::default().fg(Color::Rgb(203, 166, 247))
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        }
    }
}
