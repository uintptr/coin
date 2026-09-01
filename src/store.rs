//! Transcript persistence and export.
//!
//! Every debate is saved without being asked for. A debate costs real money and
//! several minutes, and the interesting part is often a single concession
//! buried mid-argument, so losing one to a closed terminal is a genuine loss.
//!
//! Two formats are written side by side because they serve different readers.
//! JSON carries the complete state, including every credence and parse status,
//! and is what the web layer will later serve from `/api/transcript.json`.
//! Markdown is for people.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::debate::format::FormatId;
use crate::debate::state::{DebateState, Side, StopReason};
use crate::error::{CoinError, Result};
use crate::opencode::types::ModelRef;

/// Schema version, so a later reader can recognise an older transcript.
const TRANSCRIPT_VERSION: u32 = 1;

/// Filename stem used for both auto-saved transcripts.
const TRANSCRIPT_STEM: &str = "transcript";

/// A debate, its configuration, and when it ran.
///
/// [`DebateState`] alone does not record which format or models produced it,
/// which is exactly what a reader needs to judge a result months later.
#[derive(Debug, Serialize)]
pub struct Transcript<'a> {
    /// Schema version of this file.
    pub version: u32,
    /// When the debate finished, as an RFC 3339 timestamp.
    pub saved_at: String,
    /// Format the debate ran under.
    pub format: FormatId,
    /// Model that argued side A.
    pub model_a: Option<&'a ModelRef>,
    /// Model that argued side B.
    pub model_b: Option<&'a ModelRef>,
    /// The debate itself.
    pub state: &'a DebateState,
}

impl<'a> Transcript<'a> {
    /// Assemble a transcript from a finished debate.
    ///
    /// # Arguments
    ///
    /// * `state` - The completed debate
    /// * `format` - Format it ran under
    /// * `model_a` - Model that argued side A
    /// * `model_b` - Model that argued side B
    ///
    /// # Returns
    ///
    /// A transcript stamped with the current time.
    pub fn new(
        state: &'a DebateState,
        format: FormatId,
        model_a: Option<&'a ModelRef>,
        model_b: Option<&'a ModelRef>,
    ) -> Self {
        Self {
            version: TRANSCRIPT_VERSION,
            saved_at: chrono::Local::now().to_rfc3339(),
            format,
            model_a,
            model_b,
            state,
        }
    }

