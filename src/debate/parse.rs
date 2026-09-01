//! Tolerant extraction of the structured block a turn ends with.
//!
//! Each format asks the model to close its turn with a fenced `json` block
//! carrying format-specific fields. Models comply imperfectly: they omit the
//! block, label the fence differently, emit trailing commas, wrap the JSON in
//! commentary, or leave the fence unterminated.
//!
//! Every function here degrades rather than fails. A turn whose block cannot
//! be read is still a turn; it simply carries no structure and is flagged in
//! the UI. Failing the debate over a malformed block would be a far worse
//! outcome than losing one turn's credence reading.

use serde::Deserialize;

use crate::debate::state::{Claim, ClaimKind, Credence, ParseStatus, TurnAnalysis};

/// Fields a format may ask a model to supply.
///
/// Every field is optional so that one format's block deserializes even though
/// it omits another format's fields entirely.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StructuredBlock {
    /// Stated confidence, 0-100. Signed so out-of-range values clamp rather
    /// than fail deserialization.
    credence: Option<i64>,
    /// Justification for any movement in confidence.
    moved_because: Option<String>,
    /// Points conceded this turn.
    conceded: Vec<String>,
    /// The claim this side's case most depends on.
    key_claim: Option<String>,
    /// The disagreement the dispute reduces to.
    crux: Option<String>,
    /// Claims for the ledger format.
    claims: Vec<RawClaim>,
}

/// A claim as a model writes it.
#[derive(Debug, Deserialize)]
struct RawClaim {
    /// The claim text.
    #[serde(default)]
    text: String,
    /// Its standing, one of `agreed`, `disputed`, or `unresolved`.
    #[serde(default)]
    kind: String,
}

/// Locate the last fenced block in the text and return its body.
///
/// The **last** block is taken because models often illustrate a point with an
/// example block mid-argument before emitting the real one at the end.
///
/// Both ```` ```json ```` and a bare ```` ``` ```` fence are accepted; models
/// label inconsistently and rejecting an unlabelled fence would discard
/// otherwise valid output.
fn find_fenced_block(raw: &str) -> Option<&str> {
    let mut search_from = 0;
    let mut last: Option<&str> = None;

    while let Some(offset) = raw[search_from..].find("```") {
        let fence_start = search_from + offset + 3;
        let rest = raw.get(fence_start..)?;

        // Skip an optional language tag on the remainder of the fence line.
        let body_start = match rest.find('\n') {
            Some(newline) => fence_start + newline + 1,
            None => return last,
        };

        let body = raw.get(body_start..)?;
        match body.find("```") {
            Some(end) => {
                last = raw.get(body_start..body_start + end);
                search_from = body_start + end + 3;
            }
            None => {
                // Unterminated fence: take the remainder. A model that stops
                // mid-block still produced usable JSON more often than not.
                return raw.get(body_start..).or(last);
            }
        }
    }

    last
}

/// Remove the trailing fenced block from a turn's prose.
///
/// # Arguments
///
/// * `raw` - The model's full reply
///
/// # Returns
///
/// The argument with its structured block removed and whitespace trimmed. If
/// no block is present the input is returned unchanged.
///
/// # Examples
///
/// ```
/// # use coin::debate::parse::strip_structured_block;
/// let raw = "My argument.\n```json\n{\"credence\": 60}\n```";
/// assert_eq!(strip_structured_block(raw), "My argument.");
/// ```
pub fn strip_structured_block(raw: &str) -> &str {
    // Locate the fence that opens the final block, not the block body.
    match raw.rfind("```") {
        Some(_) => {
            let opening = find_last_opening_fence(raw);
            match opening {
                Some(index) => raw[..index].trim(),
                None => raw.trim(),
            }
        }
        None => raw.trim(),
    }
}

