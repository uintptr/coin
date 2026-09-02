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

/// How one side's stated confidence moved across a debate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Movement {
    /// The side measured.
    pub side: Side,
    /// Its first readable confidence.
    pub opened: Credence,
    /// Its last readable confidence.
    pub closed: Credence,
    /// How many readable confidences it stated. One means the movement is
    /// unknown rather than zero.
    pub readings: usize,
}

impl Movement {
    /// Change in stated confidence, in percentage points.
    ///
    /// Signed, because the direction is the point: a side that talked itself
    /// down has done something different from one that dug in.
    ///
    /// # Returns
    ///
    /// Closing confidence minus opening, from -100 to 100.
    pub fn delta(&self) -> i16 {
        i16::from(self.closed.value()) - i16::from(self.opened.value())
    }
}

/// A point one side gave up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Concession {
    /// The side that conceded it.
    pub side: Side,
    /// Round it was conceded in.
    pub round: usize,
    /// What was conceded, as the side put it.
    pub text: String,
}

/// Token and cost accounting over a set of turns.
///
/// A debate spends real money, so what it spent is part of the result rather
/// than a diagnostic. Reasoning tokens are carried separately from output
/// because models that report them bill them separately, and folding them into
/// the output count would understate what a thinking model produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Usage {
    /// Turns counted.
    pub turns: usize,
    /// Tokens consumed across them.
    pub tokens: Tokens,
    /// Cost in USD across them.
    pub cost: f64,
}

/// Sum turns into a [`Usage`].
///
/// Rust note for Python developers: implementing `FromIterator` is what lets
/// any iterator of turns end in `.collect()`, so the total and the per-side
/// figures share one summing rule instead of repeating it.
impl<'a> FromIterator<&'a Turn> for Usage {
    fn from_iter<I>(turns: I) -> Self
    where
        I: IntoIterator<Item = &'a Turn>,
    {
        turns.into_iter().fold(Self::default(), |mut total, turn| {
            total.turns += 1;
            total.tokens.input += turn.tokens.input;
            total.tokens.output += turn.tokens.output;
            total.tokens.reasoning += turn.tokens.reasoning;
            total.cost += turn.cost;
            total
        })
    }
}

/// One row of the convergence table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CredenceRound {
    /// Round these readings come from, counting from one.
    pub round: usize,
    /// Side A's stated confidence in side A's position, if it stated one.
    pub a: Option<Credence>,
    /// Side B's stated confidence in side B's position, if it stated one.
    pub b: Option<Credence>,
}

impl CredenceRound {
    /// How far apart the two sides are, when both stated a confidence.
    ///
    /// # Returns
    ///
    /// The agreement gap, or `None` if either side is missing.
    pub fn gap(&self) -> Option<u8> {
        match (self.a, self.b) {
            (Some(a), Some(b)) => Some(Credence::agreement_gap(a, b)),
            _ => None,
        }
    }
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
    /// A side could not produce a turn, so there was nothing left to debate.
    ///
    /// Distinct from every other reason here: the debate did not reach a
    /// result, and whatever numbers it did collect are partial. Recording it
    /// keeps a saved transcript honest about why it stops where it does.
    Failed {
        /// The side whose turn could not be produced.
        side: Side,
        /// What went wrong, as reported by opencode or the provider.
        message: String,
    },
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

    /// Every credence stated by the given side, paired with its round.
    ///
    /// The round must travel with the value. Turns whose structured block was
    /// unreadable state no credence, so the Nth reading is not necessarily the
    /// Nth round; a debate that ran six rounds can yield two readings. Labelling
    /// by position would then misreport which round produced them.
    ///
    /// # Arguments
    ///
    /// * `side` - Whose credences to collect
    ///
    /// # Returns
    ///
    /// Round and credence pairs, oldest first.
    pub fn credence_series(&self, side: Side) -> Vec<(usize, Credence)> {
        self.turns
            .iter()
            .filter(|turn| turn.side == side)
            .filter_map(|turn| turn.analysis.credence.map(|value| (turn.round, value)))
            .collect()
    }

    /// The convergence table: one row per round in which either side stated a
    /// credence.
    ///
    /// # Returns
    ///
    /// Rows in round order, each carrying whichever readings exist.
    pub fn credence_rounds(&self) -> Vec<CredenceRound> {
        let mut rows: Vec<CredenceRound> = Vec::new();

        for turn in &self.turns {
            let Some(value) = turn.analysis.credence else {
                continue;
            };
            let row = match rows.iter_mut().find(|row| row.round == turn.round) {
                Some(existing) => existing,
                None => {
                    rows.push(CredenceRound {
                        round: turn.round,
                        a: None,
                        b: None,
                    });
                    // The push cannot leave the vector empty.
                    match rows.last_mut() {
                        Some(row) => row,
                        None => continue,
                    }
                }
            };
            match turn.side {
                Side::A => row.a = Some(value),
                Side::B => row.b = Some(value),
            }
        }

        rows
    }

