//! Integration tests that drive a real `opencode serve` process.
//!
//! These are marked `#[ignore]` so the default `cargo test` stays offline,
//! fast, and free. They spend real tokens against whatever provider opencode
//! is configured for.
//!
//! Run them deliberately:
//!
//! ```text
//! cargo test --test opencode_integration -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use coin::config::OpencodeConfig;
use coin::opencode::client::{HttpClient, OpencodeClient, PromptOptions};
use coin::opencode::events::{Flow, stream_events};
use coin::opencode::process::OpencodeServer;
use coin::opencode::types::ServerEvent;
use coin::opencode::workspace;

/// Launch a server rooted at a throwaway directory.
///
/// The directory is prepared as a git repository, which opencode requires
/// before it will populate a model catalog for the project.
async fn launch() -> OpencodeServer {
    let directory = workspace::prepare(std::env::temp_dir().join("coin-integration"))
        .await
        .expect("test workspace must be preparable");

    let config = OpencodeConfig::new(&directory);
    OpencodeServer::launch(&config)
        .await
        .expect("opencode must launch; is it installed and authenticated?")
}

#[tokio::test]
#[ignore = "spawns a real opencode server and spends tokens"]
async fn server_launches_and_reports_a_loopback_address() {
    // Act
    let server = launch().await;

    // Assert: the health poll inside launch() has already passed, so reaching
    // here means port discovery and readiness both worked.
    assert!(
        server.base_url().starts_with("http://127.0.0.1:"),
        "expected a loopback address, got {}",
        server.base_url()
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
#[ignore = "spawns a real opencode server and spends tokens"]
async fn prompt_returns_a_completed_message_with_usage() {
    // Arrange
    let server = launch().await;
    let client = HttpClient::new(server.base_url(), server.username(), server.password());
    let session = client
        .create_session()
        .await
        .expect("session creation must succeed");

    // Act
    let reply = client
        .prompt(
            &session.id,
            "Reply with exactly the word: pong",
            &PromptOptions::default(),
        )
        .await
        .expect("prompt must succeed");

    // Assert
    assert!(
        reply.text().to_lowercase().contains("pong"),
        "expected the model to echo the requested word, got {:?}",
        reply.text()
    );
    assert!(
        reply.info.tokens.output > 0,
        "usage accounting must be populated"
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
#[ignore = "spawns a real opencode server and spends tokens"]
async fn event_stream_delivers_text_deltas_and_an_idle_signal() {
    // Arrange
    let server = launch().await;
    let client = HttpClient::new(server.base_url(), server.username(), server.password());
    let session = client
        .create_session()
        .await
        .expect("session creation must succeed");

    let streamed = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&streamed);
    let stream_client = client.clone();
    let watched = session.id.clone();

    // Act: consume the stream concurrently with the prompt, stopping on idle.
    let events = tokio::spawn(async move {
        stream_events(&stream_client, move |event| match event {
            ServerEvent::PartDelta(delta) if delta.session_id == watched && delta.is_text() => {
                if let Ok(mut buffer) = sink.lock() {
                    buffer.push_str(&delta.delta);
                }
                Flow::Continue
            }
            ServerEvent::SessionIdle(idle) if idle.session_id == watched => Flow::Stop,
            _ => Flow::Continue,
        })
        .await
    });

    client
        .prompt(
            &session.id,
            "Reply with exactly the word: pong",
            &PromptOptions::default(),
        )
        .await
        .expect("prompt must succeed");

    events
        .await
        .expect("event task must not panic")
        .expect("event stream must end cleanly on session.idle");

    // Assert: reaching here proves session.idle arrived, since the handler
    // stops on nothing else.
    let text = streamed.lock().expect("mutex must not be poisoned").clone();
    assert!(
        text.to_lowercase().contains("pong"),
        "expected streamed deltas to reconstruct the reply, got {text:?}"
    );

    server.shutdown().await.expect("shutdown must succeed");
}

#[tokio::test]
#[ignore = "spawns a real opencode server and spends tokens"]
async fn models_are_listed_in_provider_slash_model_form() {
    // Arrange
    let server = launch().await;
    let client = HttpClient::new(server.base_url(), server.username(), server.password());

    // Act
    let models = client.models().await.expect("model listing must succeed");

    // Assert
    assert!(!models.is_empty(), "expected at least one routable model");
    assert!(
        models.iter().all(|model| model.contains('/')),
        "models must be provider-qualified, got {models:?}"
    );

    server.shutdown().await.expect("shutdown must succeed");
}
