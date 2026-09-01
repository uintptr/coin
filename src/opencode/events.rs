//! Consumer for the opencode `GET /event` SSE bus.
//!
//! opencode publishes every session's activity on one stream. This module
//! decodes that stream into [`ServerEvent`] values and hands them to a caller
//! supplied handler. Filtering by session is the caller's job, because a debate
//! runs three sessions concurrently and each needs routing to its own column.
//!
//! Events that fail to decode are logged and skipped rather than terminating
//! the stream. opencode carries many event types coin does not model, and a
//! new one appearing upstream must not stop a running debate.

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use tracing::{debug, trace, warn};

use crate::error::{CoinError, Result};
use crate::opencode::client::HttpClient;
use crate::opencode::types::ServerEvent;

/// Outcome of handling one event, controlling whether the stream continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep consuming events.
    Continue,
    /// Stop consuming and return.
    Stop,
}

/// Consume the server event stream until the handler asks to stop.
///
/// # Arguments
///
/// * `client` - Client supplying the base URL and credentials
/// * `handler` - Called for each decoded event; returns whether to continue
///
/// # Returns
///
/// `Ok(())` once the handler returns [`Flow::Stop`].
///
/// # Errors
///
/// Returns [`CoinError::Http`] if the stream cannot be opened and
/// [`CoinError::EventStream`] if it ends before the handler stops it.
///
/// # Examples
///
/// ```no_run
/// # use coin::opencode::client::HttpClient;
/// # use coin::opencode::events::{stream_events, Flow};
/// # use coin::opencode::types::ServerEvent;
/// # async fn run(client: &HttpClient) -> coin::error::Result<()> {
/// stream_events(client, |event| match event {
///     ServerEvent::SessionIdle(_) => Flow::Stop,
///     _ => Flow::Continue,
/// })
/// .await
/// # }
/// ```
pub async fn stream_events<F>(client: &HttpClient, mut handler: F) -> Result<()>
where
    F: FnMut(ServerEvent) -> Flow + Send,
{
    let (username, password) = client.credentials();
    let url = format!("{}/event", client.base_url());

    debug!(url = %url, "opening opencode event stream");

    let response = reqwest::Client::new()
        .get(&url)
        .basic_auth(username, Some(password))
        .send()
        .await?
        .error_for_status()?;

    let mut stream = response.bytes_stream().eventsource();

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(event) => event,
            Err(err) => {
                return Err(CoinError::EventStream(err.to_string()));
            }
        };

        // opencode sends periodic keepalives with empty data.
        if event.data.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<ServerEvent>(&event.data) {
            Ok(decoded) => {
                trace!(?decoded, "decoded opencode event");
                if handler(decoded) == Flow::Stop {
                    return Ok(());
                }
            }
            Err(err) => {
                // Decoding failures are expected as opencode evolves. Log and
                // continue; dropping one event must not end the debate.
                warn!(error = %err, data = %event.data, "skipping undecodable event");
            }
        }
    }

    Err(CoinError::EventStream(
        "opencode closed the event stream unexpectedly".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_stop_is_distinct_from_continue() {
        // Guards against a refactor collapsing the enum into a bool.
        assert_ne!(Flow::Continue, Flow::Stop);
    }

    #[test]
    fn session_idle_is_recognised_as_the_completion_signal() {
        // Arrange: the payload shape opencode publishes on turn completion.
        let raw = r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#;

        // Act
        let event: ServerEvent = serde_json::from_str(raw).expect("must decode");

        // Assert
        match event {
            ServerEvent::SessionIdle(session) => assert_eq!(session.session_id, "ses_1"),
            other => panic!("expected SessionIdle, got {other:?}"),
        }
    }
}
