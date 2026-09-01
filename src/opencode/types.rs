//! Serde models for the subset of the opencode REST and event surface we use.
//!
//! opencode exposes roughly 190 operations across two coexisting API surfaces:
//! flat legacy routes and a v2 surface under `/api/*` that wraps responses in
//! `{"data": ...}`. Only the parts coin actually depends on are modelled here.
//!
//! Every struct sets `#[serde(default)]` on optional fields rather than
//! requiring them. opencode is an actively developed upstream, and a response
//! gaining a field must not break deserialization.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Envelope used by the v2 `/api/*` routes.
#[derive(Debug, Clone, Deserialize)]
pub struct V2Envelope<T> {
    /// The wrapped payload.
    pub data: T,
}

/// Response from `GET /api/health`.
#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    /// Whether the server is ready to accept API requests.
    pub healthy: bool,
}

/// A session as returned by `POST /session`.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    /// Stable session identifier, for example `ses_...`.
    pub id: String,
    /// Human-readable title assigned by opencode.
    #[serde(default)]
    pub title: String,
}

/// Token counts reported alongside an assistant message.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Tokens {
    /// Prompt tokens consumed.
    #[serde(default)]
    pub input: u64,
    /// Completion tokens produced.
    #[serde(default)]
    pub output: u64,
    /// Reasoning tokens, for models that report them separately.
    #[serde(default)]
    pub reasoning: u64,
}

/// Metadata attached to a completed assistant message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageInfo {
    /// Message identifier, for example `msg_...`.
    #[serde(default)]
    pub id: String,
    /// Session the message belongs to.
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
    /// `user` or `assistant`.
    #[serde(default)]
    pub role: String,
    /// Model that produced the message.
    #[serde(default, rename = "modelID")]
    pub model_id: String,
    /// Provider that served the model.
    #[serde(default, rename = "providerID")]
    pub provider_id: String,
    /// Accumulated cost in USD.
    #[serde(default)]
    pub cost: f64,
    /// Token accounting for the message.
    #[serde(default)]
    pub tokens: Tokens,
}

/// One part of a message: text, a tool invocation, or a step marker.
///
/// The `type` field is the discriminant. Unrecognized variants deserialize to
/// [`Part::Other`] rather than failing, so a new upstream part type cannot
/// break a running debate.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Part {
    /// Free text produced by the model.
    #[serde(rename = "text")]
    Text {
        /// Part identifier.
        #[serde(default)]
        id: String,
        /// Text content accumulated so far.
        #[serde(default)]
        text: String,
    },
    /// A tool invocation and its state.
    #[serde(rename = "tool")]
    Tool {
        /// Part identifier.
        #[serde(default)]
        id: String,
        /// Tool name, for example `bash` or `websearch`.
        #[serde(default)]
        tool: String,
        /// Invocation state, including status and input.
        #[serde(default)]
        state: ToolState,
    },
    /// Marks the beginning of a reasoning or tool step.
    #[serde(rename = "step-start")]
    StepStart {
        /// Part identifier.
        #[serde(default)]
        id: String,
    },
    /// Any part type coin does not model.
    #[serde(other)]
    Other,
}

/// State of a tool invocation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolState {
    /// One of `pending`, `running`, `completed`, or `error`.
    #[serde(default)]
    pub status: String,
    /// Arguments the model passed to the tool.
    #[serde(default)]
    pub input: serde_json::Value,
    /// Tool output, present once the call completes.
    #[serde(default)]
    pub output: Option<String>,
    /// Error text, present when the call failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// A completed assistant message returned by `POST /session/{id}/message`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AssistantMessage {
    /// Message metadata including cost and token counts.
    #[serde(default)]
    pub info: MessageInfo,
    /// Ordered parts making up the message.
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl AssistantMessage {
    /// Concatenate all text parts into a single string.
    ///
    /// # Returns
    ///
    /// The message's text content with non-text parts omitted.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text, .. } => Some(text.as_str()),
                Part::Tool { .. } | Part::StepStart { .. } | Part::Other => None,
            })
            .collect()
    }
}

