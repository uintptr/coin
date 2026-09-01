//! The debate format abstraction.
//!
//! Format is chosen per debate rather than fixed, because the right structure
//! depends on the question: an empirical dispute with a checkable answer wants
//! credence tracking, a definitional one wants classic rounds. Each format
//! implements [`DebateFormat`], so adding a fifth is purely additive.

use serde::{Deserialize, Serialize};

use crate::debate::state::{DebateState, Side, StopReason, Topic, TurnAnalysis};

/// Which format a debate is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FormatId {
    /// Narrow toward the single load-bearing disagreement.
    Crux,
    /// Track and justify a stated confidence each turn.
    Credence,
    /// Opening, rebuttal, cross-examination, closing.
    Classic,
    /// Accumulate claims into a shared ledger.
    Ledger,
}

impl FormatId {
    /// Every format, for populating a picker.
    pub const ALL: [Self; 4] = [Self::Crux, Self::Credence, Self::Classic, Self::Ledger];

    /// Stable identifier used in configuration and on the wire.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Crux => "crux",
            Self::Credence => "credence",
            Self::Classic => "classic",
            Self::Ledger => "ledger",
        }
    }

    /// One-line description of how the format ends.
    pub fn stop_description(self) -> &'static str {
        match self {
            Self::Crux => "both sides name the same crux",
            Self::Credence => "stated confidences converge",
            Self::Classic => "after closing statements",
            Self::Ledger => "a round introduces no new claims",
        }
    }
}

impl std::str::FromStr for FormatId {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|format| format.slug() == value.to_lowercase())
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|f| f.slug()).collect();
                format!(
                    "unknown format {value:?}; expected one of {}",
                    names.join(", ")
                )
            })
    }
}

impl std::fmt::Display for FormatId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.slug())
    }
}

/// The truth-seeking mandate shared by every format.
///
/// This is the heart of the project: the debaters are told that being right
/// matters more than winning, and that conceding is a success rather than a
/// loss. Formats append their own structural instructions to it.
///
/// # Arguments
///
/// * `side` - Which side the debater is arguing
/// * `topic` - The proposition and both positions
///
/// # Returns
///
/// The shared portion of the system prompt.
pub fn shared_mandate(side: Side, topic: &Topic) -> String {
    format!(
        "You are Debater {side} in a structured debate.\n\n\
         QUESTION UNDER DISPUTE\n{question}\n\n\
         YOUR ASSIGNED POSITION\n{yours}\n\n\
         THE OPPOSING POSITION\n{theirs}\n\n\
         YOUR ACTUAL GOAL\n\
         This is not a contest. The goal is to determine what is actually true. \
         You have been assigned a position to argue, but you are not required to \
         defend it past the point where the evidence stops supporting it.\n\n\
         Rules that matter more than winning:\n\
         - Conceding a point you cannot support is a success, not a loss.\n\
         - If the evidence turns against your assigned position, say so plainly.\n\
         - Never manufacture evidence, statistics, citations, or quotations.\n\
         - Attack the strongest version of the opposing case, not a weak one.\n\
         - Distinguish what you know from what you are inferring.\n\
         - You have tools available. Use them to check claims rather than \
         asserting from memory. A verified fact outranks a confident assertion, \
         including your own.\n\n\
         Write in plain prose. Be concise: a few short paragraphs at most. Do \
         not use headings or bullet lists.",
        side = side,
        question = topic.question,
        yours = topic.position(side),
        theirs = topic.position(side.other()),
    )
}

/// A debate format: how turns are structured and when the debate ends.
pub trait DebateFormat: Send + Sync {
    /// Which format this is.
    fn id(&self) -> FormatId;

    /// System prompt establishing the persona and the mandate for one side.
    ///
    /// # Arguments
    ///
    /// * `side` - Which side the debater argues
    /// * `topic` - The proposition and both positions
    ///
    /// # Returns
    ///
    /// The complete system prompt for that side's agent.
    fn system_prompt(&self, side: Side, topic: &Topic) -> String;

    /// Prompt for this side's next turn.
    ///
    /// # Arguments
    ///
    /// * `state` - Everything that has happened so far
    /// * `side` - Which side is about to speak
    ///
    /// # Returns
    ///
    /// The message to send for the next turn.
    fn turn_prompt(&self, state: &DebateState, side: Side) -> String;

    /// Extract format-specific structure from a completed turn.
    ///
    /// # Arguments
    ///
    /// * `raw` - The model's full reply
    ///
    /// # Returns
    ///
    /// The prose with any structured block removed, and the analysis.
    fn parse_turn(&self, raw: &str) -> (String, TurnAnalysis);

    /// Whether the debate has reached its natural end.
    ///
    /// The engine applies the hard round cap separately, so a format need only
    /// express its own stop condition.
    ///
    /// # Arguments
    ///
    /// * `state` - Everything that has happened so far
    ///
    /// # Returns
    ///
    /// `Some` with the reason once the format is finished.
    fn should_stop(&self, state: &DebateState) -> Option<StopReason>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_slugs_round_trip_through_parsing() {
        // Act and assert
        for format in FormatId::ALL {
            let parsed: FormatId = format
                .slug()
                .parse()
                .expect("every slug must parse back to its format");
            assert_eq!(parsed, format);
        }
    }

    #[test]
    fn format_parsing_is_case_insensitive() {
        // Act and assert
        assert_eq!("CREDENCE".parse::<FormatId>(), Ok(FormatId::Credence));
    }

    #[test]
    fn an_unknown_format_lists_the_valid_ones() {
        // Act
        let error = "socratic".parse::<FormatId>().expect_err("must reject");

        // Assert: the message has to be actionable at a CLI.
        assert!(error.contains("credence"), "got {error}");
        assert!(error.contains("socratic"), "got {error}");
    }

    #[test]
    fn the_mandate_names_both_positions_and_forbids_fabrication() {
        // Arrange
        let topic = Topic::new("Is X true?", "X is true", "X is false");

        // Act
        let prompt = shared_mandate(Side::A, &topic);

        // Assert
        assert!(prompt.contains("X is true"));
        assert!(prompt.contains("X is false"));
        assert!(prompt.contains("Never manufacture"));
    }

    #[test]
    fn the_mandate_gives_each_side_its_own_position() {
        // Arrange
        let topic = Topic::new("q", "first case", "second case");

        // Act
        let for_a = shared_mandate(Side::A, &topic);
        let for_b = shared_mandate(Side::B, &topic);

        // Assert: the assigned position appears before the opposing one.
        let a_yours = for_a.find("first case").expect("A must see its position");
        let a_theirs = for_a
            .find("second case")
            .expect("A must see the opposition");
        assert!(a_yours < a_theirs);

        let b_yours = for_b.find("second case").expect("B must see its position");
        let b_theirs = for_b.find("first case").expect("B must see the opposition");
        assert!(b_yours < b_theirs);
    }
}
