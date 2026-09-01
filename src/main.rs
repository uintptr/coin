//! Command line entry point for coin.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::time::timeout;
use tracing::error;
use tracing_subscriber::EnvFilter;

use coin::config::{OpencodeConfig, data_dir, debate_dir};
use coin::debate::engine::{DebateConfig, Engine, Progress};
use coin::debate::format::FormatId;
use coin::debate::format_for;
use coin::debate::state::{Credence, DebateState, Side, StopReason, Topic};
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

    /// Run a debate between two models and print the transcript as it happens.
    Debate {
        /// The question under dispute.
        #[arg(short, long)]
        question: String,

        /// The case assigned to side A.
        #[arg(short = 'a', long)]
        position_a: String,

        /// The case assigned to side B.
        #[arg(short = 'b', long)]
        position_b: String,

        /// Debate format.
        #[arg(short, long, default_value = "credence")]
        format: FormatId,

        /// Hard cap on rounds.
        #[arg(short = 'r', long, default_value_t = 3)]
        max_rounds: usize,

        /// Model for both sides, in `provider/model` form.
        #[arg(short, long, value_parser = parse_model)]
        model: Option<ModelRef>,

        /// Model for side B, when the two sides should differ.
        #[arg(long, value_parser = parse_model)]
        model_b: Option<ModelRef>,

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
        Command::Debate {
            question,
            position_a,
            position_b,
            format,
            max_rounds,
            model,
            model_b,
            no_websearch,
        } => {
            let config = DebateConfig {
                topic: Topic::new(question, position_a, position_b),
                max_rounds,
                model_a: model.clone(),
                model_b: model_b.or(model),
            };
            debate(config, format, !no_websearch).await
        }
    }
}

/// Run a debate and print it as it unfolds.
async fn debate(config: DebateConfig, format_id: FormatId, websearch: bool) -> Result<()> {
    let Some(format) = format_for(format_id) else {
        return Err(CoinError::EventStream(format!(
            "format {format_id} is specified but not implemented yet; \
             only 'credence' is available so far"
        )));
    };

    // Each debate gets a disposable workspace, so a debater's tool use lands
    // somewhere throwaway rather than in a real project.
    let id = uuid::Uuid::new_v4();
    let directory = workspace::prepare(debate_dir(id.to_string())?).await?;

    // Agent files are read at server startup and replace the built-in system
    // prompt, so both personas must be written before launching.
    for side in [Side::A, Side::B] {
        workspace::write_agent(
            &directory,
            format!("debater-{}", side.label()),
            format!("Debate side {side}"),
            format.system_prompt(side, &config.topic),
        )
        .await?;
    }

    let mut opencode = OpencodeConfig::new(&directory);
    opencode.enable_websearch = websearch;

    eprintln!("Launching opencode...");
    let server = OpencodeServer::launch(&opencode).await?;
    let client = Arc::new(HttpClient::new(
        server.base_url(),
        server.username(),
        server.password(),
    ));

    println!("QUESTION  {}", config.topic.question);
    println!("SIDE A    {}", config.topic.position_a);
    println!("SIDE B    {}", config.topic.position_b);
    println!(
        "FORMAT    {format_id} ({}), max {} rounds\n",
        format_id.stop_description(),
        config.max_rounds
    );

    let engine = Engine::new(Arc::clone(&client), format, config).await?;

    let state = engine
        .run(|progress| {
            if let Progress::TurnStarted { side, round } = progress {
                println!("--- Round {round}, Debater {side} ---");
                let _ = std::io::stdout().flush();
            }
        })
        .await?;

    for turn in &state.turns {
        print_turn(turn);
    }

    print_summary(&state);
    server.shutdown().await
}

/// Print one completed turn.
fn print_turn(turn: &coin::debate::state::Turn) {
    println!("\n=== Round {} | Debater {} ===", turn.round, turn.side);

    for call in &turn.tool_calls {
        if call.detail.is_empty() {
            println!("  [{} {}]", call.status, call.tool);
        } else {
            println!("  [{} {}: {}]", call.status, call.tool, call.detail);
        }
    }

    println!("{}", turn.text);

    if let Some(credence) = turn.analysis.credence {
        println!("  confidence: {credence}");
    }
    if let Some(reason) = &turn.analysis.moved_because {
        println!("  moved because: {reason}");
    }
    for conceded in &turn.analysis.conceded {
        println!("  conceded: {conceded}");
    }
    if !turn.analysis.parse_status.is_ok() {
        println!("  (no readable structured block in this turn)");
    }
}

/// Print the closing summary, including the convergence series.
fn print_summary(state: &DebateState) {
    println!("\n=== Result ===");

    let series_a = state.credence_series(Side::A);
    let series_b = state.credence_series(Side::B);

    if !series_a.is_empty() || !series_b.is_empty() {
        // Each side reports confidence in its own position, so the gap column
        // restates them on a single proposition. Without it, two sides that
        // fully agree look maximally far apart.
        println!("confidence in own position, and how far apart that leaves them:");
        for round in 0..series_a.len().max(series_b.len()) {
            let format_value = |series: &[Credence]| {
                series
                    .get(round)
                    .map_or_else(|| "  -".to_string(), |value| format!("{value:>3}"))
            };
            let gap = match (series_a.get(round), series_b.get(round)) {
                (Some(a), Some(b)) => format!("{:>3}", Credence::agreement_gap(*a, *b)),
                _ => "  -".to_string(),
            };
            println!(
                "  round {}   A {}   B {}   gap {}",
                round + 1,
                format_value(&series_a),
                format_value(&series_b),
                gap,
            );
        }
    }

    match &state.stop_reason {
        Some(StopReason::Converged { gap }) => {
            println!("ended: confidences converged, {gap} points apart");
        }
        Some(StopReason::Conceded { side }) => println!("ended: Debater {side} conceded"),
        Some(StopReason::RoundCap) => println!("ended: round cap reached without convergence"),
        Some(other) => println!("ended: {other:?}"),
        None => println!("ended: unknown"),
    }

    let tokens = state.total_tokens();
    println!(
        "{} turns | {} in, {} out | ${:.4}",
        state.turns.len(),
        tokens.input,
        tokens.output,
        state.total_cost()
    );
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
