//! The one loading marker of the interface.
//!
//! A read that runs in the background shows a spinner in the place of the data
//! it will bring. The frame is not taken from a clock: the application holds a
//! counter ([`super::task_app::TaskApp::animation_frame`]) that the loop raises
//! on each tick while a read runs, so a test picks a frame by setting it.
//!
//! The marker is always a glyph and a word ("⠋ wczytuję…"), so it reads without
//! colour. A terminal that does not draw braille gets four ASCII frames;
//! [`Spinner::from_env`] looks at the environment once, at start.

/// The braille frames.
const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The frames for a terminal without UTF-8 or braille.
const ASCII: [&str; 4] = ["|", "/", "-", "\\"];

/// The frames of the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spinner {
    frames: &'static [&'static str],
}

impl Default for Spinner {
    fn default() -> Self {
        Self::braille()
    }
}

impl Spinner {
    /// The braille frames.
    pub fn braille() -> Self {
        Self { frames: &BRAILLE }
    }

    /// The ASCII frames.
    pub fn ascii() -> Self {
        Self { frames: &ASCII }
    }

    /// Picks the frames from the environment, read once at start.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self::from_values(
            var("LC_ALL").as_deref(),
            var("LC_CTYPE").as_deref(),
            var("LANG").as_deref(),
            var("TERM").as_deref(),
        )
    }

    /// [`Self::from_env`] over the given values, so a test needs no change of
    /// the environment. The locale is the first of `lc_all`, `lc_ctype`,
    /// `lang` that is not empty; ASCII is chosen when it does not name UTF-8,
    /// and on the Linux console and a dumb terminal.
    pub fn from_values(
        lc_all: Option<&str>,
        lc_ctype: Option<&str>,
        lang: Option<&str>,
        term: Option<&str>,
    ) -> Self {
        let locale = [lc_all, lc_ctype, lang]
            .into_iter()
            .flatten()
            .find(|value| !value.is_empty());
        let utf8 = locale.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        });
        let plain_terminal = matches!(term, Some("linux" | "dumb"));
        if utf8 && !plain_terminal {
            Self::braille()
        } else {
            Self::ascii()
        }
    }

    /// How many frames the marker has.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Always false: a marker has frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The glyph of `frame`, wrapping round.
    pub fn glyph(&self, frame: usize) -> &'static str {
        self.frames[frame % self.frames.len()]
    }

    /// The glyph and `text`: what a panel shows in the place of its data.
    pub fn label(&self, frame: usize, text: &str) -> String {
        format!("{} {text}", self.glyph(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_differ_and_wrap() {
        let spinner = Spinner::braille();
        let glyphs: Vec<_> = (0..spinner.len()).map(|n| spinner.glyph(n)).collect();
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), spinner.len());
        assert_eq!(spinner.glyph(spinner.len()), spinner.glyph(0));
    }

    #[test]
    fn ascii_frames_are_ascii_and_braille_frames_are_not() {
        let ascii = Spinner::ascii();
        assert!((0..ascii.len()).all(|n| ascii.glyph(n).is_ascii()));
        let braille = Spinner::braille();
        assert!((0..braille.len()).all(|n| !braille.glyph(n).is_ascii()));
    }

    #[test]
    fn the_environment_picks_the_frames() {
        let utf8 = Spinner::from_values(None, None, Some("pl_PL.UTF-8"), Some("xterm"));
        assert_eq!(utf8, Spinner::braille());
        let c = Spinner::from_values(None, None, Some("C"), Some("xterm"));
        assert_eq!(c, Spinner::ascii());
        let all_wins = Spinner::from_values(Some("C"), None, Some("pl_PL.UTF-8"), None);
        assert_eq!(all_wins, Spinner::ascii());
        let linux = Spinner::from_values(None, None, Some("en_US.utf8"), Some("linux"));
        assert_eq!(linux, Spinner::ascii());
        let dumb = Spinner::from_values(None, None, Some("en_US.utf8"), Some("dumb"));
        assert_eq!(dumb, Spinner::ascii());
        let empty_skipped = Spinner::from_values(Some(""), None, Some("en_US.UTF-8"), None);
        assert_eq!(empty_skipped, Spinner::braille());
        let nothing = Spinner::from_values(None, None, None, None);
        assert_eq!(nothing, Spinner::ascii());
    }

    #[test]
    fn the_label_has_the_glyph_and_the_text() {
        let spinner = Spinner::ascii();
        assert_eq!(spinner.label(1, "wczytuję…"), "/ wczytuję…");
    }
}
