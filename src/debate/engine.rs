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

use tracing::{debug, info, warn};

use crate::debate::format::DebateFormat;
use crate::debate::state::{DebateState, Side, StopReason, Topic};
use crate::error::Result;
use crate::opencode::client::{OpencodeClient, PromptOptions};
use crate::opencode::types::{AssistantMessage, ModelRef, Tokens, ToolCall};

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
    TurnCompleted {
        /// Index of the recorded turn.
        index: usize,
    },
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

    /// Run one side's turn and record it.
    async fn take_turn(&self, state: &mut DebateState, side: Side) -> Result<usize> {
        let session = self.session(side);
        let prompt = self.format.turn_prompt(state, side);

        let options = PromptOptions {
            agent: Some(Self::agent_name(side)),
            model: self.config.model(side),
        };

        self.client.prompt(session, &prompt, &options).await?;

        // Re-read the session rather than trusting the prompt response, which
        // carries only the final message of a multi-message turn.
        let messages = self.client.messages(session).await?;
        let raw = collect_turn(&messages);

        if raw.text.trim().is_empty() {
            warn!(%side, "turn produced no visible text");
        }

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
    /// The final state, with [`DebateState::stop_reason`] set.
    ///
    /// # Errors
    ///
    /// Propagates transport and server failures from the client.
    pub async fn run<F>(&self, mut on_progress: F) -> Result<DebateState>
    where
        F: FnMut(Progress) + Send,
    {
        let mut state = DebateState::new(self.config.topic.clone(), self.config.max_rounds);

        let reason = loop {
            let side = state.next_side();
            let round = state.current_round();

            on_progress(Progress::TurnStarted { side, round });
            let index = self.take_turn(&mut state, side).await?;
            on_progress(Progress::TurnCompleted { index });

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
    use crate::opencode::types::{MessageInfo, Part, Session, ToolState};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A client returning scripted replies, so the engine can be exercised
    /// with no network and no model spend.
    struct MockClient {
        replies: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
        agents: Mutex<Vec<Option<String>>>,
        sessions_created: Mutex<usize>,
    }

    impl MockClient {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().rev().map(|s| s.to_string()).collect()),
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
                .unwrap_or_else(|| "```json\n{\"credence\": 50}\n```".to_string());

            // Shaped like a real turn: a user message, then a tool-bearing
            // assistant message, then the final assistant message.
            Ok(vec![
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
                AssistantMessage {
                    info: MessageInfo {
                        role: "assistant".into(),
                        cost: 0.002,
                        tokens: Tokens {
                            input: 50,
                            output: 20,
                            reasoning: 0,
                        },
                        ..MessageInfo::default()
                    },
                    parts: vec![Part::Text {
                        id: "prt_2".into(),
                        text: reply,
                    }],
                },
            ])
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
        }
    }

    async fn engine_with(
        replies: &[&str],
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
            "A opens.\n```json\n{\"credence\": 85}\n```",
            "B opens.\n```json\n{\"credence\": 80}\n```",
            "A concedes ground.\n```json\n{\"credence\": 62}\n```",
            "B holds.\n```json\n{\"credence\": 45}\n```",
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
            "A.\n```json\n{\"credence\": 95}\n```",
            "B.\n```json\n{\"credence\": 95}\n```",
            "A.\n```json\n{\"credence\": 95}\n```",
            "B.\n```json\n{\"credence\": 95}\n```",
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
        let (engine, _) = engine_with(&["My argument.\n```json\n{\"credence\": 70}\n```"], 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert
        assert_eq!(state.turns[0].text, "My argument.");
        assert!(!state.turns[0].text.contains("credence"));
    }

    #[tokio::test]
    async fn a_turn_without_structure_still_records() {
        // Arrange: the model ignored the format instruction.
        let (engine, _) = engine_with(&["Just prose, no block."], 1).await;

        // Act
        let state = engine.run(|_| {}).await.expect("debate must run");

        // Assert: the debate continues rather than failing.
        assert_eq!(state.turns[0].text, "Just prose, no block.");
        assert!(!state.turns[0].analysis.parse_status.is_ok());
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
            &["A's distinctive point.\n```json\n{\"credence\": 80}\n```"],
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
