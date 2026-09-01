//! Core debate data model.
//!
//! [`DebateState`] is the single source of truth for a running debate. Formats
//! read it to build the next prompt and to decide whether to stop; the web
//! layer serializes it as the snapshot described in `PROJECT_SPECS.md`
//! section 9.3.

use serde::{Deserialize, Serialize};

use crate::opencode::types::{Tokens, ToolCall};

/// Which side of the argument a turn belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The side arguing the first position.
    A,
    /// The side arguing the second position.
    B,
}

impl Side {
    /// The opposing side.
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// Short lowercase label, used for agent names and identifiers.
    pub fn label(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::A => write!(formatter, "A"),
            Self::B => write!(formatter, "B"),
        }
    }
}

/// The proposition under dispute and the two assigned positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    /// The question being settled.
    pub question: String,
    /// The case assigned to side A.
    pub position_a: String,
    /// The case assigned to side B.
    pub position_b: String,
}

impl Topic {
    /// Build a topic from a question and both positions.
    ///
    /// # Arguments
    ///
    /// * `question` - The proposition under dispute
    /// * `position_a` - Case assigned to side A
    /// * `position_b` - Case assigned to side B
    ///
    /// # Returns
    ///
    /// The assembled topic.
    pub fn new<Q, A, B>(question: Q, position_a: A, position_b: B) -> Self
    where
        Q: Into<String>,
        A: Into<String>,
        B: Into<String>,
    {
        Self {
            question: question.into(),
            position_a: position_a.into(),
            position_b: position_b.into(),
        }
    }

    /// The position assigned to the given side.
    pub fn position(&self, side: Side) -> &str {
        match side {
            Side::A => &self.position_a,
            Side::B => &self.position_b,
        }
    }
}

/// A confidence value between 0 and 100 inclusive.
///
/// A newtype rather than a bare `u8` so an out-of-range model response is
/// rejected at construction instead of corrupting the convergence chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Credence(u8);

impl Credence {
    /// Largest permitted value.
    pub const MAX: u8 = 100;

    /// Build a credence, rejecting values above 100.
    ///
    /// # Arguments
    ///
    /// * `value` - Confidence as a percentage
    ///
    /// # Returns
    ///
    /// `Some` when in range, `None` otherwise.
    pub fn new(value: u8) -> Option<Self> {
        (value <= Self::MAX).then_some(Self(value))
    }

    /// Clamp a signed model-supplied value into range.
    ///
    /// Models occasionally emit values slightly outside 0-100. Clamping keeps
    /// a debate running where rejection would strand it.
    ///
    /// # Arguments
    ///
    /// * `value` - Raw value from a model
    ///
    /// # Returns
    ///
    /// The nearest valid credence.
    pub fn clamped(value: i64) -> Self {
        Self(value.clamp(0, i64::from(Self::MAX)) as u8)
    }

    /// The underlying percentage.
    pub fn value(self) -> u8 {
        self.0
    }

    /// Absolute distance from another credence, in percentage points.
    pub fn distance(self, other: Self) -> u8 {
        self.0.abs_diff(other.0)
    }

    /// How far apart two sides are, given each states confidence in its **own**
    /// assigned position.
    ///
    /// The two credences are not measured against the same proposition: side A
    /// reports confidence that A is right, side B that B is right. When the
    /// sides agree, one number is near 0 and the other near 100, so comparing
    /// them directly reports maximum disagreement at the exact moment they have
    /// converged. Restating B's confidence on A's proposition is `100 - b`,
    /// which makes the gap `|a + b - 100|`.
    ///
    /// # Arguments
    ///
    /// * `a` - Side A's confidence in side A's position
    /// * `b` - Side B's confidence in side B's position
    ///
    /// # Returns
    ///
    /// The gap in percentage points, where 0 is complete agreement.
    ///
    /// # Examples
    ///
    /// ```
    /// # use coin::debate::state::Credence;
    /// let a = Credence::clamped(3);
    /// let b = Credence::clamped(99);
    /// // Both believe B's position: near-complete agreement.
    /// assert_eq!(Credence::agreement_gap(a, b), 2);
    /// ```
    pub fn agreement_gap(a: Self, b: Self) -> u8 {
        let total = i16::from(a.0) + i16::from(b.0);
        total.abs_diff(100).min(u8::MAX.into()) as u8
    }
}

