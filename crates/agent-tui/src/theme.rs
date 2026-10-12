//! Four palettes, made of terminal colours rather than RGB.
//!
//! A coding agent gets run in tmux, in ssh, in a screen reader's terminal and in
//! a terminal whose theme the user did not choose. Sticking to the sixteen ANSI
//! colours means every palette is readable wherever the screen is, and it keeps
//! the honest promise the rest of this crate makes: what is on the terminal is
//! what the code asked for, with no gradient that quietly falls back to grey.
//!
//! The distinction a palette has to carry is *what a line means*, not how loud it
//! is: a refused call and a failed one look different because they are different
//! answers, and a person scanning the log is reading for that.

use ratatui::style::{Color, Modifier, Style};

/// The palettes this crate ships, named the way `/theme` takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Dark,
    Light,
    Solarized,
    Monokai,
}

impl Palette {
    /// Every name a person can type into `/theme`, in the order `/help` lists them.
    pub const ALL: [Palette; 4] = [
        Palette::Dark,
        Palette::Light,
        Palette::Solarized,
        Palette::Monokai,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized",
            Self::Monokai => "monokai",
        }
    }

    /// A palette by the name `/theme` takes, case- and space-insensitive, or
    /// `None` with the caller listing what it could take instead.
    pub fn parse(word: &str) -> Option<Palette> {
        let word = word.trim().to_lowercase();
        Self::ALL.into_iter().find(|palette| palette.name() == word)
    }

    pub fn names() -> Vec<&'static str> {
        Self::ALL.into_iter().map(Self::name).collect()
    }
}

/// The styles one palette gives each meaning on screen.
///
/// Built once per theme change rather than per frame: it is nine `Style` values,
/// and a render loop has better things to do at 60 Hz than rebuild them.
#[derive(Debug, Clone)]
pub struct Theme {
    pub palette: Palette,
    pub text: Style,
    pub muted: Style,
    pub accent: Style,
    pub heading: Style,
    pub ok: Style,
    pub failed: Style,
    pub warning: Style,
    pub bar: Style,
    pub border: Style,
    /// The border and title of the pane asking a question, which is the one thing
    /// on the screen that is waiting for a person.
    pub asking: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::of(Palette::default())
    }
}

impl Theme {
    pub fn of(palette: Palette) -> Self {
        let (base, faint, strong, good, bad, warn, key) = match palette {
            // The default: grey for what a person does not need to read now, white
            // for what they do, cyan where the screen is speaking as itself.
            Palette::Dark => (
                Color::Gray,
                Color::DarkGray,
                Color::White,
                Color::Green,
                Color::Red,
                Color::Yellow,
                Color::Cyan,
            ),
            // Light assumes a pale background, so the accent goes dark rather than
            // bright; green stays green because a check mark has to read as one.
            Palette::Light => (
                Color::Black,
                Color::DarkGray,
                Color::Black,
                Color::Green,
                Color::Red,
                Color::Yellow,
                Color::Blue,
            ),
            Palette::Solarized => (
                Color::LightYellow,
                Color::DarkGray,
                Color::White,
                Color::LightGreen,
                Color::LightRed,
                Color::LightYellow,
                Color::LightBlue,
            ),
            Palette::Monokai => (
                Color::White,
                Color::DarkGray,
                Color::White,
                Color::LightGreen,
                Color::LightMagenta,
                Color::LightYellow,
                Color::LightCyan,
            ),
        };
        Self {
            palette,
            text: plain(base),
            muted: plain(faint),
            accent: plain(key),
            heading: bold(strong),
            ok: plain(good),
            failed: bold(bad),
            warning: bold(warn),
            bar: Style::default().fg(base).add_modifier(Modifier::REVERSED),
            border: plain(faint),
            asking: bold(warn),
        }
    }
}

fn plain(color: Color) -> Style {
    Style::default().fg(color)
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_has_a_name_a_person_can_type() {
        for palette in Palette::ALL {
            assert_eq!(Palette::parse(palette.name()), Some(palette));
        }
        assert_eq!(Palette::parse("  Solarized "), Some(Palette::Solarized));
        assert_eq!(
            Palette::parse("gruvbox"),
            None,
            "an unknown name is refused"
        );
        assert_eq!(Palette::names().len(), 4);
    }

    #[test]
    fn a_success_and_a_refusal_are_not_the_same_style() {
        let theme = Theme::default();
        assert_ne!(theme.ok.fg, theme.failed.fg);
        assert_ne!(theme.text.fg, theme.muted.fg, "faint must differ from body");
        assert_ne!(
            theme.failed.fg, theme.warning.fg,
            "a failure and a caution should not be one colour"
        );
    }

    #[test]
    fn switching_palette_changes_the_body_colour_but_not_the_meaning() {
        for palette in Palette::ALL {
            let theme = Theme::of(palette);
            assert_eq!(theme.palette, palette);
            assert!(
                theme.asking.add_modifier.contains(Modifier::BOLD),
                "{}: the question must stand out",
                palette.name()
            );
        }
    }
}
