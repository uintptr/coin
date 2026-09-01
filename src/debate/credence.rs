//! The credence-updating format.
//!
//! Both sides carry an explicit 0-100 confidence and must restate it every
//! turn, justifying any movement. The debate ends when the two confidences
//! converge, which makes truth-seeking measurable rather than rhetorical and
//! gives the analysis rail its convergence chart.
//!
//! The prompt deliberately treats an unchanged confidence as something
//! requiring justification. Without that pressure a debater simply restates
//! its opening number every round and the format degenerates into two
//! monologues.

use crate::debate::format::{DebateFormat, FormatId, shared_mandate};
use crate::debate::parse::parse_turn;
use crate::debate::state::{Credence, DebateState, Side, StopReason, Topic, TurnAnalysis};

/// Confidence gap, in percentage points, at or below which the sides have
/// converged.
pub const CONVERGENCE_THRESHOLD: u8 = 15;

/// Minimum rounds before convergence is allowed to end the debate.
///
/// Two models handed a truth-seeking mandate will sometimes agree immediately
/// and produce a one-round debate with no argument in it. Requiring a second
/// round forces at least one exchange of evidence before agreement counts.
pub const MIN_ROUNDS_BEFORE_CONVERGENCE: usize = 2;

/// The credence-updating format.
#[derive(Debug, Clone, Copy, Default)]
pub struct CredenceFormat;

impl CredenceFormat {
    /// Instructions for the structured block, appended to every turn prompt.
    fn block_instructions() -> &'static str {
        "End your reply with a fenced json block, and nothing after it:\n\n\
         ```json\n\
         {\n\
         \x20 \"credence\": <integer 0-100>,\n\
         \x20 \"moved_because\": \"<why your confidence changed, or why it did not>\",\n\
         \x20 \"conceded\": [\"<points you now accept>\"],\n\
         \x20 \"key_claim\": \"<the single claim your case most depends on>\"\n\
         }\n\
         ```"
    }

    /// Describe the opponent's most recent turn, for inclusion in a prompt.
    fn opponent_summary(state: &DebateState, side: Side) -> String {
        match state.last_turn(side.other()) {
            None => String::new(),
            Some(turn) => {
                let credence = turn
                    .analysis
                    .credence
                    .map_or_else(|| "not stated".to_string(), |value| value.to_string());
                format!(
                    "DEBATER {opponent} JUST ARGUED (their confidence: {credence})\n{text}\n\n",
                    opponent = side.other(),
                    credence = credence,
                    text = turn.text,
                )
            }
        }
    }
}

impl DebateFormat for CredenceFormat {
    fn id(&self) -> FormatId {
        FormatId::Credence
    }

    fn system_prompt(&self, side: Side, topic: &Topic) -> String {
        format!(
            "{mandate}\n\n\
             FORMAT: CREDENCE UPDATING\n\
             You carry an explicit confidence from 0 to 100 that your assigned \
             position is correct. State it every turn.\n\n\
             Your confidence must respond to the argument. If the opposing side \
             makes a point you cannot answer, your confidence should fall, and \
             you must say so. If it stays the same, justify why the opposing \
             argument failed to move it. Restating the same number every round \
             without reason is the main way this format fails.\n\n\
             An honest confidence is worth more than a high one. Two debaters \
             who converge on the truth have both succeeded.",
            mandate = shared_mandate(side, topic),
        )
    }

    fn turn_prompt(&self, state: &DebateState, side: Side) -> String {
        let round = state.current_round();

        // The opening turn has nothing to respond to.
        if state.turns.is_empty() {
            return format!(
                "Round {round}. Open the debate.\n\n\
                 State your case for your assigned position as strongly as the \
                 evidence allows, and give your starting confidence.\n\n{instructions}",
                round = round,
                instructions = Self::block_instructions(),
            );
        }

        let own_credence = state.latest_credence(side).map_or_else(
            || "you have not stated one yet".to_string(),
            |value| format!("{value}"),
        );

        format!(
            "{opponent}\
             Round {round}. Your previous confidence was {own_credence}.\n\n\
             Respond to their argument. Address their strongest point directly \
             rather than the weakest one. Then restate your confidence, and \
             explain what moved it or why nothing did.\n\n{instructions}",
            opponent = Self::opponent_summary(state, side),
            round = round,
            own_credence = own_credence,
            instructions = Self::block_instructions(),
        )
    }

    fn parse_turn(&self, raw: &str) -> (String, TurnAnalysis) {
        parse_turn(raw)
    }

    fn should_stop(&self, state: &DebateState) -> Option<StopReason> {
        // A side that concedes outright ends the debate regardless of numbers.
        if let Some(turn) = state.last_turn_any()
            && let Some(credence) = turn.analysis.credence
            && credence.value() == 0
        {
            return Some(StopReason::Conceded { side: turn.side });
        }

        if state.completed_rounds() < MIN_ROUNDS_BEFORE_CONVERGENCE {
            return None;
        }

        let (Some(a), Some(b)) = (
            state.latest_credence(Side::A),
            state.latest_credence(Side::B),
        ) else {
            return None;
        };

        // Each side reports confidence in its own position, so agreement means
        // the two sum to about 100, not that they are numerically close. See
        // Credence::agreement_gap.
        let gap = Credence::agreement_gap(a, b);
        (gap <= CONVERGENCE_THRESHOLD).then_some(StopReason::Converged { gap })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debate::state::ParseStatus;
    use crate::opencode::types::Tokens;

    /// Build a state whose turns carry the given credences, A first.
    fn state_with(credences: &[u8]) -> DebateState {
        let mut state = DebateState::new(Topic::new("q", "yes", "no"), 6);
        for (index, value) in credences.iter().enumerate() {
            let side = if index % 2 == 0 { Side::A } else { Side::B };
            let mut analysis = TurnAnalysis::empty(ParseStatus::Ok);
            analysis.credence = Credence::new(*value);
            state.push_turn(
                side,
                format!("argument {index}"),
                analysis,
                Vec::new(),
                Tokens::default(),
                0.0,
            );
        }
        state
    }

    #[test]
    fn converged_confidences_stop_the_debate() {
        // Arrange: A ends at 40 confident in A, B at 55 confident in B. Each
        // side's view of A's position is then 40 and 45: near agreement.
        let state = state_with(&[85, 20, 40, 55]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Converged { gap: 5 })
        );
    }