impl std::fmt::Display for Credence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// How a claim stands between the two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimKind {
    /// Both sides accept it.
    Agreed,
    /// The sides disagree about it.
    Disputed,
    /// Raised but not yet addressed.
    Unresolved,
}

/// A discrete claim extracted from a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The claim itself.
    pub text: String,
    /// Its standing between the sides.
    pub kind: ClaimKind,
}

/// Whether a turn's structured block could be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ParseStatus {
    /// The block was present and understood.
    Ok,
    /// No structured block was present.
    Missing,
    /// A block was present but could not be read.
    Malformed {
        /// Why it could not be read.
        reason: String,
    },
}

impl ParseStatus {
    /// Whether the structured block was successfully read.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Structure extracted from a turn, over and above its prose.
///
/// Every field is optional. A turn whose structured block is missing or
/// malformed still counts as a turn: the debate degrades to prose rather than
/// failing, as specified in `PROJECT_SPECS.md` section 6.
#[derive(Debug, Clone, Serialize)]
pub struct TurnAnalysis {
    /// Stated confidence, for formats that track it.
    pub credence: Option<Credence>,
    /// The side's justification for any movement in confidence.
    pub moved_because: Option<String>,
    /// Points explicitly conceded this turn.
    pub conceded: Vec<String>,
    /// The claim this side's case most depends on.
    pub key_claim: Option<String>,
    /// The disagreement the side believes the dispute reduces to.
    pub crux: Option<String>,
    /// Claims extracted for the ledger format.
    pub claims: Vec<Claim>,
    /// Whether the structured block was readable.
    pub parse_status: ParseStatus,
}

impl TurnAnalysis {
    /// An analysis carrying no structure, for a turn with no readable block.
    ///
    /// # Arguments
    ///
    /// * `parse_status` - Why no structure is present
    ///
    /// # Returns
    ///
    /// An otherwise empty analysis.
    pub fn empty(parse_status: ParseStatus) -> Self {
        Self {
            credence: None,
            moved_because: None,
            conceded: Vec::new(),
            key_claim: None,
            crux: None,
            claims: Vec::new(),
            parse_status,
        }
    }
}

/// One completed turn by one side.
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    /// Position in the debate, counting from zero.
    pub index: usize,
    /// Round this turn belongs to, counting from one.
    pub round: usize,
    /// Which side spoke.
    pub side: Side,
    /// The visible argument, with any structured block stripped.
    pub text: String,
    /// Structure extracted from the turn.
    pub analysis: TurnAnalysis,
    /// Tools invoked while producing the turn.
    pub tool_calls: Vec<ToolCall>,
    /// Tokens consumed across every message in the turn.
    pub tokens: Tokens,
    /// Cost in USD across every message in the turn.
    pub cost: f64,
}

/// Why a debate ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    /// The sides' stated confidences came within the format's threshold.
    Converged {
        /// Final gap in percentage points.
        gap: u8,
    },
    /// A side conceded the question.
    Conceded {
        /// The side that conceded.
        side: Side,
    },
    /// Both sides named the same crux.
    CruxIsolated,
    /// A full round introduced no new claims.
    NoNewClaims,
    /// The format's scripted rounds finished.
    FormatComplete,
    /// The hard round cap was reached.
    RoundCap,
    /// The operator stopped it.
    Aborted,
}

/// Everything known about a debate in progress.
#[derive(Debug, Clone, Serialize)]
pub struct DebateState {
    /// The proposition and both positions.
    pub topic: Topic,
    /// Hard cap on rounds, regardless of format.
    pub max_rounds: usize,
    /// Completed turns, in order.
    pub turns: Vec<Turn>,
    /// Why the debate ended, once it has.
    pub stop_reason: Option<StopReason>,
}

impl DebateState {
    /// Begin a debate with no turns taken.
    ///
    /// # Arguments
    ///
    /// * `topic` - The proposition and both positions
    /// * `max_rounds` - Hard cap on rounds
    ///
    /// # Returns
    ///
    /// A state ready for the first turn.
    pub fn new(topic: Topic, max_rounds: usize) -> Self {
        Self {
            topic,
            max_rounds,
            turns: Vec::new(),
            stop_reason: None,
        }
    }