/// Request body for `POST /session/{id}/message`.
#[derive(Debug, Clone, Serialize)]
pub struct PromptRequest {
    /// Message parts to send. Only text parts are used by coin.
    pub parts: Vec<PromptPart>,
    /// Agent to answer as, selecting the persona.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model in `provider/model` form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A single outgoing message part.
#[derive(Debug, Clone, Serialize)]
pub struct PromptPart {
    /// Part discriminant. Always `text` for coin.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Text to send.
    pub text: String,
}

impl PromptPart {
    /// Build a text part.
    ///
    /// # Arguments
    ///
    /// * `text` - Content of the part
    ///
    /// # Returns
    ///
    /// A part tagged as `text`.
    pub fn text<S>(text: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// An event from the `GET /event` SSE bus.
///
/// opencode publishes many event types; coin models the ones it acts on and
/// collapses the rest into [`ServerEvent::Other`].
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// Incremental text appended to a part. The primary streaming signal.
    PartDelta(PartDelta),
    /// A part reached a new state, used to observe tool invocations.
    PartUpdated(PartUpdated),
    /// A session finished its turn. The authoritative completion signal.
    SessionIdle(SessionRef),
    /// A session failed.
    SessionError(SessionErrorEvent),
    /// A tool is requesting permission to run.
    PermissionAsked(serde_json::Value),
    /// Any event type coin does not model, or a known type whose payload did
    /// not decode.
    Other,
}

/// Wire shape of every event on the bus, before the payload is interpreted.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: serde_json::Value,
}

impl<'de> Deserialize<'de> for ServerEvent {
    /// Decode an event by dispatching on its `type` tag.
    ///
    /// This is written by hand rather than derived because serde's
    /// `#[serde(other)]` fallback requires a unit variant, and an adjacently
    /// tagged enum still tries to decode the `properties` map into it. That
    /// makes every unmodelled event a hard error, which is the opposite of
    /// what coin needs: opencode publishes many event types and adds more over
    /// time, and an unrecognized one must never interrupt a running debate.
    ///
    /// A known tag whose payload fails to decode also degrades to
    /// [`ServerEvent::Other`], for the same reason.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawEvent::deserialize(deserializer)?;

        /// Decode a payload, degrading to `Other` rather than failing.
        fn decode<T, F>(properties: serde_json::Value, wrap: F) -> ServerEvent
        where
            T: DeserializeOwned,
            F: FnOnce(T) -> ServerEvent,
        {
            serde_json::from_value(properties).map_or(ServerEvent::Other, wrap)
        }

        Ok(match raw.kind.as_str() {
            "message.part.delta" => decode(raw.properties, ServerEvent::PartDelta),
            "message.part.updated" => decode(raw.properties, ServerEvent::PartUpdated),
            "session.idle" => decode(raw.properties, ServerEvent::SessionIdle),
            "session.error" => decode(raw.properties, ServerEvent::SessionError),
            "permission.asked" => ServerEvent::PermissionAsked(raw.properties),
            _ => ServerEvent::Other,
        })
    }
}

/// Payload of `message.part.delta`, the primary streaming signal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartDelta {
    /// Session the delta belongs to.
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
    /// Message being appended to.
    #[serde(default, rename = "messageID")]
    pub message_id: String,
    /// Part being appended to.
    #[serde(default, rename = "partID")]
    pub part_id: String,
    /// Which field of the part this delta extends.
    ///
    /// Observed values are `text` for visible output and `reasoning` for
    /// thinking tokens. Debate transcripts want the former only.
    #[serde(default)]
    pub field: String,
    /// The fragment appended by this delta.
    #[serde(default)]
    pub delta: String,
}

impl PartDelta {
    /// Whether this delta extends the visible text of a message.
    ///
    /// # Returns
    ///
    /// `true` for `text` deltas, `false` for reasoning and any other field.
    pub fn is_text(&self) -> bool {
        self.field == "text"
    }
}

/// Payload of `message.part.updated`.
#[derive(Debug, Clone, Deserialize)]
pub struct PartUpdated {
    /// The updated part.
    pub part: PartEnvelope,
}

/// A part together with the session it belongs to.
#[derive(Debug, Clone, Deserialize)]
pub struct PartEnvelope {
    /// Session the part belongs to.
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
    /// The part itself.
    #[serde(flatten)]
    pub part: Part,
}