    #[test]
    fn a_wide_gap_does_not_stop_the_debate() {
        // Arrange: both sides remain confident in their own position, which is
        // the definition of disagreement. 80 + 75 is far from 100.
        let state = state_with(&[85, 80, 80, 75]);

        // Act and assert
        assert_eq!(CredenceFormat.should_stop(&state), None);
    }

    #[test]
    fn one_side_conceding_to_the_other_counts_as_agreement() {
        // Arrange: taken from a real run. A verified the facts, found they
        // contradicted its assigned position, and dropped to 3 while B rose to
        // 99. Both now believe B's position, which is total agreement.
        //
        // Comparing the raw numbers would report a 96 point gap and call this
        // maximum disagreement, at the exact moment the debate had succeeded.
        let state = state_with(&[85, 20, 3, 99]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Converged { gap: 2 })
        );
    }

    #[test]
    fn two_sides_equally_confident_in_opposite_positions_have_not_converged() {
        // Arrange: numerically identical credences, maximum real disagreement.
        // The inverse of the case above, and the reason a naive equality test
        // is wrong in both directions.
        let state = state_with(&[90, 90, 90, 90]);

        // Act and assert
        assert_eq!(CredenceFormat.should_stop(&state), None);
    }

    #[test]
    fn early_agreement_does_not_end_the_debate_in_one_round() {
        // Arrange: both sides agree immediately, which would otherwise produce
        // a debate containing no actual exchange of evidence.
        let state = state_with(&[50, 50]);

        // Act and assert
        assert_eq!(CredenceFormat.should_stop(&state), None);
    }

    #[test]
    fn agreement_counts_once_a_second_round_has_happened() {
        // Arrange: the same agreement, one round later.
        let state = state_with(&[50, 50, 50, 50]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Converged { gap: 0 })
        );
    }

    #[test]
    fn the_threshold_boundary_counts_as_converged() {
        // Arrange: 50 and 65 sum to 115, exactly the threshold away from 100.
        let state = state_with(&[50, 65, 50, 65]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Converged {
                gap: CONVERGENCE_THRESHOLD
            })
        );
    }

    #[test]
    fn one_point_beyond_the_threshold_does_not_converge() {
        // Arrange: 50 and 66 sum to 116, one point too far.
        let state = state_with(&[50, 66, 50, 66]);

        // Act and assert
        assert_eq!(CredenceFormat.should_stop(&state), None);
    }

    #[test]
    fn a_credence_of_zero_is_treated_as_conceding() {
        // Arrange: side B abandons its position outright.
        let state = state_with(&[85, 20, 84, 0]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Conceded { side: Side::B })
        );
    }

    #[test]
    fn a_concession_ends_the_debate_even_in_the_first_round() {
        // Arrange: the minimum-rounds guard must not suppress a real
        // concession, which is a genuine resolution rather than false agreement.
        let state = state_with(&[85, 0]);

        // Act and assert
        assert_eq!(
            CredenceFormat.should_stop(&state),
            Some(StopReason::Conceded { side: Side::B })
        );
    }

    #[test]
    fn missing_credences_never_stop_the_debate() {
        // Arrange: neither side stated a readable confidence.
        let mut state = DebateState::new(Topic::new("q", "yes", "no"), 6);
        for index in 0..4 {
            let side = if index % 2 == 0 { Side::A } else { Side::B };
            state.push_turn(
                side,
                "prose only".into(),
                TurnAnalysis::empty(ParseStatus::Missing),
                Vec::new(),
                Tokens::default(),
                0.0,
            );
        }

        // Act and assert: the round cap will end it instead.
        assert_eq!(CredenceFormat.should_stop(&state), None);
    }

    #[test]
    fn the_opening_prompt_asks_for_a_starting_confidence() {
        // Arrange
        let state = DebateState::new(Topic::new("q", "yes", "no"), 6);

        // Act
        let prompt = CredenceFormat.turn_prompt(&state, Side::A);

        // Assert
        assert!(prompt.contains("Open the debate"));
        assert!(prompt.contains("starting confidence"));
        assert!(prompt.contains("```json"));
    }

    #[test]
    fn later_prompts_quote_the_opponent_and_the_previous_confidence() {
        // Arrange
        let state = state_with(&[85]);

        // Act
        let prompt = CredenceFormat.turn_prompt(&state, Side::B);

        // Assert
        assert!(prompt.contains("DEBATER A JUST ARGUED"));
        assert!(prompt.contains("their confidence: 85"));
        assert!(prompt.contains("argument 0"));
    }

    #[test]
    fn the_system_prompt_pushes_back_on_unchanged_confidence() {
        // Arrange
        let topic = Topic::new("q", "yes", "no");

        // Act
        let prompt = CredenceFormat.system_prompt(Side::A, &topic);

        // Assert: this is the format's main failure mode, so it is called out.
        assert!(prompt.contains("Restating the same number"));
    }
}