    /// The round the next turn belongs to, counting from one.
    ///
    /// A round is one turn from each side, so it advances every two turns.
    pub fn current_round(&self) -> usize {
        self.turns.len() / 2 + 1
    }

    /// Number of complete rounds, where both sides have spoken.
    pub fn completed_rounds(&self) -> usize {
        self.turns.len() / 2
    }

    /// Which side speaks next. Side A opens.
    pub fn next_side(&self) -> Side {
        if self.turns.len().is_multiple_of(2) {
            Side::A
        } else {
            Side::B
        }
    }

    /// The most recent turn taken by the given side.
    pub fn last_turn(&self, side: Side) -> Option<&Turn> {
        self.turns.iter().rev().find(|turn| turn.side == side)
    }

    /// The most recent turn by either side.
    pub fn last_turn_any(&self) -> Option<&Turn> {
        self.turns.last()
    }

    /// The most recent credence stated by the given side.
    pub fn latest_credence(&self, side: Side) -> Option<Credence> {
        self.turns
            .iter()
            .rev()
            .filter(|turn| turn.side == side)
            .find_map(|turn| turn.analysis.credence)
    }

    /// Every credence stated by the given side, oldest first.
    ///
    /// This is the series behind the convergence chart.
    pub fn credence_series(&self, side: Side) -> Vec<Credence> {
        self.turns
            .iter()
            .filter(|turn| turn.side == side)
            .filter_map(|turn| turn.analysis.credence)
            .collect()
    }

    /// Total cost across every turn, in USD.
    pub fn total_cost(&self) -> f64 {
        self.turns.iter().map(|turn| turn.cost).sum()
    }

    /// Total tokens across every turn.
    pub fn total_tokens(&self) -> Tokens {
        self.turns
            .iter()
            .fold(Tokens::default(), |mut total, turn| {
                total.input += turn.tokens.input;
                total.output += turn.tokens.output;
                total.reasoning += turn.tokens.reasoning;
                total
            })
    }

    /// Record a completed turn.
    ///
    /// # Arguments
    ///
    /// * `side` - Which side spoke
    /// * `text` - The visible argument
    /// * `analysis` - Structure extracted from it
    /// * `tool_calls` - Tools invoked while producing it
    /// * `tokens` - Tokens consumed
    /// * `cost` - Cost in USD
    ///
    /// # Returns
    ///
    /// The index assigned to the new turn.
    pub fn push_turn(
        &mut self,
        side: Side,
        text: String,
        analysis: TurnAnalysis,
        tool_calls: Vec<ToolCall>,
        tokens: Tokens,
        cost: f64,
    ) -> usize {
        let index = self.turns.len();
        let round = index / 2 + 1;
        self.turns.push(Turn {
            index,
            round,
            side,
            text,
            analysis,
            tool_calls,
            tokens,
            cost,
        });
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a state with the given credences applied in turn order.
    fn state_with_credences(values: &[Option<u8>]) -> DebateState {
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 6);
        for (index, value) in values.iter().enumerate() {
            let side = if index % 2 == 0 { Side::A } else { Side::B };
            let mut analysis = TurnAnalysis::empty(ParseStatus::Ok);
            analysis.credence = value.and_then(Credence::new);
            state.push_turn(
                side,
                format!("turn {index}"),
                analysis,
                Vec::new(),
                Tokens::default(),
                0.001,
            );
        }
        state
    }

    #[test]
    fn side_a_opens_and_sides_alternate() {
        // Arrange
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 6);