/// Reference to a session, used by events carrying no other payload.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionRef {
    /// Session identifier.
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
}

/// Payload of `session.error`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionErrorEvent {
    /// Session that failed, when reported.
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
    /// Error detail, shape varies by cause.
    #[serde(default)]
    pub error: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_message_concatenates_only_text_parts() {
        // Arrange: a message mixing a step marker, text, and a tool call.
        let raw = serde_json::json!({
            "info": { "id": "msg_1", "sessionID": "ses_1", "cost": 0.02 },
            "parts": [
                { "type": "step-start", "id": "prt_0" },
                { "type": "text", "id": "prt_1", "text": "hello " },
                { "type": "tool", "id": "prt_2", "tool": "bash",
                  "state": { "status": "completed", "output": "ignored" } },
                { "type": "text", "id": "prt_3", "text": "world" }
            ]
        });

        // Act
        let message: AssistantMessage =
            serde_json::from_value(raw).expect("message fixture must deserialize");

        // Assert
        assert_eq!(message.text(), "hello world");
        assert_eq!(message.info.cost, 0.02);
    }

    #[test]
    fn unknown_part_type_does_not_fail_deserialization() {
        // Arrange: a part type coin does not model.
        let raw = serde_json::json!({
            "parts": [{ "type": "some-future-part", "id": "prt_9" }]
        });

        // Act
        let message: AssistantMessage =
            serde_json::from_value(raw).expect("unknown parts must degrade, not fail");

        // Assert
        assert!(matches!(message.parts.as_slice(), [Part::Other]));
    }

    #[test]
    fn unknown_event_type_deserializes_to_other() {
        // Arrange
        let raw = r#"{"type":"session.archive","properties":{"sessionID":"ses_1"}}"#;

        // Act
        let event: ServerEvent =
            serde_json::from_str(raw).expect("unknown events must degrade, not fail");

        // Assert
        assert!(matches!(event, ServerEvent::Other));
    }

    #[test]
    fn known_event_with_unusable_payload_degrades_to_other() {
        // Arrange: the tag is one we model, but the payload is the wrong shape.
        let raw = r#"{"type":"session.idle","properties":"not-an-object"}"#;

        // Act
        let event: ServerEvent =
            serde_json::from_str(raw).expect("a bad payload must degrade, not fail");

        // Assert
        assert!(matches!(event, ServerEvent::Other));
    }

    #[test]
    fn event_without_properties_still_decodes() {
        // Arrange: some events carry no payload at all.
        let raw = r#"{"type":"session.archive"}"#;

        // Act
        let event: ServerEvent = serde_json::from_str(raw).expect("must decode");

        // Assert
        assert!(matches!(event, ServerEvent::Other));
    }

    #[test]
    fn part_delta_matches_the_shape_opencode_emits() {
        // Arrange: captured verbatim from opencode 1.18.20's /event stream.
        let raw = r#"{
            "id":"evt_1",
            "type":"message.part.delta",
            "properties":{
                "sessionID":"ses_1",
                "messageID":"msg_1",
                "partID":"prt_1",
                "field":"text",
                "delta":"The"
            }
        }"#;

        // Act
        let event: ServerEvent = serde_json::from_str(raw).expect("delta must deserialize");

        // Assert
        match event {
            ServerEvent::PartDelta(delta) => {
                assert_eq!(delta.session_id, "ses_1");
                assert_eq!(delta.message_id, "msg_1");
                assert_eq!(delta.delta, "The");
                assert!(delta.is_text());
            }
            other => panic!("expected a PartDelta, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_deltas_are_not_text() {
        // Arrange: thinking tokens arrive on the same event with a different
        // field, and must not be mixed into the visible transcript.
        let raw = r#"{
            "type":"message.part.delta",
            "properties":{"sessionID":"ses_1","field":"reasoning","delta":"hmm"}
        }"#;

        // Act
        let event: ServerEvent = serde_json::from_str(raw).expect("delta must deserialize");

        // Assert
        match event {
            ServerEvent::PartDelta(delta) => assert!(!delta.is_text()),
            other => panic!("expected a PartDelta, got {other:?}"),
        }
    }
}
