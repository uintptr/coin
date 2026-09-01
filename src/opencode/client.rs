//! HTTP client for the opencode server.
//!
//! The [`OpencodeClient`] trait is the seam the debate engine is written
//! against, so the engine can be tested against a mock with no network and no
//! model spend. [`HttpClient`] is the real implementation.
//!
//! Rust note for Python developers: `#[async_trait]` rewrites the trait's
//! `async fn`s to return boxed futures. Plain `async fn` in a trait is not
//! usable behind `dyn`, which is exactly what mocking needs. The cost is one
//! small allocation per call, negligible beside an HTTP round trip to a model.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::error::{CoinError, Result};
use crate::opencode::types::{
    AssistantMessage, ModelRef, PromptPart, PromptRequest, ProvidersResponse, Session,
};

/// Maximum number of body bytes retained when reporting an HTTP error.
const ERROR_BODY_LIMIT: usize = 512;

/// Options for a single prompt.
#[derive(Debug, Clone, Default)]
pub struct PromptOptions {
    /// Agent to answer as, which selects the persona.
    pub agent: Option<String>,
    /// Model to answer with. Falls back to the server default when unset.
    pub model: Option<ModelRef>,
}

/// Operations coin performs against an opencode server.
#[async_trait]
pub trait OpencodeClient: Send + Sync {
    /// Create a new session.
    ///
    /// # Returns
    ///
    /// The created session, including the identifier used by later calls.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Http`] on transport failure and
    /// [`CoinError::OpencodeStatus`] if the server rejects the request.
    async fn create_session(&self) -> Result<Session>;

    /// Send a prompt and wait for the completed assistant message.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Session to prompt
    /// * `text` - Message text
    /// * `options` - Agent and model selection
    ///
    /// # Returns
    ///
    /// The completed message, including cost and token accounting.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Http`] on transport failure and
    /// [`CoinError::OpencodeStatus`] if the server rejects the request.
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        options: &PromptOptions,
    ) -> Result<AssistantMessage>;

    /// Fetch every message in a session, oldest first.
    ///
    /// [`OpencodeClient::prompt`] returns only the **last** assistant message
    /// of a turn. When a model uses tools, the turn spans several assistant
    /// messages, and the tool invocations, reasoning, and part of the cost sit
    /// in the earlier ones. Reconstructing a complete turn therefore requires
    /// the whole list.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Session to read
    ///
    /// # Returns
    ///
    /// All messages, including the user messages that delimit turns.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Http`] on transport failure.
    async fn messages(&self, session_id: &str) -> Result<Vec<AssistantMessage>>;

    /// Abort an in-flight turn.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Session whose turn should be cancelled
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Http`] on transport failure.
    async fn abort(&self, session_id: &str) -> Result<()>;

    /// List the models the server can route to.
    ///
    /// # Returns
    ///
    /// Every model across all configured providers, sorted for stable display.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Http`] on transport failure.
    async fn models(&self) -> Result<Vec<ModelRef>>;
}

/// Real client, talking to a running opencode server over HTTP.
#[derive(Debug, Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
}

impl HttpClient {
    /// Build a client for a running server.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Server base URL, for example `http://127.0.0.1:4199`
    /// * `username` - Basic auth username
    /// * `password` - Basic auth password
    ///
    /// # Returns
    ///
    /// A client ready to issue requests.
    pub fn new<U, N, P>(base_url: U, username: N, password: P) -> Self
    where
        U: AsRef<str>,
        N: AsRef<str>,
        P: AsRef<str>,
    {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.as_ref().trim_end_matches('/').to_string(),
            username: username.as_ref().to_string(),
            password: password.as_ref().to_string(),
        }
    }

    /// Base URL this client targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Basic auth credentials, for callers that open their own connections.
    ///
    /// The event stream is consumed outside this client, so it needs the same
    /// credentials.
    pub fn credentials(&self) -> (&str, &str) {
        (&self.username, &self.password)
    }

    /// Issue a request and deserialize a successful JSON response.
    ///
    /// Reads the body as text first so that a non-success status can report
    /// what the server actually said, rather than a decode error against an
    /// unexpected payload.
    async fn send<T>(&self, method: reqwest::Method, path: &str, body: Option<String>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let url = format!("{}{path}", self.base_url);
        let mut request = self
            .http
            .request(method.clone(), &url)
            .basic_auth(&self.username, Some(&self.password));

        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }

        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            let mut body = text;
            body.truncate(ERROR_BODY_LIMIT);
            return Err(CoinError::OpencodeStatus {
                method: method.to_string(),
                path: path.to_string(),
                status: status.as_u16(),
                body,
            });
        }

        serde_json::from_str(&text).map_err(|source| CoinError::decode(path.to_string(), source))
    }
}