    /// How many turns produced no readable structured block.
    ///
    /// A debate can run to completion with most of its structure unreadable,
    /// since parsing degrades rather than failing. That is the intended
    /// behaviour, but it silently weakens every conclusion drawn from the
    /// numbers, so the count is surfaced rather than hidden.
    pub fn unreadable_turns(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| !turn.analysis.parse_status.is_ok())
            .count()
    }

    /// Turns one side took, oldest first.
    fn turns_of(&self, side: Side) -> impl DoubleEndedIterator<Item = &Turn> {
        self.turns.iter().filter(move |turn| turn.side == side)
    }

    /// How one side's stated confidence moved across the debate.
    ///
    /// Measured between the first and last **readable** readings, not the
    /// first and last turns. A turn whose structured block could not be read
    /// states no confidence, so counting it as unchanged would invent a
    /// steadiness the side never expressed.
    ///
    /// # Arguments
    ///
    /// * `side` - The side to measure
    ///
    /// # Returns
    ///
    /// `None` when the side never stated a readable confidence.
    pub fn movement(&self, side: Side) -> Option<Movement> {
        let mut readings = self
            .turns_of(side)
            .filter_map(|turn| turn.analysis.credence)
            .peekable();

        let opened = readings.next()?;
        let mut closed = opened;
        let mut count = 1;
        for value in readings {
            closed = value;
            count += 1;
        }

        Some(Movement {
            side,
            opened,
            closed,
            readings: count,
        })
    }

    /// Every point either side gave up, in the order it was conceded.
    ///
    /// This is the most valuable thing a debate produces and the easiest to
    /// lose: a concession arrives mid-argument and scrolls away. Gathering
    /// them puts the debate's actual movement in one place.
    ///
    /// # Returns
    ///
    /// One entry per conceded point, blank entries dropped.
    pub fn concessions(&self) -> Vec<Concession> {
        self.turns
            .iter()
            .flat_map(|turn| {
                turn.analysis
                    .conceded
                    .iter()
                    .filter(|point| !point.trim().is_empty())
                    .map(move |point| Concession {
                        side: turn.side,
                        round: turn.round,
                        text: point.trim().to_string(),
                    })
            })
            .collect()
    }

    /// The claim a side's case finally rested on.
    ///
    /// The **last** one stated, not the first: a side that revised what its
    /// argument depends on has told us something, and the closing position is
    /// the one that survived the debate.
    ///
    /// # Arguments
    ///
    /// * `side` - The side to read
    ///
    /// # Returns
    ///
    /// `None` when the side never stated one readably.
    pub fn key_claim(&self, side: Side) -> Option<&str> {
        self.turns_of(side)
            .rev()
            .find_map(|turn| turn.analysis.key_claim.as_deref())
            .map(str::trim)
            .filter(|claim| !claim.is_empty())
    }

    /// Tools invoked across the debate, tallied by name.
    ///
    /// Which tools a debate reached for says how much of it was checked
    /// against the world rather than asserted, which is the project's whole
    /// premise.
    ///
    /// # Returns
    ///
    /// Tool names with their invocation counts, most used first, ties broken
    /// by name so the order is stable.
    pub fn tool_tally(&self) -> Vec<(&str, usize)> {
        let mut counts: Vec<(&str, usize)> = Vec::new();

        for call in self.turns.iter().flat_map(|turn| turn.tool_calls.iter()) {
            match counts.iter_mut().find(|(name, _)| *name == call.tool) {
                Some((_, count)) => *count += 1,
                None => counts.push((&call.tool, 1)),
            }
        }

        counts.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
        counts
    }

    /// Accounting across every turn.
    pub fn usage(&self) -> Usage {
        self.turns.iter().collect()
    }

    /// Accounting for the turns one side took.
    ///
    /// Worth separating because the two sides can run different models. A
    /// single total hides which of them the money went to, which is exactly
    /// what a reader wants to know when the sides differ.
    ///
    /// # Arguments
    ///
    /// * `side` - The side to account for
    ///
    /// # Returns
    ///
    /// That side's turn count, tokens, and cost.
    pub fn usage_for(&self, side: Side) -> Usage {
        self.turns.iter().filter(|turn| turn.side == side).collect()
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
            let status = if value.is_some() {
                ParseStatus::Ok
            } else {
                ParseStatus::Missing
            };
            let mut analysis = TurnAnalysis::empty(status);
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
        let series: Vec<(usize, u8)> = state
            .credence_series(Side::A)
            .iter()
            .map(|(round, credence)| (*round, credence.value()))
            .collect();

        // Assert
        assert_eq!(series, vec![(1, 85), (2, 71)]);
    }

    #[test]
    fn credence_series_reports_the_round_a_reading_came_from() {
        // Arrange: a long debate where only the later turns parsed, which is
        // what a six round debate yielding two readings looks like.
        let state = state_with_credences(&[None, None, None, None, Some(66), Some(48)]);

        // Act
        let series = state.credence_series(Side::A);

        // Assert: the single reading is from round 3, not round 1. Labelling
        // by position would have reported it as the first round.
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].0, 3);
        assert_eq!(series[0].1.value(), 66);
    }

    #[test]
    fn credence_rounds_pairs_the_sides_by_round() {
        // Arrange
        let state = state_with_credences(&[Some(72), Some(60), Some(66), Some(48)]);

        // Act
        let rows = state.credence_rounds();

        // Assert
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].round, 1);
        assert_eq!(rows[0].a.map(Credence::value), Some(72));
        assert_eq!(rows[0].b.map(Credence::value), Some(60));
        assert_eq!(rows[0].gap(), Some(32));
        assert_eq!(rows[1].gap(), Some(14));
    }

    #[test]
    fn credence_rounds_skips_rounds_where_neither_side_parsed() {
        // Arrange: rounds 1 and 2 unreadable, round 3 fine.
        let state = state_with_credences(&[None, None, None, None, Some(66), Some(48)]);

        // Act
        let rows = state.credence_rounds();

        // Assert: one row, correctly labelled round 3.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].round, 3);
    }

    #[test]
    fn credence_rounds_leaves_a_gap_absent_when_one_side_is_missing() {
        // Arrange: only A stated a confidence this round.
        let state = state_with_credences(&[Some(70), None]);

        // Act
        let rows = state.credence_rounds();

        // Assert
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].b, None);
        assert_eq!(rows[0].gap(), None);
    }

    #[test]
    fn unreadable_turns_are_counted() {
        // Arrange: four of six turns produced no readable block.
        let state = state_with_credences(&[None, None, None, None, Some(66), Some(48)]);

        // Act and assert
        assert_eq!(state.unreadable_turns(), 4);
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
    fn movement_spans_the_first_and_last_readable_readings() {
        // Arrange: A opens at 80 and ends at 70, B opens at 20 and ends at 35.
        let state = state_with_credences(&[Some(80), Some(20), Some(70), Some(35)]);

        // Act
        let a = state.movement(Side::A).expect("A stated confidences");
        let b = state.movement(Side::B).expect("B stated confidences");

        // Assert: the sign carries the direction, which is the point.
        assert_eq!((a.opened.value(), a.closed.value()), (80, 70));
        assert_eq!(a.delta(), -10);
        assert_eq!(b.delta(), 15);
        assert_eq!(a.readings, 2);
    }

    #[test]
    fn movement_skips_turns_whose_structure_was_unreadable() {
        // Arrange: A states 90, then a turn that could not be read, then 40.
        // Counting the unreadable turn as unchanged would invent a steadiness
        // the side never expressed.
        let state = state_with_credences(&[Some(90), None, None, None, Some(40), None]);

        // Act
        let a = state.movement(Side::A).expect("A stated confidences");

        // Assert
        assert_eq!(a.delta(), -50);
        assert_eq!(a.readings, 2, "the unreadable turn must not count");
    }

    #[test]
    fn a_side_that_never_stated_a_confidence_has_no_movement() {
        // Arrange
        let state = state_with_credences(&[None, None]);

        // Act and assert
        assert!(state.movement(Side::A).is_none());
    }

    #[test]
    fn a_single_reading_is_reported_as_one_rather_than_as_no_change() {
        // Arrange: one readable turn. Delta is zero, but that is unknown
        // movement, not a side holding firm, so the count says which.
        let state = state_with_credences(&[Some(65), None]);

        // Act
        let a = state.movement(Side::A).expect("A stated one confidence");

        // Assert
        assert_eq!(a.readings, 1);
        assert_eq!(a.delta(), 0);
    }

    #[test]
    fn concessions_are_gathered_in_order_with_their_side_and_round() {
        // Arrange
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 3);
        let mut first = TurnAnalysis::empty(ParseStatus::Ok);
        first.conceded = vec!["the 2019 figure was superseded".into(), "   ".into()];
        let mut second = TurnAnalysis::empty(ParseStatus::Ok);
        second.conceded = vec!["  the effect size is smaller  ".into()];

        for (side, analysis) in [(Side::A, first), (Side::B, second)] {
            state.push_turn(
                side,
                "argument".into(),
                analysis,
                Vec::new(),
                Tokens::default(),
                0.0,
            );
        }

        // Act
        let conceded = state.concessions();

        // Assert: blank entries are dropped and text is trimmed.
        assert_eq!(conceded.len(), 2);
        assert_eq!(conceded[0].side, Side::A);
        assert_eq!(conceded[0].round, 1);
        assert_eq!(conceded[0].text, "the 2019 figure was superseded");
        assert_eq!(conceded[1].text, "the effect size is smaller");
    }

    #[test]
    fn the_key_claim_is_the_last_one_a_side_stated() {
        // Arrange: a side that revised what its argument depends on has told
        // us something, and the closing position is the one that survived.
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 3);
        for claim in ["the first thing", "the thing it settled on"] {
            let mut analysis = TurnAnalysis::empty(ParseStatus::Ok);
            analysis.key_claim = Some(claim.to_string());
            state.push_turn(
                Side::A,
                "argument".into(),
                analysis,
                Vec::new(),
                Tokens::default(),
                0.0,
            );
        }

        // Act and assert
        assert_eq!(state.key_claim(Side::A), Some("the thing it settled on"));
        assert_eq!(state.key_claim(Side::B), None);
    }

    #[test]
    fn tools_are_tallied_most_used_first() {
        // Arrange
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 3);
        let call = |tool: &str| ToolCall {
            tool: tool.to_string(),
            status: "completed".to_string(),
            detail: String::new(),
        };
        state.push_turn(
            Side::A,
            "argument".into(),
            TurnAnalysis::empty(ParseStatus::Ok),
            vec![call("websearch"), call("bash"), call("websearch")],
            Tokens::default(),
            0.0,
        );
        state.push_turn(
            Side::B,
            "argument".into(),
            TurnAnalysis::empty(ParseStatus::Ok),
            vec![call("websearch"), call("read")],
            Tokens::default(),
            0.0,
        );

        // Act
        let tally = state.tool_tally();

        // Assert: ties break by name so the order is stable across runs.
        assert_eq!(tally, vec![("websearch", 3), ("bash", 1), ("read", 1)]);
    }

    #[test]
    fn totals_accumulate_across_turns() {
        // Arrange: four turns at a tenth of a cent each.
        let state = state_with_credences(&[Some(80), Some(20), Some(70), Some(35)]);

        // Act
        let usage = state.usage();

        // Act and assert
        assert_eq!(usage.turns, 4);
        assert!((usage.cost - 0.004).abs() < 1e-9);
    }

    #[test]
    fn usage_splits_by_side() {
        // Arrange: turns alternate from A, so four turns is two each.
        let state = state_with_credences(&[Some(80), Some(20), Some(70), Some(35)]);

        // Act
        let (a, b) = (state.usage_for(Side::A), state.usage_for(Side::B));

        // Assert: the halves must account for the whole, or the split is
        // dropping or double counting turns.
        assert_eq!(a.turns, 2);
        assert_eq!(b.turns, 2);
        assert!((a.cost + b.cost - state.usage().cost).abs() < 1e-9);
    }

    #[test]
    fn usage_of_a_debate_with_no_turns_is_zero() {
        // Arrange: a debate that failed before either side spoke.
        let state = DebateState::new(Topic::new("q", "a", "b"), 3);

        // Act and assert
        assert_eq!(state.usage(), Usage::default());
        assert_eq!(state.usage_for(Side::A).turns, 0);
    }

    #[test]
    fn reasoning_tokens_are_counted_separately_from_output() {
        // Arrange: a thinking model bills reasoning apart from output, so
        // folding the two together would understate what it produced.
        let mut state = DebateState::new(Topic::new("q", "a", "b"), 3);
        state.push_turn(
            Side::A,
            "argument".into(),
            TurnAnalysis::empty(ParseStatus::Ok),
            Vec::new(),
            Tokens {
                input: 100,
                output: 20,
                reasoning: 500,
            },
            0.001,
        );

        // Act
        let usage = state.usage();

        // Assert
        assert_eq!(usage.tokens.output, 20);
        assert_eq!(usage.tokens.reasoning, 500);
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