        // Act and assert
        assert_eq!(state.next_side(), Side::A);
        state.push_turn(
            Side::A,
            "x".into(),
            TurnAnalysis::empty(ParseStatus::Ok),
            Vec::new(),
            Tokens::default(),
            0.0,
        );
        assert_eq!(state.next_side(), Side::B);
    }

    #[test]
    fn a_round_is_one_turn_from_each_side() {
        // Arrange: three turns means one complete round and a second in flight.
        let state = state_with_credences(&[Some(80), Some(20), Some(70)]);

        // Assert
        assert_eq!(state.completed_rounds(), 1);
        assert_eq!(state.current_round(), 2);
        assert_eq!(state.turns[0].round, 1);
        assert_eq!(state.turns[1].round, 1);
        assert_eq!(state.turns[2].round, 2);
    }

    #[test]
    fn latest_credence_skips_turns_that_stated_none() {
        // Arrange: side A's most recent turn carries no credence.
        let state = state_with_credences(&[Some(80), Some(20), None, Some(35)]);

        // Act and assert: it falls back to the last one that did.
        assert_eq!(
            state.latest_credence(Side::A).map(Credence::value),
            Some(80)
        );
        assert_eq!(
            state.latest_credence(Side::B).map(Credence::value),
            Some(35)
        );
    }

    #[test]
    fn credence_series_preserves_order_for_charting() {
        // Arrange
        let state = state_with_credences(&[Some(85), Some(20), Some(71), Some(52)]);

        // Act
        let series: Vec<u8> = state
            .credence_series(Side::A)
            .iter()
            .map(|credence| credence.value())
            .collect();

        // Assert
        assert_eq!(series, vec![85, 71]);
    }

    #[test]
    fn credence_rejects_values_above_one_hundred() {
        // Act and assert
        assert!(Credence::new(100).is_some());
        assert!(Credence::new(101).is_none());
    }

    #[test]
    fn credence_clamps_out_of_range_model_output() {
        // Arrange: models occasionally emit values outside the range.
        // Act and assert
        assert_eq!(Credence::clamped(-5).value(), 0);
        assert_eq!(Credence::clamped(140).value(), 100);
        assert_eq!(Credence::clamped(64).value(), 64);
    }

    #[test]
    fn agreement_gap_treats_opposite_confidences_as_agreement() {
        // Arrange: A abandons its position, B is sure of its own. Both now
        // believe the same thing.
        let a = Credence::clamped(3);
        let b = Credence::clamped(99);

        // Act and assert: the raw difference is 96, which would report maximum
        // disagreement at the moment the debate actually succeeded.
        assert_eq!(Credence::agreement_gap(a, b), 2);
        assert_eq!(a.distance(b), 96);
    }

    #[test]
    fn agreement_gap_treats_matching_confidences_as_disagreement() {
        // Arrange: both certain of opposing positions.
        let a = Credence::clamped(90);
        let b = Credence::clamped(90);

        // Act and assert: identical numbers, total disagreement.
        assert_eq!(Credence::agreement_gap(a, b), 80);
        assert_eq!(a.distance(b), 0);
    }

    #[test]
    fn agreement_gap_is_symmetric() {
        // Arrange
        let a = Credence::clamped(30);
        let b = Credence::clamped(55);

        // Act and assert
        assert_eq!(Credence::agreement_gap(a, b), Credence::agreement_gap(b, a));
    }

    #[test]
    fn agreement_gap_is_zero_when_the_pair_sums_to_one_hundred() {
        // Arrange and act and assert
        for value in [0u8, 25, 50, 75, 100] {
            let a = Credence::clamped(i64::from(value));
            let b = Credence::clamped(i64::from(100 - value));
            assert_eq!(Credence::agreement_gap(a, b), 0, "failed at {value}");
        }
    }

    #[test]
    fn credence_distance_is_symmetric() {
        // Arrange
        let low = Credence::clamped(20);
        let high = Credence::clamped(85);

        // Act and assert
        assert_eq!(low.distance(high), 65);
        assert_eq!(high.distance(low), 65);
    }

    #[test]
    fn totals_accumulate_across_turns() {
        // Arrange: four turns at a tenth of a cent each.
        let state = state_with_credences(&[Some(80), Some(20), Some(70), Some(35)]);

        // Act and assert
        assert!((state.total_cost() - 0.004).abs() < 1e-9);
    }

    #[test]
    fn side_other_is_an_involution() {
        // Guards against a typo swapping only one direction.
        assert_eq!(Side::A.other(), Side::B);
        assert_eq!(Side::B.other(), Side::A);
        assert_eq!(Side::A.other().other(), Side::A);
    }

    #[test]
    fn topic_returns_the_position_for_each_side() {
        // Arrange
        let topic = Topic::new("q", "case for", "case against");

        // Act and assert
        assert_eq!(topic.position(Side::A), "case for");
        assert_eq!(topic.position(Side::B), "case against");
    }
}