#[async_trait]
impl OpencodeClient for HttpClient {
    async fn create_session(&self) -> Result<Session> {
        let session: Session = self
            .send(reqwest::Method::POST, "/session", Some("{}".to_string()))
            .await?;
        debug!(session_id = %session.id, "created opencode session");
        Ok(session)
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        options: &PromptOptions,
    ) -> Result<AssistantMessage> {
        let request = PromptRequest {
            parts: vec![PromptPart::text(text)],
            agent: options.agent.clone(),
            model: options.model.clone(),
        };
        let body = serde_json::to_string(&request)
            .map_err(|source| CoinError::decode("prompt request", source))?;

        self.send(
            reqwest::Method::POST,
            &format!("/session/{session_id}/message"),
            Some(body),
        )
        .await
    }

    async fn messages(&self, session_id: &str) -> Result<Vec<AssistantMessage>> {
        self.send(
            reqwest::Method::GET,
            &format!("/session/{session_id}/message"),
            None,
        )
        .await
    }

    async fn abort(&self, session_id: &str) -> Result<()> {
        // The abort route returns a bare boolean rather than an object.
        let _: serde_json::Value = self
            .send(
                reqwest::Method::POST,
                &format!("/session/{session_id}/abort"),
                Some("{}".to_string()),
            )
            .await?;
        Ok(())
    }

    async fn models(&self) -> Result<Vec<ModelRef>> {
        // `/config/providers`, not `/api/model`. The latter lists only
        // opencode's own hosted models and omits every configured provider,
        // so a client built on it sees an empty catalog for the providers the
        // user is actually authenticated against.
        let response: ProvidersResponse = self
            .send(reqwest::Method::GET, "/config/providers", None)
            .await?;

        let mut models: Vec<ModelRef> = response
            .providers
            .into_iter()
            .flat_map(|provider| {
                provider
                    .models
                    .into_keys()
                    .map(move |model_id| ModelRef::new(provider.id.clone(), model_id))
            })
            .collect();

        models.sort_by(|left, right| {
            (&left.provider_id, &left.model_id).cmp(&(&right.provider_id, &right.model_id))
        });
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_loses_its_trailing_slash() {
        // Arrange and act: paths are joined with a leading slash, so a
        // trailing slash on the base would produce a double slash.
        let client = HttpClient::new("http://127.0.0.1:4199/", "opencode", "secret");

        // Assert
        assert_eq!(client.base_url(), "http://127.0.0.1:4199");
    }

    #[test]
    fn credentials_round_trip() {
        // Arrange and act
        let client = HttpClient::new("http://127.0.0.1:4199", "opencode", "secret");

        // Assert
        assert_eq!(client.credentials(), ("opencode", "secret"));
    }

    #[test]
    fn prompt_request_omits_unset_agent_and_model() {
        // Arrange: the server rejects explicit nulls for these fields.
        let request = PromptRequest {
            parts: vec![PromptPart::text("hello")],
            agent: None,
            model: None,
        };

        // Act
        let json = serde_json::to_string(&request).expect("request must serialize");

        // Assert
        assert!(!json.contains("agent"), "agent must be omitted, got {json}");
        assert!(!json.contains("model"), "model must be omitted, got {json}");
        assert!(json.contains(r#""type":"text""#));
    }

    #[test]
    fn prompt_request_sends_the_model_as_an_object() {
        // Arrange: opencode rejects a bare string here with
        // `Expected object | null`, so the shape matters.
        let request = PromptRequest {
            parts: vec![PromptPart::text("hello")],
            agent: Some("side-a".to_string()),
            model: Some(ModelRef::new("digitalocean", "openai-gpt-oss-20b")),
        };

        // Act
        let json = serde_json::to_string(&request).expect("request must serialize");

        // Assert
        assert!(json.contains(r#""agent":"side-a""#));
        assert!(
            json.contains(
                r#""model":{"providerID":"digitalocean","modelID":"openai-gpt-oss-20b"}"#
            ),
            "model must serialize as an object, got {json}"
        );
    }
}
