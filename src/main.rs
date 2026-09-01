//! Command line entry point for coin.

use std::io::Write;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::time::timeout;
use tracing::error;
use tracing_subscriber::EnvFilter;

use coin::config::{OpencodeConfig, data_dir};
use coin::error::{CoinError, Result};
use coin::opencode::client::{HttpClient, OpencodeClient, PromptOptions};
use coin::opencode::events::{Flow, stream_events};
use coin::opencode::process::OpencodeServer;
use coin::opencode::types::{ModelRef, Part, ServerEvent};
use coin::opencode::workspace;

/// How long to wait for the event stream to observe completion after the
/// prompt returns. The idle event normally arrives first, so this is a
/// backstop rather than an expected wait.
const EVENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Structured debate between two LLM debaters, streamed to a web UI.
#[derive(Debug, Parser)]
#[command(name = "coin", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Send one prompt through a spawned opencode server and stream the reply.
    ///
    /// This exercises the whole integration path: process launch, port
    /// discovery, health polling, authentication, session creation, prompting,
    /// event streaming, and clean shutdown.
    Probe {
        /// Message to send.
        message: Vec<String>,

        /// Model in `provider/model` form. Defaults to the server's choice.
        #[arg(short, long, value_parser = parse_model)]
        model: Option<ModelRef>,

        /// Disable the Exa-backed web search tool.
        #[arg(long)]
        no_websearch: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("coin=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if let Err(err) = run(cli).await {
        error!("{err}");
        std::process::exit(1);
    }
}

/// Dispatch the parsed command.
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe {
            message,
            model,
            no_websearch,
        } => probe(message.join(" "), model, !no_websearch).await,
    }
}

/// Parse a `provider/model` argument into a reference.
fn parse_model(value: &str) -> std::result::Result<ModelRef, String> {
    value.parse()
}

/// Print a tool invocation as it is observed on the event stream.
fn report_tool(part: &Part) {
    if let Part::Tool { tool, state, .. } = part {
        let detail = state
            .input
            .get("command")
            .or_else(|| state.input.get("query"))
            .or_else(|| state.input.get("url"))
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        if detail.is_empty() {
            eprintln!("\n  [{} {}]", state.status, tool);
        } else {
            eprintln!("\n  [{} {}: {}]", state.status, tool, detail);
        }
    }
}

/// Run one prompt end to end against a freshly launched opencode server.
async fn probe(message: String, model: Option<ModelRef>, websearch: bool) -> Result<()> {
    if message.trim().is_empty() {
        return Err(CoinError::EventStream(
            "a probe message is required".to_string(),
        ));
    }

    // The workspace must be a git repository or opencode reports an empty
    // model catalog. See `opencode::workspace`.
    let directory = workspace::prepare(data_dir()?.join("probe")).await?;

    let mut config = OpencodeConfig::new(&directory);
    config.enable_websearch = websearch;

    eprintln!("Launching opencode...");
    let server = OpencodeServer::launch(&config).await?;
    eprintln!("Server ready at {}", server.base_url());

    let client = HttpClient::new(server.base_url(), server.username(), server.password());
    let session = client.create_session().await?;
    eprintln!("Session {}\n", session.id);

    // Consume the event stream concurrently with the prompt so tokens appear
    // as they are produced rather than after the turn completes.
    let stream_client = client.clone();
    let watched_session = session.id.clone();
    let events = tokio::spawn(async move {
        stream_events(&stream_client, move |event| match event {
            ServerEvent::PartDelta(delta)
                if delta.session_id == watched_session && delta.is_text() =>
            {
                print!("{}", delta.delta);
                let _ = std::io::stdout().flush();
                Flow::Continue
            }
            ServerEvent::PartUpdated(update) if update.part.session_id == watched_session => {
                report_tool(&update.part.part);
                Flow::Continue
            }
            ServerEvent::SessionIdle(idle) if idle.session_id == watched_session => Flow::Stop,
            _ => Flow::Continue,
        })
        .await
    });

    let options = PromptOptions { agent: None, model };
    let reply = client.prompt(&session.id, &message, &options).await?;

    // The idle event usually arrives before the prompt returns. Bound the wait
    // so a missed event cannot hang the probe.
    match timeout(EVENT_DRAIN_TIMEOUT, events).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(err))) => error!("event stream ended with an error: {err}"),
        Ok(Err(err)) => error!("event task failed: {err}"),
        Err(_elapsed) => error!("timed out waiting for the session to go idle"),
    }

    println!();
    let info = &reply.info;
    eprintln!(
        "\n{} via {} | {} in, {} out | ${:.4}",
        info.model_id, info.provider_id, info.tokens.input, info.tokens.output, info.cost
    );

    server.shutdown().await
}
