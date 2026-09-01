//! Terminal styling for the command line interface.
//!
//! Colour is decided once and cached. A debate streams two voices into one
//! terminal, so distinguishing them by colour is what makes the transcript
//! readable as it arrives rather than something to untangle afterwards.
//!
//! Styling is suppressed when output is redirected, so piping a debate to a
//! file or another program yields clean text rather than escape sequences.

use std::io::IsTerminal;
use std::sync::OnceLock;

use crate::debate::state::Side;

/// Cached result of [`colors_enabled`].
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Escape sequence returning the terminal to its default appearance.
const RESET: &str = "\x1b[0m";

/// A visual role, mapped to an escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Side A's argument.
    SideA,
    /// Side B's argument.
    SideB,
    /// Supporting detail such as tool invocations.
    Dim,
    /// Section headings.
    Heading,
    /// Values worth picking out, such as a confidence reading.
    Value,
}

impl Style {
    /// The escape sequence introducing this style.
    fn code(self) -> &'static str {
        match self {
            Self::SideA => "\x1b[36m",
            Self::SideB => "\x1b[35m",
            Self::Dim => "\x1b[2m",
            Self::Heading => "\x1b[1m",
            Self::Value => "\x1b[33m",
        }
    }

    /// The style used for a debater's argument.
    pub fn for_side(side: Side) -> Self {
        match side {
            Side::A => Self::SideA,
            Side::B => Self::SideB,
        }
    }
}

/// Decide whether to emit escape sequences.
///
/// Split from the environment lookup so the policy can be tested without
/// mutating process-wide state.
fn should_colorize(no_color: Option<&str>, force: Option<&str>, is_tty: bool) -> bool {
    // NO_COLOR is honoured whenever it is present and non-empty, per the
    // informal convention at no-color.org.
    if no_color.is_some_and(|value| !value.is_empty()) {
        return false;
    }
    if force.is_some_and(|value| value == "1") {
        return true;
    }
    is_tty
}

/// Whether styling is active for this process.
///
/// # Returns
///
/// `true` when standard output is a terminal and `NO_COLOR` is unset, or when
/// `CLICOLOR_FORCE` is `1`.
pub fn colors_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        should_colorize(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("CLICOLOR_FORCE").ok().as_deref(),
            std::io::stdout().is_terminal(),
        )
    })
}

/// Wrap text in a style, or return it unchanged when styling is off.
///
/// # Arguments
///
/// * `style` - The visual role to apply
/// * `text` - The text to wrap
///
/// # Returns
///
/// The styled string, or the original text when styling is disabled.
///
/// # Examples
///
/// ```
/// # use coin::term::{paint, Style};
/// // With output redirected, styling is suppressed.
/// assert_eq!(paint(Style::Dim, "plain"), "plain");
/// ```
pub fn paint<S>(style: Style, text: S) -> String
where
    S: AsRef<str>,
{
    let text = text.as_ref();
    if colors_enabled() {
        format!("{}{text}{RESET}", style.code())
    } else {
        text.to_string()
    }
}

/// Open a style that later text inherits, without closing it.
///
/// Streamed output arrives one token at a time. Wrapping each fragment with
/// [`paint`] would emit an escape pair per token, bloating the transcript for
/// no visual gain. Opening the style once per turn and closing it with
/// [`reset`] produces the same appearance from two sequences.
///
/// # Arguments
///
/// * `style` - The visual role to open
///
/// # Returns
///
/// The opening sequence, or an empty string when styling is disabled.
pub fn start(style: Style) -> &'static str {
    if colors_enabled() { style.code() } else { "" }
}

/// Close any style opened by [`start`].
///
/// # Returns
///
/// The reset sequence, or an empty string when styling is disabled.
pub fn reset() -> &'static str {
    if colors_enabled() { RESET } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_gets_colour_by_default() {
        assert!(should_colorize(None, None, true));
    }

    #[test]
    fn redirected_output_is_left_plain() {
        // Piping a debate to a file must not embed escape sequences.
        assert!(!should_colorize(None, None, false));
    }

    #[test]
    fn no_color_overrides_a_terminal() {
        assert!(!should_colorize(Some("1"), None, true));
    }

    #[test]
    fn an_empty_no_color_is_ignored() {
        // The convention treats only a non-empty value as set.
        assert!(should_colorize(Some(""), None, true));
    }

    #[test]
    fn clicolor_force_overrides_redirection() {
        assert!(should_colorize(None, Some("1"), false));
    }

    #[test]
    fn no_color_beats_clicolor_force() {
        // Suppression is the safer default when the two conflict.
        assert!(!should_colorize(Some("1"), Some("1"), true));
    }

    #[test]
    fn start_and_reset_agree_with_paint() {
        // The streaming path must render identically to the wrapped path.
        let combined = format!("{}{}{}", start(Style::SideA), "text", reset());
        assert_eq!(combined, paint(Style::SideA, "text"));
    }

    #[test]
    fn the_two_sides_are_visually_distinct() {
        // The whole point is telling the debaters apart at a glance.
        assert_ne!(
            Style::for_side(Side::A).code(),
            Style::for_side(Side::B).code()
        );
    }

    #[test]
    fn every_style_emits_a_distinct_sequence() {
        let styles = [
            Style::SideA,
            Style::SideB,
            Style::Dim,
            Style::Heading,
            Style::Value,
        ];
        for (index, style) in styles.iter().enumerate() {
            for other in styles.iter().skip(index + 1) {
                assert_ne!(style.code(), other.code(), "{style:?} matches {other:?}");
            }
        }
    }
}