    /// Serialize to pretty-printed JSON.
    ///
    /// # Returns
    ///
    /// The complete transcript, including every field the UI would show.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Decode`] if serialization fails.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|source| CoinError::decode("transcript", source))
    }

    /// Render as Markdown for reading.
    ///
    /// # Returns
    ///
    /// A document with the topic, each turn in order with its tool use and
    /// analysis, and the closing convergence table.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        self.write_header(&mut out);

        for turn in &self.state.turns {
            out.push_str(&format!(
                "\n## Round {}, Debater {}\n\n",
                turn.round, turn.side
            ));

            if !turn.tool_calls.is_empty() {
                for call in &turn.tool_calls {
                    if call.detail.is_empty() {
                        out.push_str(&format!("- tool `{}` ({})\n", call.tool, call.status));
                    } else {
                        out.push_str(&format!(
                            "- tool `{}` ({}): {}\n",
                            call.tool, call.status, call.detail
                        ));
                    }
                }
                out.push('\n');
            }

            out.push_str(turn.text.trim());
            out.push_str("\n\n");
            self.write_analysis(&mut out, turn);
        }

        self.write_result(&mut out);
        out
    }

    /// Write the topic, configuration, and positions.
    fn write_header(&self, out: &mut String) {
        out.push_str(&format!("# {}\n\n", self.state.topic.question.trim()));
        out.push_str(&format!("- **Side A**: {}\n", self.state.topic.position_a));
        out.push_str(&format!("- **Side B**: {}\n", self.state.topic.position_b));
        out.push_str(&format!("- **Format**: {}\n", self.format));

        let name = |model: Option<&ModelRef>| {
            model.map_or_else(|| "server default".to_string(), ModelRef::to_string)
        };
        if self.model_a == self.model_b {
            out.push_str(&format!("- **Model**: {}\n", name(self.model_a)));
        } else {
            out.push_str(&format!("- **Model A**: {}\n", name(self.model_a)));
            out.push_str(&format!("- **Model B**: {}\n", name(self.model_b)));
        }
        out.push_str(&format!("- **Recorded**: {}\n", self.saved_at));
    }

    /// Write one turn's extracted structure.
    fn write_analysis(&self, out: &mut String, turn: &crate::debate::state::Turn) {
        if let Some(credence) = turn.analysis.credence {
            out.push_str(&format!("**Confidence: {credence}**\n\n"));
        }
        if let Some(reason) = &turn.analysis.moved_because {
            out.push_str(&format!("*Moved because:* {reason}\n\n"));
        }
        if !turn.analysis.conceded.is_empty() {
            out.push_str("*Conceded:*\n\n");
            for point in &turn.analysis.conceded {
                out.push_str(&format!("- {point}\n"));
            }
            out.push('\n');
        }
        if !turn.analysis.parse_status.is_ok() {
            out.push_str("*No readable structured block in this turn.*\n\n");
        }
    }

    /// Write the convergence table and closing totals.
    fn write_result(&self, out: &mut String) {
        out.push_str("\n## Result\n\n");

        let series_a = self.state.credence_series(Side::A);
        let series_b = self.state.credence_series(Side::B);

        if !series_a.is_empty() || !series_b.is_empty() {
            // Each side states confidence in its own position, so the gap
            // column restates them on one proposition.
            out.push_str("| Round | A | B | Gap |\n|---|---|---|---|\n");
            for round in 0..series_a.len().max(series_b.len()) {
                let cell = |series: &[crate::debate::state::Credence]| {
                    series
                        .get(round)
                        .map_or_else(|| "-".to_string(), ToString::to_string)
                };
                let gap = match (series_a.get(round), series_b.get(round)) {
                    (Some(a), Some(b)) => {
                        crate::debate::state::Credence::agreement_gap(*a, *b).to_string()
                    }
                    _ => "-".to_string(),
                };
                out.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    round + 1,
                    cell(&series_a),
                    cell(&series_b),
                    gap
                ));
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "**Outcome:** {}\n\n",
            describe_stop(&self.state.stop_reason)
        ));

        let tokens = self.state.total_tokens();
        out.push_str(&format!(
            "{} turns, {} tokens in, {} out, ${:.4}\n",
            self.state.turns.len(),
            tokens.input,
            tokens.output,
            self.state.total_cost()
        ));
    }
}

/// Describe why a debate ended, in a full sentence.
///
/// # Arguments
///
/// * `reason` - The recorded stop reason, if any
///
/// # Returns
///
/// A readable description suitable for a transcript or a terminal.
pub fn describe_stop(reason: &Option<StopReason>) -> String {
    match reason {
        Some(StopReason::Converged { gap }) => {
            format!("confidences converged, {gap} points apart")
        }
        Some(StopReason::Conceded { side }) => format!("Debater {side} conceded"),
        // Deliberately not "without convergence": the credence format
        // suppresses convergence until two rounds have completed, so a
        // single-round debate hits the cap even when the sides fully agree.
        // The gap column shows how close they got.
        Some(StopReason::RoundCap) => "round cap reached".to_string(),
        Some(StopReason::CruxIsolated) => "both sides named the same crux".to_string(),
        Some(StopReason::NoNewClaims) => "a round introduced no new claims".to_string(),
        Some(StopReason::FormatComplete) => "the format's rounds finished".to_string(),
        Some(StopReason::Aborted) => "stopped by the operator".to_string(),
        None => "did not finish".to_string(),
    }
}

