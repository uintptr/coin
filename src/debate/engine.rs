//! Orchestration of a debate across two opencode sessions.
//!
//! The engine owns the loop: prompt a side, reconstruct the turn, parse it,
//! record it, decide whether to continue. It is generic over
//! [`OpencodeClient`] so it can be driven by a mock in tests with no network
//! and no model spend.
//!
//! Intervention (pause, inject, reroll) is step 8 and is not implemented here
//! yet. The loop is written so that adding a command channel later does not
//! require restructuring it.

use std::sync::Arc;

use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::RetryPolicy;
use crate::debate::format::DebateFormat;
use crate::debate::state::{DebateState, Side, StopReason, Topic, Turn};
use crate::error::{CoinError, Result};
use crate::opencode::client::{OpencodeClient, PromptOptions};
use crate::opencode::types::{AssistantMessage, MessageError, ModelRef, Tokens, ToolCall};

/// Everything needed to run one debate.
#[derive(Debug, Clone)]
pub struct DebateConfig {
    /// The proposition and both positions.
    pub topic: Topic,
    /// Hard cap on rounds, regardless of the format's own stop condition.
    pub max_rounds: usize,
    /// Model for side A. Falls back to the server default when unset.
    pub model_a: Option<ModelRef>,
    /// Model for side B.
    pub model_b: Option<ModelRef>,
    /// How a turn that comes back empty is retried.
    pub retry: RetryPolicy,
}

impl DebateConfig {
    /// The model assigned to a side.
    fn model(&self, side: Side) -> Option<ModelRef> {
        match side {
            Side::A => self.model_a.clone(),
            Side::B => self.model_b.clone(),
        }
    }
}

/// What one side produced during a single turn.
///
/// A turn is not one message. When a model uses tools, opencode records the
/// tool calls in earlier assistant messages and returns only the last one from
/// the prompt route, so the pieces are gathered from the session's message list
/// instead. See [`collect_turn`].
#[derive(Debug, Default)]
struct RawTurn {
    text: String,
    tool_calls: Vec<ToolCall>,
    tokens: Tokens,
    cost: f64,
    /// Failure recorded against one of the turn's messages, if any.
    error: Option<MessageError>,
}

/// Why an attempt at a turn produced nothing usable.
#[derive(Debug)]
enum TurnFault {
    /// Worth another attempt: a transient provider or transport failure, or a
    /// turn that came back silently for no stated reason.
    Transient(String),
    /// Not worth another attempt. Authentication, credit, and unavailable-model
    /// failures refuse identically however often they are asked, so retrying
    /// only delays the report by several minutes.
    Permanent(String),
}

/// Classify a completed attempt.
///
/// # Returns
///
/// `None` when the turn carries visible text, which is the only outcome the
/// debate can use. Otherwise the fault that explains the silence.
fn fault_of(raw: &RawTurn) -> Option<TurnFault> {
    if !raw.text.trim().is_empty() {
        return None;
    }

    Some(match &raw.error {
        Some(error) if error.is_retryable() => TurnFault::Transient(error.to_string()),
        Some(error) => TurnFault::Permanent(error.to_string()),
        // The provider accepted the turn and the model said nothing. There is
        // no stated cause to judge from, so it is treated as transient:
        // another attempt is the only way to separate a fluke from a model
        // that will not answer.
        None => TurnFault::Transient("the model produced no visible text".to_string()),
    })
}

/// Reconstruct the most recent turn from a session's full message list.
///
/// Everything after the final user message belongs to the turn just completed.
/// Aggregating them is what makes tool calls visible and the cost accurate;
/// reading only the prompt response undercounts both.
fn collect_turn(messages: &[AssistantMessage]) -> RawTurn {
    let start = messages
        .iter()
        .rposition(|message| message.info.role == "user")
        .map_or(0, |index| index + 1);

    messages
        .get(start..)
        .unwrap_or_default()
        .iter()
        .filter(|message| message.is_assistant())
        .fold(RawTurn::default(), |mut turn, message| {
            turn.text.push_str(&message.text());
            turn.tool_calls.extend(message.tool_calls());
            turn.tokens.input += message.info.tokens.input;
            turn.tokens.output += message.info.tokens.output;
            turn.tokens.reasoning += message.info.tokens.reasoning;
            turn.cost += message.info.cost;
            // The first failure is the one that explains the turn; anything
            // after it is a consequence.
            turn.error = turn.error.or_else(|| message.info.error.clone());
            turn
        })
}

