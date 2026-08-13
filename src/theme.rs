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
}