/// Write both transcript formats into a directory.
///
/// # Arguments
///
/// * `directory` - Where to write `transcript.json` and `transcript.md`
/// * `transcript` - The debate to record
///
/// # Returns
///
/// The path of the Markdown file, which is the one worth telling a user about.
///
/// # Errors
///
/// Returns [`CoinError::Io`] if either file cannot be written.
pub async fn save_to_dir<P>(directory: P, transcript: &Transcript<'_>) -> Result<PathBuf>
where
    P: AsRef<Path>,
{
    let directory = directory.as_ref();
    let json_path = directory.join(format!("{TRANSCRIPT_STEM}.json"));
    let markdown_path = directory.join(format!("{TRANSCRIPT_STEM}.md"));

    write_file(&json_path, &transcript.to_json()?).await?;
    write_file(&markdown_path, &transcript.to_markdown()).await?;

    Ok(markdown_path)
}

/// Write a transcript to one explicit path, choosing format by extension.
///
/// # Arguments
///
/// * `path` - Destination file; a `.json` extension selects JSON, anything
///   else selects Markdown
/// * `transcript` - The debate to record
///
/// # Errors
///
/// Returns [`CoinError::Io`] if the file cannot be written.
pub async fn save_to_file<P>(path: P, transcript: &Transcript<'_>) -> Result<()>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let is_json = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));

    let contents = if is_json {
        transcript.to_json()?
    } else {
        transcript.to_markdown()
    };

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| CoinError::io(parent, source))?;
    }

    write_file(path, &contents).await
}