/// Events emitted as a debate runs, for a caller to render.
#[derive(Debug, Clone)]
pub enum Progress {
    /// A side is about to speak.
    TurnStarted {
        /// Which side.
        side: Side,
        /// Round number, counting from one.
        round: usize,
    },
    /// A side finished speaking.
    ///
    /// Carries the completed turn so a caller can render its analysis without
    /// waiting for the debate to finish.
    TurnCompleted(Box<Turn>),
    /// The debate ended.
    Finished {
        /// Why it ended.
        reason: StopReason,
    },
}

/// Drives a debate to completion.
pub struct Engine<C>
where
    C: OpencodeClient,
{
    client: Arc<C>,
    format: Box<dyn DebateFormat>,
    config: DebateConfig,
    session_a: String,
    session_b: String,
}

impl<C> Engine<C>
where
    C: OpencodeClient,
{
    /// Create the sessions a debate needs and prepare the engine.
    ///
    /// The system prompts are delivered through agent definitions written into
    /// the workspace before the server started, so nothing needs to be seeded
    /// into the sessions here.
    ///
    /// # Arguments
    ///
    /// * `client` - Client for the running opencode server
    /// * `format` - The debate format to run
    /// * `config` - Topic, round cap, and model assignments
    ///
    /// # Returns
    ///
    /// An engine with one session per side.
    ///
    /// # Errors
    ///
    /// Propagates failures from session creation.
    pub async fn new(
        client: Arc<C>,
        format: Box<dyn DebateFormat>,
        config: DebateConfig,
    ) -> Result<Self> {
        let session_a = client.create_session().await?.id;
        let session_b = client.create_session().await?.id;

        debug!(%session_a, %session_b, format = %format.id(), "prepared debate sessions");

        Ok(Self {
            client,
            format,
            config,
            session_a,
            session_b,
        })
    }

    /// Session identifier for a side.
    ///
    /// Exposed so a caller can map opencode event-stream traffic back to the
    /// side that produced it, which is how live token streaming is rendered
    /// without the engine itself depending on a concrete transport.
    pub fn session_id(&self, side: Side) -> &str {
        self.session(side)
    }

    /// Session identifier for a side.
    fn session(&self, side: Side) -> &str {
        match side {
            Side::A => &self.session_a,
            Side::B => &self.session_b,
        }
    }

    /// Agent name for a side, matching the file written into the workspace.
    fn agent_name(side: Side) -> String {
        format!("debater-{}", side.label())
    }

    /// Prompt one side once and reconstruct what it produced.
    async fn attempt_turn(
        &self,
        side: Side,
        prompt: &str,
        options: &PromptOptions,
    ) -> Result<RawTurn> {
        let session = self.session(side);
        self.client.prompt(session, prompt, options).await?;

        // Re-read the session rather than trusting the prompt response, which
        // carries only the final message of a multi-message turn.
        let messages = self.client.messages(session).await?;
        Ok(collect_turn(&messages))
    }

    /// Prompt one side, retrying a turn that comes back empty.
    ///
    /// A silent turn is usually not a silent model. opencode answers the
    /// prompt route with 200 even when the provider refused the request, so an
    /// overloaded or throttled provider arrives here as a turn with no text
    /// and an error recorded on the message. Retrying recovers the transient
    /// cases; the rest are reported with the provider's own words, which is
    /// the difference between a debate that stops with a reason and one that
    /// runs its remaining rounds producing nothing.
    ///
    /// Each retry re-sends the same prompt, which appends a fresh user message
    /// to the session. That is deliberate: [`collect_turn`] reads back only
    /// what follows the last user message, so a retry cannot pick up debris
    /// from the attempt before it.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Session`] when the side is still silent after
    /// every attempt, or immediately for a failure that retrying cannot fix.
    async fn turn_with_retries(&self, side: Side, prompt: &str) -> Result<RawTurn> {
        let options = PromptOptions {
            agent: Some(Self::agent_name(side)),
            model: self.config.model(side),
        };
        let attempts = self.config.retry.attempts.max(1);
        let mut delay = self.config.retry.backoff;
        let mut attempt = 1;

        loop {
            let fault = match self.attempt_turn(side, prompt, &options).await {
                Ok(raw) => match fault_of(&raw) {
                    None => {
                        if attempt > 1 {
                            info!(%side, attempt, "turn succeeded on retry");
                        }
                        return Ok(raw);
                    }
                    Some(fault) => fault,
                },
                // A transport failure is indistinguishable from a slow one
                // that recovered, so it is retried on the same terms.
                Err(error) if error.is_retryable() => TurnFault::Transient(error.to_string()),
                Err(error) => return Err(error),
            };

            let reason = match fault {
                TurnFault::Permanent(reason) => {
                    error!(%side, attempt, %reason, "turn failed for a reason retrying cannot fix");
                    return Err(self.session_error(side, reason));
                }
                TurnFault::Transient(reason) => reason,
            };

            if attempt >= attempts {
                error!(%side, attempts, %reason, "turn produced nothing on every attempt");
                return Err(
                    self.session_error(side, format!("{reason} (after {attempts} attempts)"))
                );
            }

            warn!(
                %side,
                attempt,
                of = attempts,
                retry_in_ms = delay.as_millis(),
                %reason,
                "turn produced nothing, retrying"
            );
            sleep(delay).await;
            delay = delay.saturating_mul(2);
            attempt += 1;
        }
    }

    /// Build a session error naming the side that failed.
    fn session_error(&self, side: Side, message: String) -> CoinError {
        CoinError::Session {
            session_id: self.session(side).to_string(),
            message,
        }
    }

    /// Run one side's turn and record it.
    async fn take_turn(&self, state: &mut DebateState, side: Side) -> Result<usize> {
        let prompt = self.format.turn_prompt(state, side);
        let raw = self.turn_with_retries(side, &prompt).await?;

        let (prose, analysis) = self.format.parse_turn(&raw.text);

        if !analysis.parse_status.is_ok() {
            warn!(%side, status = ?analysis.parse_status, "turn had no readable structure");
        }

        Ok(state.push_turn(side, prose, analysis, raw.tool_calls, raw.tokens, raw.cost))
    }

    /// Run the debate to completion.
    ///
    /// Turns alternate starting with side A. The debate ends when the format's
    /// stop condition fires or the round cap is reached, whichever comes first.
    ///
    /// # Arguments
    ///
    /// * `on_progress` - Called as each turn starts and finishes
    ///
    /// # Returns
    ///
    /// The final state, with [`DebateState::stop_reason`] set. A side that
    /// cannot produce a turn ends the debate with [`StopReason::Failed`]
    /// rather than discarding the turns already taken.
    ///
    /// # Errors
    ///
    /// Does not fail on a failed turn. Reserved for failures that leave no
    /// state worth returning.
    pub async fn run<F>(&self, mut on_progress: F) -> Result<DebateState>
    where
        F: FnMut(Progress) + Send,
    {
        let mut state = DebateState::new(self.config.topic.clone(), self.config.max_rounds);

        let reason = loop {
            let side = state.next_side();
            let round = state.current_round();

            on_progress(Progress::TurnStarted { side, round });

            // A side that cannot answer ends the debate rather than failing
            // the call. The turns already taken cost real money and are still
            // worth saving, and the transcript records why it stops here.
            let index = match self.take_turn(&mut state, side).await {
                Ok(index) => index,
                Err(failure) => {
                    error!(%side, %failure, "debate cannot continue");
                    break StopReason::Failed {
                        side,
                        message: failure.to_string(),
                    };
                }
            };

            if let Some(turn) = state.turns.get(index) {
                on_progress(Progress::TurnCompleted(Box::new(turn.clone())));
            }

            // Stop conditions are only meaningful on a complete round, so that
            // both sides have answered the same exchange.
            if state.next_side() == Side::A {
                if let Some(reason) = self.format.should_stop(&state) {
                    break reason;
                }
                if state.completed_rounds() >= self.config.max_rounds {
                    break StopReason::RoundCap;
                }
            }
        };

        info!(
            ?reason,
            rounds = state.completed_rounds(),
            "debate finished"
        );
        state.stop_reason = Some(reason.clone());
        on_progress(Progress::Finished { reason });

        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::types::{
        MessageError, MessageErrorData, MessageInfo, Part, Session, ToolState,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    /// One scripted turn.
    #[derive(Debug, Clone)]
    enum Reply {
        /// The model answered with this text.
        Text(&'static str),
        /// The turn came back with no text and no stated reason.
        Silent,
        /// The provider rejected the turn, as opencode records it: a 200
        /// response whose message carries no text and an error.
        Rejected {
            /// Status the provider returned.
            status: u16,
            /// Whether the provider marked it worth retrying.
            retryable: bool,
        },
    }

    /// A client returning scripted replies, so the engine can be exercised
    /// with no network and no model spend.
    struct MockClient {
        replies: Mutex<Vec<Reply>>,
        prompts: Mutex<Vec<String>>,
        agents: Mutex<Vec<Option<String>>>,
        sessions_created: Mutex<usize>,
    }

    impl MockClient {
        fn new(replies: &[Reply]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().rev().cloned().collect()),
                prompts: Mutex::new(Vec::new()),
                agents: Mutex::new(Vec::new()),
                sessions_created: Mutex::new(0),
            }
        }

        fn lock<T>(guard: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
            guard.lock().expect("mock mutex must not be poisoned")
        }
    }

    #[async_trait]
    impl OpencodeClient for MockClient {
        async fn create_session(&self) -> Result<Session> {
            let mut count = Self::lock(&self.sessions_created);
            *count += 1;
            Ok(Session {
                id: format!("ses_{count}"),
                title: String::new(),
            })
        }

        async fn prompt(
            &self,
            _session_id: &str,
            text: &str,
            options: &PromptOptions,
        ) -> Result<AssistantMessage> {
            Self::lock(&self.prompts).push(text.to_string());
            Self::lock(&self.agents).push(options.agent.clone());
            Ok(AssistantMessage::default())
        }

        async fn messages(&self, _session_id: &str) -> Result<Vec<AssistantMessage>> {
            let reply = Self::lock(&self.replies)
                .pop()
                .unwrap_or(Reply::Text("```json\n{\"credence\": 50}\n```"));

            // Shaped like a real turn: a user message, then a tool-bearing
            // assistant message, then the final assistant message.
            let mut messages = vec![
                AssistantMessage {
                    info: MessageInfo {
                        role: "user".into(),
                        ..MessageInfo::default()
                    },
                    parts: Vec::new(),
                },
                AssistantMessage {
                    info: MessageInfo {
                        role: "assistant".into(),
                        cost: 0.001,
                        tokens: Tokens {
                            input: 100,
                            output: 10,
                            reasoning: 0,
                        },
                        ..MessageInfo::default()
                    },
                    parts: vec![Part::Tool {
                        id: "prt_1".into(),
                        tool: "websearch".into(),
                        state: ToolState {
                            status: "completed".into(),
                            input: serde_json::json!({"query": "evidence"}),
                            output: None,
                            error: None,
                        },
                    }],
                },
            ];

            let (text, error) = match reply {
                Reply::Text(text) => (text.to_string(), None),
                Reply::Silent => (String::new(), None),
                Reply::Rejected { status, retryable } => (
                    String::new(),
                    Some(MessageError {
                        name: "APIError".into(),
                        data: MessageErrorData {
                            message: "provider said no".into(),
                            status_code: Some(status),
                            is_retryable: Some(retryable),
                        },
                    }),
                ),
            };

            messages.push(AssistantMessage {
                info: MessageInfo {
                    role: "assistant".into(),
                    cost: 0.002,
                    tokens: Tokens {
                        input: 50,
                        output: 20,
                        reasoning: 0,
                    },
                    error,
                    ..MessageInfo::default()
                },
                parts: vec![Part::Text {
                    id: "prt_2".into(),
                    text,
                }],
            });

            Ok(messages)
        }

        async fn abort(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        async fn models(&self) -> Result<Vec<ModelRef>> {
            Ok(Vec::new())
        }
    }

    fn config(max_rounds: usize) -> DebateConfig {
        DebateConfig {
            topic: Topic::new("Is X true?", "X is true", "X is false"),
            max_rounds,
            model_a: None,
            model_b: None,
            // Retries are exercised here, so the backoff is removed rather
            // than making the suite wait out a real one.
            retry: RetryPolicy {
                attempts: 3,
                backoff: Duration::ZERO,
            },
        }
    }

    async fn engine_with(
        replies: &[Reply],
        max_rounds: usize,
    ) -> (Engine<MockClient>, Arc<MockClient>) {
        let client = Arc::new(MockClient::new(replies));
        let engine = Engine::new(
            Arc::clone(&client),
            Box::new(crate::debate::credence::CredenceFormat),
            config(max_rounds),
        )
        .await
        .expect("engine construction must succeed");
        (engine, client)
    }

    #[tokio::test]
    async fn a_debate_runs_until_confidences_converge() {
        // Arrange: both sure of their own position, then A gives ground until
        // the two credences restate to nearly the same view. 62 + 45 is 107,
        // seven points from agreement.
        let replies = [
            Reply::Text("A opens.\n```json\n{\"credence\": 85}\n```"),
            Reply::Text("B opens.\n```json\n{\"credence\": 80}\n```"),
            Reply::Text("A concedes ground.\n```json\n{\"credence\": 62}\n```"),
            Reply::Text("B holds.\n```json\n{\"credence\": 45}\n```"),
        ];
        let (engine, _) = engine_with(&replies, 6).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert
        assert_eq!(state.stop_reason, Some(StopReason::Converged { gap: 7 }));
        assert_eq!(state.turns.len(), 4);
    }

    #[tokio::test]
    async fn the_round_cap_stops_a_debate_that_never_converges() {
        // Arrange: both sides stay certain of their own opposing position, so
        // they never approach agreement. 95 + 95 is 90 points from it.
        let replies = [
            Reply::Text("A.\n```json\n{\"credence\": 95}\n```"),
            Reply::Text("B.\n```json\n{\"credence\": 95}\n```"),
            Reply::Text("A.\n```json\n{\"credence\": 95}\n```"),
            Reply::Text("B.\n```json\n{\"credence\": 95}\n```"),
        ];
        let (engine, _) = engine_with(&replies, 2).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert
        assert_eq!(state.stop_reason, Some(StopReason::RoundCap));
        assert_eq!(state.completed_rounds(), 2);
    }

    #[tokio::test]
    async fn each_side_is_prompted_as_its_own_agent() {
        // Arrange
        let (engine, client) = engine_with(&[], 1).await;

        // Act
        engine.run(|_| {}).await.expect("debate must run");

        // Assert: personas come from agent definitions, so the agent name
        // must alternate or both sides argue identically.
        let agents = MockClient::lock(&client.agents).clone();
        assert_eq!(
            agents,
            vec![Some("debater-a".to_string()), Some("debater-b".to_string())]
        );
    }

    #[tokio::test]
    async fn a_turn_aggregates_cost_and_tools_across_its_messages() {
        // Arrange: the mock's turn spans two assistant messages, one of which
        // holds the tool call and part of the cost.
        let (engine, _) = engine_with(&[], 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert: reading only the prompt response would have given 0.002 and
        // no tools.
        let turn = state.turns.first().expect("a turn must be recorded");
        assert!((turn.cost - 0.003).abs() < 1e-9, "cost was {}", turn.cost);
        assert_eq!(turn.tokens.input, 150);
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].tool, "websearch");
        assert_eq!(turn.tool_calls[0].detail, "evidence");
    }

    #[tokio::test]
    async fn prose_reaches_the_transcript_without_its_json_block() {
        // Arrange
        let (engine, _) = engine_with(
            &[Reply::Text(
                "My argument.\n```json\n{\"credence\": 70}\n```",
            )],
            1,
        )
        .await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert
        assert_eq!(state.turns[0].text, "My argument.");
        assert!(!state.turns[0].text.contains("credence"));
    }

    #[tokio::test]
    async fn a_turn_without_structure_still_records() {
        // Arrange: the model ignored the format instruction.
        let (engine, _) = engine_with(&[Reply::Text("Just prose, no block.")], 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert: the debate continues rather than failing.
        assert_eq!(state.turns[0].text, "Just prose, no block.");
        assert!(!state.turns[0].analysis.parse_status.is_ok());
    }

    #[tokio::test]
    async fn a_silent_turn_is_retried_until_the_side_speaks() {
        // Arrange: the provider throttles the first attempt, drops the second
        // without saying why, then answers.
        let replies = [
            Reply::Rejected {
                status: 429,
                retryable: true,
            },
            Reply::Silent,
            Reply::Text("A finally speaks.\n```json\n{\"credence\": 80}\n```"),
        ];
        let (engine, client) = engine_with(&replies, 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert: the turn that reached the transcript is the one with content,
        // and the two dead attempts left nothing behind.
        assert_eq!(state.turns[0].text, "A finally speaks.");
        assert_eq!(
            MockClient::lock(&client.prompts).len(),
            4,
            "3 for A, 1 for B"
        );
    }

    #[tokio::test]
    async fn a_side_that_never_speaks_ends_the_debate_with_what_it_has() {
        // Arrange: A opens, B answers, then A goes silent for good.
        let replies = [
            Reply::Text("A opens.\n```json\n{\"credence\": 80}\n```"),
            Reply::Text("B answers.\n```json\n{\"credence\": 80}\n```"),
            Reply::Silent,
            Reply::Silent,
            Reply::Silent,
        ];
        let (engine, _) = engine_with(&replies, 6).await;

        // Act
        let state = engine
            .run(|_| {})
            .await
            .expect("a failed turn must not fail the run");

        // Assert: the two completed turns survive, and the transcript records
        // which side stopped and why.
        assert_eq!(state.turns.len(), 2);
        match state.stop_reason {
            Some(StopReason::Failed { side, ref message }) => {
                assert_eq!(side, Side::A);
                assert!(
                    message.contains("after 3 attempts"),
                    "message was: {message}"
                );
            }
            other => panic!("expected a failed stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refusal_that_retrying_cannot_fix_is_not_retried() {
        // Arrange: 402, out of credit. Waiting only arrives at the same answer
        // several minutes later.
        let (engine, client) = engine_with(
            &[Reply::Rejected {
                status: 402,
                retryable: false,
            }],
            2,
        )
        .await;

        // Act
        let state = engine
            .run(|_| {})
            .await
            .expect("a failed turn must not fail the run");

        // Assert: one attempt only, and the provider's own words are kept.
        assert_eq!(MockClient::lock(&client.prompts).len(), 1);
        match state.stop_reason {
            Some(StopReason::Failed { ref message, .. }) => {
                assert!(message.contains("402"), "message was: {message}");
                assert!(
                    message.contains("provider said no"),
                    "message was: {message}"
                );
            }
            other => panic!("expected a failed stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_turn_with_prose_but_no_structure_is_not_retried() {
        // Arrange: the model argued but ignored the format instruction. The
        // argument is worth keeping, and re-rolling it would pay twice to
        // discard a real turn.
        let (engine, client) = engine_with(&[Reply::Text("Just prose, no block.")], 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert
        assert_eq!(state.turns[0].text, "Just prose, no block.");
        assert_eq!(MockClient::lock(&client.prompts).len(), 2, "one turn each");
    }

    #[tokio::test]
    async fn progress_is_reported_for_every_turn_and_the_ending() {
        // Arrange
        let (engine, _) = engine_with(&[], 1).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);

        // Act
        engine
            .run(move |progress| {
                if let Ok(mut log) = sink.lock() {
                    log.push(progress);
                }
            })
            .await
            .expect("debate must run");

        // Assert: two starts, two completions, one finish.
        let log = MockClient::lock(&events);
        assert_eq!(log.len(), 5);
        assert!(matches!(
            log[0],
            Progress::TurnStarted { side: Side::A, .. }
        ));
        assert!(matches!(
            log[2],
            Progress::TurnStarted { side: Side::B, .. }
        ));
        assert!(matches!(log[4], Progress::Finished { .. }));
    }

    #[tokio::test]
    async fn the_opponents_argument_is_carried_into_the_next_prompt() {
        // Arrange
        let (engine, client) = engine_with(
            &[Reply::Text(
                "A's distinctive point.\n```json\n{\"credence\": 80}\n```",
            )],
            1,
        )
        .await;

        // Act
        engine.run(|_| {}).await.expect("debate must run");

        // Assert: B must see what A said, or the sides are not debating.
        let prompts = MockClient::lock(&client.prompts).clone();
        assert!(
            prompts[1].contains("A's distinctive point"),
            "B's prompt was: {}",
            prompts[1]
        );
    }

    #[test]
    fn collect_turn_takes_only_messages_after_the_last_user_message() {
        // Arrange: a prior turn that must not be counted twice.
        let messages = vec![
            AssistantMessage {
                info: MessageInfo {
                    role: "user".into(),
                    ..MessageInfo::default()
                },
                parts: Vec::new(),
            },
            AssistantMessage {
                info: MessageInfo {
                    role: "assistant".into(),
                    cost: 5.0,
                    ..MessageInfo::default()
                },
                parts: vec![Part::Text {
                    id: "old".into(),
                    text: "stale".into(),
                }],
            },
            AssistantMessage {
                info: MessageInfo {
                    role: "user".into(),
                    ..MessageInfo::default()
                },
                parts: Vec::new(),
            },
            AssistantMessage {
                info: MessageInfo {
                    role: "assistant".into(),
                    cost: 1.0,
                    ..MessageInfo::default()
                },
                parts: vec![Part::Text {
                    id: "new".into(),
                    text: "fresh".into(),
                }],
            },
        ];

        // Act
        let turn = collect_turn(&messages);

        // Assert
        assert_eq!(turn.text, "fresh");
        assert!((turn.cost - 1.0).abs() < 1e-9);
    }
}