/// Index of the fence that opens the final fenced block.
fn find_last_opening_fence(raw: &str) -> Option<usize> {
    let mut fences: Vec<usize> = Vec::new();
    let mut search_from = 0;

    while let Some(offset) = raw[search_from..].find("```") {
        let index = search_from + offset;
        fences.push(index);
        search_from = index + 3;
    }

    match fences.len() {
        0 => None,
        // An odd count means the last fence is unterminated and opens a block.
        count if count % 2 == 1 => fences.last().copied(),
        // An even count means the second to last fence opens the final block.
        count => fences.get(count - 2).copied(),
    }
}

/// Parse a turn's reply into prose and structure.
///
/// # Arguments
///
/// * `raw` - The model's full reply
///
/// # Returns
///
/// The prose with its block removed, and the extracted analysis. The analysis
/// records why extraction failed when it did, rather than reporting success.
///
/// # Examples
///
/// ```
/// # use coin::debate::parse::parse_turn;
/// let (prose, analysis) = parse_turn("Case.\n```json\n{\"credence\": 64}\n```");
/// assert_eq!(prose, "Case.");
/// assert_eq!(analysis.credence.map(|c| c.value()), Some(64));
/// ```
pub fn parse_turn(raw: &str) -> (String, TurnAnalysis) {
    let prose = strip_structured_block(raw).to_string();

    let Some(body) = find_fenced_block(raw) else {
        return (prose, TurnAnalysis::empty(ParseStatus::Missing));
    };

    let block: StructuredBlock = match serde_json::from_str(body.trim()) {
        Ok(block) => block,
        Err(error) => {
            return (
                prose,
                TurnAnalysis::empty(ParseStatus::Malformed {
                    reason: error.to_string(),
                }),
            );
        }
    };

    let claims = block
        .claims
        .into_iter()
        .filter(|claim| !claim.text.trim().is_empty())
        .map(|claim| Claim {
            text: claim.text,
            kind: match claim.kind.to_lowercase().as_str() {
                "agreed" => ClaimKind::Agreed,
                "disputed" => ClaimKind::Disputed,
                // An unrecognized label is treated as unresolved rather than
                // discarded: the claim text is still worth keeping.
                _ => ClaimKind::Unresolved,
            },
        })
        .collect();

    (
        prose,
        TurnAnalysis {
            credence: block.credence.map(Credence::clamped),
            moved_because: block.moved_because,
            conceded: block.conceded,
            key_claim: block.key_claim,
            crux: block.crux,
            claims,
            parse_status: ParseStatus::Ok,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_block() {
        // Arrange
        let raw = "The data supports my position.\n\n```json\n\
                   {\"credence\": 64, \"moved_because\": \"new figure\"}\n```";

        // Act
        let (prose, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(prose, "The data supports my position.");
        assert_eq!(analysis.credence.map(Credence::value), Some(64));
        assert_eq!(analysis.moved_because.as_deref(), Some("new figure"));
        assert!(analysis.parse_status.is_ok());
    }

    #[test]
    fn a_missing_block_degrades_to_prose_only() {
        // Arrange: the model ignored the format instruction entirely.
        let raw = "I simply disagree, and here is why.";

        // Act
        let (prose, analysis) = parse_turn(raw);

        // Assert: the turn survives, flagged rather than failed.
        assert_eq!(prose, "I simply disagree, and here is why.");
        assert_eq!(analysis.parse_status, ParseStatus::Missing);
        assert!(analysis.credence.is_none());
    }

    #[test]
    fn a_malformed_block_keeps_the_prose_and_records_why() {
        // Arrange: a trailing comma, which models emit routinely.
        let raw = "My case.\n```json\n{\"credence\": 64,}\n```";

        // Act
        let (prose, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(prose, "My case.");
        assert!(matches!(
            analysis.parse_status,
            ParseStatus::Malformed { .. }
        ));
    }

    #[test]
    fn accepts_an_unlabelled_fence() {
        // Arrange: models frequently omit the language tag.
        let raw = "My case.\n```\n{\"credence\": 40}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(analysis.credence.map(Credence::value), Some(40));
    }

    #[test]
    fn takes_the_last_block_when_the_argument_contains_an_example() {
        // Arrange: an illustrative block mid-argument, the real one at the end.
        let raw = "Consider this:\n```json\n{\"credence\": 99}\n```\n\
                   That was an example. My actual position:\n\
                   ```json\n{\"credence\": 30}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(analysis.credence.map(Credence::value), Some(30));
    }

    #[test]
    fn recovers_from_an_unterminated_fence() {
        // Arrange: the model stopped before closing the block.
        let raw = "My case.\n```json\n{\"credence\": 55}";

        // Act
        let (prose, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(prose, "My case.");
        assert_eq!(analysis.credence.map(Credence::value), Some(55));
    }

    #[test]
    fn clamps_an_out_of_range_credence() {
        // Arrange: 150 is not a confidence, but the turn is still usable.
        let raw = "```json\n{\"credence\": 150}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(analysis.credence.map(Credence::value), Some(100));
    }

    #[test]
    fn a_negative_credence_clamps_to_zero() {
        // Arrange
        let raw = "```json\n{\"credence\": -20}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert_eq!(analysis.credence.map(Credence::value), Some(0));
    }

    #[test]
    fn unknown_fields_do_not_prevent_parsing() {
        // Arrange: a model volunteering extra structure must not break the turn.
        let raw = "```json\n{\"credence\": 50, \"mood\": \"confident\"}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert!(analysis.parse_status.is_ok());
        assert_eq!(analysis.credence.map(Credence::value), Some(50));
    }

    #[test]
    fn a_block_absent_of_known_fields_still_parses() {
        // Arrange: one format's block lacks another format's fields.
        let raw = "```json\n{\"claims\": []}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert
        assert!(analysis.parse_status.is_ok());
        assert!(analysis.credence.is_none());
    }

    #[test]
    fn claims_map_their_kind_and_drop_empty_text() {
        // Arrange
        let raw = "```json\n{\"claims\": [\
                   {\"text\": \"inflation rose\", \"kind\": \"agreed\"},\
                   {\"text\": \"cause was fiscal\", \"kind\": \"DISPUTED\"},\
                   {\"text\": \"effect size\", \"kind\": \"nonsense\"},\
                   {\"text\": \"   \", \"kind\": \"agreed\"}]}\n```";

        // Act
        let (_, analysis) = parse_turn(raw);

        // Assert: kind matching is case insensitive, unknown kinds become
        // unresolved rather than being discarded, and blank claims are dropped.
        assert_eq!(analysis.claims.len(), 3);
        assert_eq!(analysis.claims[0].kind, ClaimKind::Agreed);
        assert_eq!(analysis.claims[1].kind, ClaimKind::Disputed);
        assert_eq!(analysis.claims[2].kind, ClaimKind::Unresolved);
    }

    #[test]
    fn strips_the_block_but_keeps_multi_paragraph_prose() {
        // Arrange
        let raw = "First paragraph.\n\nSecond paragraph.\n\n```json\n{}\n```";

        // Act
        let prose = strip_structured_block(raw);

        // Assert
        assert_eq!(prose, "First paragraph.\n\nSecond paragraph.");
    }

    #[test]
    fn prose_without_any_fence_is_returned_intact() {
        // Arrange
        let raw = "  Just an argument.  ";

        // Act and assert
        assert_eq!(strip_structured_block(raw), "Just an argument.");
    }

    #[test]
    fn an_empty_reply_does_not_panic() {
        // Arrange: an aborted or empty turn must be handled, not crash.
        let (prose, analysis) = parse_turn("");

        // Assert
        assert_eq!(prose, "");
        assert_eq!(analysis.parse_status, ParseStatus::Missing);
    }

    #[test]
    fn a_bare_fence_with_no_body_does_not_panic() {
        // Arrange
        let (_, analysis) = parse_turn("```");

        // Assert
        assert!(!analysis.parse_status.is_ok());
    }
}