/// Write a string to a path, naming the path on failure.
async fn write_file(path: &Path, contents: &str) -> Result<()> {
    tokio::fs::write(path, contents)
        .await
        .map_err(|source| CoinError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debate::state::{Credence, ParseStatus, Topic, TurnAnalysis};
    use crate::opencode::types::{Tokens, ToolCall};

    /// A two-turn debate ending in agreement, with tool use on side A.
    fn finished_debate() -> DebateState {
        let mut state = DebateState::new(Topic::new("Is X true?", "X is true", "X is false"), 6);

        let mut analysis_a = TurnAnalysis::empty(ParseStatus::Ok);
        analysis_a.credence = Credence::new(70);
        analysis_a.moved_because = Some("opened".to_string());
        state.push_turn(
            Side::A,
            "A argues.".to_string(),
            analysis_a,
            vec![ToolCall {
                tool: "websearch".to_string(),
                status: "completed".to_string(),
                detail: "evidence for X".to_string(),
            }],
            Tokens {
                input: 100,
                output: 20,
                reasoning: 0,
            },
            0.01,
        );

        let mut analysis_b = TurnAnalysis::empty(ParseStatus::Ok);
        analysis_b.credence = Credence::new(35);
        analysis_b.conceded = vec!["the central study is genuine".to_string()];
        state.push_turn(
            Side::B,
            "B concedes ground.".to_string(),
            analysis_b,
            Vec::new(),
            Tokens {
                input: 200,
                output: 40,
                reasoning: 0,
            },
            0.02,
        );

        state.stop_reason = Some(StopReason::Converged { gap: 5 });
        state
    }

    fn transcript_of(state: &DebateState) -> Transcript<'_> {
        Transcript::new(state, FormatId::Credence, None, None)
    }

    #[test]
    fn markdown_records_the_topic_and_both_positions() {
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        assert!(markdown.starts_with("# Is X true?"));
        assert!(markdown.contains("**Side A**: X is true"));
        assert!(markdown.contains("**Side B**: X is false"));
    }

    #[test]
    fn markdown_includes_every_turn_in_order() {
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        let first = markdown.find("A argues.").expect("turn A must appear");
        let second = markdown
            .find("B concedes ground.")
            .expect("turn B must appear");
        assert!(first < second, "turns must be in order");
    }

    #[test]
    fn markdown_records_tool_use() {
        // Tool use is the evidence a claim was checked rather than asserted,
        // so a transcript that omitted it would lose the point of the project.
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        assert!(markdown.contains("tool `websearch`"));
        assert!(markdown.contains("evidence for X"));
    }

    #[test]
    fn markdown_records_confidences_and_concessions() {
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        assert!(markdown.contains("**Confidence: 70**"));
        assert!(markdown.contains("the central study is genuine"));
    }

    #[test]
    fn markdown_gap_column_uses_agreement_not_difference() {
        // 70 and 35 sum to 105, so the sides are five points from agreement,
        // not the 35 a naive subtraction would report.
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        assert!(
            markdown.contains("| 1 | 70 | 35 | 5 |"),
            "gap column wrong in:\n{markdown}"
        );
    }

    #[test]
    fn markdown_states_the_outcome_and_totals() {
        let state = finished_debate();
        let markdown = transcript_of(&state).to_markdown();

        assert!(markdown.contains("confidences converged, 5 points apart"));
        assert!(markdown.contains("2 turns, 300 tokens in, 60 out, $0.0300"));
    }

    #[test]
    fn json_carries_the_configuration_the_state_omits() {
        // Which format and models produced a result is exactly what a reader
        // needs months later, and DebateState alone does not record it.
        let state = finished_debate();
        let model = ModelRef::new("digitalocean", "glm-5.3-flash");
        let transcript = Transcript::new(&state, FormatId::Credence, Some(&model), Some(&model));

        let json = transcript.to_json().expect("must serialize");

        assert!(json.contains("\"format\": \"credence\""));
        assert!(json.contains("glm-5.3-flash"));
        assert!(json.contains("\"version\": 1"));
    }

    #[test]
    fn json_preserves_parse_failures() {
        // A turn whose structure could not be read must stay visible in the
        // record rather than looking like a turn that simply said nothing.
        let mut state = finished_debate();
        state.push_turn(
            Side::A,
            "prose only".to_string(),
            TurnAnalysis::empty(ParseStatus::Malformed {
                reason: "trailing comma".to_string(),
            }),
            Vec::new(),
            Tokens::default(),
            0.0,
        );

        let json = transcript_of(&state).to_json().expect("must serialize");

        assert!(json.contains("malformed"));
        assert!(json.contains("trailing comma"));
    }

    #[test]
    fn stop_reasons_all_render_readably() {
        for reason in [
            StopReason::Converged { gap: 3 },
            StopReason::Conceded { side: Side::B },
            StopReason::RoundCap,
            StopReason::CruxIsolated,
            StopReason::NoNewClaims,
            StopReason::FormatComplete,
            StopReason::Aborted,
        ] {
            let text = describe_stop(&Some(reason.clone()));
            assert!(!text.is_empty(), "{reason:?} produced nothing");
            assert!(
                !text.contains('{'),
                "{reason:?} leaked debug formatting: {text}"
            );
        }
        assert_eq!(describe_stop(&None), "did not finish");
    }

    #[tokio::test]
    async fn save_to_dir_writes_both_formats() {
        let state = finished_debate();
        let directory =
            std::env::temp_dir().join(format!("coin-store-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("scratch directory must be creatable");

        let path = save_to_dir(&directory, &transcript_of(&state))
            .await
            .expect("save must succeed");

        assert!(path.ends_with("transcript.md"));
        assert!(directory.join("transcript.json").exists());
        assert!(directory.join("transcript.md").exists());

        let _ = tokio::fs::remove_dir_all(&directory).await;
    }

    #[tokio::test]
    async fn save_to_file_picks_format_from_the_extension() {
        let state = finished_debate();
        let directory = std::env::temp_dir().join(format!("coin-store-ext-{}", std::process::id()));
        let json_path = directory.join("out.json");
        let markdown_path = directory.join("out.txt");

        save_to_file(&json_path, &transcript_of(&state))
            .await
            .expect("json save must succeed");
        save_to_file(&markdown_path, &transcript_of(&state))
            .await
            .expect("markdown save must succeed");

        let json = tokio::fs::read_to_string(&json_path)
            .await
            .expect("json must be readable");
        let markdown = tokio::fs::read_to_string(&markdown_path)
            .await
            .expect("markdown must be readable");

        assert!(json.starts_with('{'));
        assert!(markdown.starts_with("# Is X true?"));

        let _ = tokio::fs::remove_dir_all(&directory).await;
    }

    #[tokio::test]
    async fn save_to_file_creates_missing_directories() {
        // A user naming a path under a directory that does not exist should
        // get the file, not an error.
        let state = finished_debate();
        let directory = std::env::temp_dir()
            .join(format!("coin-store-nested-{}", std::process::id()))
            .join("deep");
        let path = directory.join("debate.md");

        save_to_file(&path, &transcript_of(&state))
            .await
            .expect("save must create the parent directory");

        assert!(path.exists());
        let _ = tokio::fs::remove_dir_all(directory.parent().unwrap_or(&directory)).await;
    }
}
