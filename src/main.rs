//! Command line entry point for coin.

use std::io::Write;
use std::path::PathBuf;
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
use coin::debate::state::{Credence, DebateState, Side, Topic};
use coin::error::{CoinError, Result};
use coin::opencode::client::{HttpClient, OpencodeClient, PromptOptions};
use coin::opencode::events::{Flow, stream_events};
use coin::opencode::process::OpencodeServer;
use coin::opencode::types::{ModelRef, Part, ServerEvent};
use coin::opencode::workspace;
use coin::store::{self, Transcript};
use coin::term::{self, Style, paint};

/// How long to wait for the event stream to observe completion after the
/// prompt returns. The idle event normally arrives first, so this is a
/// backstop rather than an expected wait.
const EVENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Model debaters use unless one is given.
///
/// Chosen by benchmarking candidates on a real debate. It costs roughly a
/// fifteenth of the server default (kimi-k3) while still stating
/// well-calibrated confidences, which is what the credence format depends on:
/// cheaper models were faster still but reported low confidence in positions
/// the evidence plainly supported, which corrupts the convergence reading.
const DEFAULT_DEBATE_MODEL: &str = "digitalocean/glm-5.3-flash";

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

        /// Also write the transcript here. A `.json` extension saves JSON,
        /// anything else saves Markdown.
        #[arg(short = 's', long, value_name = "FILE")]
        save: Option<PathBuf>,

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
            save,
            no_websearch,
        } => {
            let default = DEFAULT_DEBATE_MODEL
                .parse()
                .map_err(|reason: String| CoinError::EventStream(reason))?;
            let model_a = model.clone().unwrap_or(default);
            let config = DebateConfig {
                topic: Topic::new(question, position_a, position_b),
                max_rounds,
                model_b: Some(model_b.or(model).unwrap_or_else(|| model_a.clone())),
                model_a: Some(model_a),
            };
            debate(config, format, !no_websearch, save).await
        }
    }
}

/// Describe which model each side will argue with.
///
/// The two sides usually share a model, which isolates the argument from model
/// capability, so that case is collapsed to a single line.
fn describe_models(config: &DebateConfig) -> String {
    let name = |model: &Option<ModelRef>| {
        model
            .as_ref()
            .map_or_else(|| "server default".to_string(), ModelRef::to_string)
    };

    if config.model_a == config.model_b {
        format!("  model {}", name(&config.model_a))
    } else {
        format!(
            "  model A {}, model B {}",
            name(&config.model_a),
            name(&config.model_b)
        )
    }
}

/// Run a debate and print it as it unfolds.
async fn debate(
    config: DebateConfig,
    format_id: FormatId,
    websearch: bool,
    save: Option<PathBuf>,
) -> Result<()> {
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

    println!("{}", paint(Style::Heading, &config.topic.question));
    println!(
        "  {} {}",
        paint(Style::SideA, "A:"),
        config.topic.position_a
    );
    println!(
        "  {} {}",
        paint(Style::SideB, "B:"),
        config.topic.position_b
    );
    println!(
        "{}",
        paint(
            Style::Dim,
            format!(
                "  {format_id} format, ends when {}, max {} rounds",
                format_id.stop_description(),
                config.max_rounds
            )
        )
    );
    println!("{}", paint(Style::Dim, describe_models(&config)));

    let config_models = (config.model_a.clone(), config.model_b.clone());
    let engine = Engine::new(Arc::clone(&client), format, config).await?;

    // Stream tokens live by consuming the opencode event bus alongside the
    // debate, routing each session's output to the side that owns it. The
    // engine stays transport-agnostic; only this rendering path knows about
    // the event stream.
    let sessions = [
        (engine.session_id(Side::A).to_string(), Side::A),
        (engine.session_id(Side::B).to_string(), Side::B),
    ];
    let stream_client = (*client).clone();
    let streamer = tokio::spawn(async move {
        stream_events(&stream_client, move |event| {
            let side_of = |id: &str| {
                sessions
                    .iter()
                    .find(|(session, _)| session == id)
                    .map(|(_, side)| *side)
            };

            match event {
                ServerEvent::PartDelta(delta) if delta.is_text() => {
                    // The style is opened once per turn, so fragments are
                    // written raw rather than wrapped individually.
                    if side_of(&delta.session_id).is_some() {
                        print!("{}", delta.delta);
                        let _ = std::io::stdout().flush();
                    }
                }
                ServerEvent::PartUpdated(update) => {
                    if let Some(side) = side_of(&update.part.session_id)
                        && let Some(line) = tool_line(&update.part.part)
                    {
                        // Interrupt the streamed argument for a tool line, then
                        // restore the side's colour for the text that follows.
                        println!(
                            "\n{}{}",
                            paint(Style::Dim, line),
                            term::start(Style::for_side(side))
                        );
                        let _ = std::io::stdout().flush();
                    }
                }
                _ => {}
            }
            Flow::Continue
        })
        .await
    });

    let state = engine
        .run(|progress| match progress {
            Progress::TurnStarted { side, round } => {
                // Open the side's colour here so streamed fragments inherit it.
                print!(
                    "\n{}\n{}",
                    paint(
                        Style::for_side(side),
                        format!("--- Round {round}, Debater {side} ---")
                    ),
                    term::start(Style::for_side(side))
                );
                let _ = std::io::stdout().flush();
            }
            Progress::TurnCompleted(turn) => print_turn_analysis(&turn),
            Progress::Finished { .. } => {}
        })
        .await?;

    // The debate is over, so nothing further will arrive on the stream.
    streamer.abort();

    print_summary(&state);

    // Every debate is saved without being asked for: one costs real money and
    // several minutes, and the interesting part is often a single concession
    // buried mid-argument.
    let transcript = Transcript::new(
        &state,
        format_id,
        config_models.0.as_ref(),
        config_models.1.as_ref(),
    );
    let saved = store::save_to_dir(&directory, &transcript).await?;
    println!(
        "{}",
        paint(Style::Dim, format!("transcript {}", saved.display()))
    );

    if let Some(path) = save {
        store::save_to_file(&path, &transcript).await?;
        println!(
            "{}",
            paint(Style::Dim, format!("transcript {}", path.display()))
        );
    }

    server.shutdown().await
}

/// Print the structured findings that follow a turn's prose.
///
/// The prose itself has already been streamed token by token, so only the
/// extracted analysis is printed here.
fn print_turn_analysis(turn: &coin::debate::state::Turn) {
    // Close the colour opened when the turn started.
    println!("{}", term::reset());

    if let Some(credence) = turn.analysis.credence {
        println!(
            "  {} {}",
            paint(Style::Dim, "confidence:"),
            paint(Style::Value, credence.to_string())
        );
    }
    if let Some(reason) = &turn.analysis.moved_because {
        println!("  {} {reason}", paint(Style::Dim, "moved because:"));
    }
    for conceded in &turn.analysis.conceded {
        println!("  {} {conceded}", paint(Style::Dim, "conceded:"));
    }
    if !turn.analysis.parse_status.is_ok() {
        println!(
            "  {}",
            paint(Style::Dim, "(no readable structured block in this turn)")
        );
    }
    let _ = std::io::stdout().flush();
}

/// Print the closing summary, including the convergence series.
fn print_summary(state: &DebateState) {
    println!("\n{}", paint(Style::Heading, "Result"));

    let series_a = state.credence_series(Side::A);
    let series_b = state.credence_series(Side::B);

    if !series_a.is_empty() || !series_b.is_empty() {
        // Each side reports confidence in its own position, so the gap column
        // restates them on a single proposition. Without it, two sides that
        // fully agree look maximally far apart.
        println!(
            "{}",
            paint(
                Style::Dim,
                "confidence in own position, and how far apart that leaves them:"
            )
        );
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
                "  round {}   {} {}   {} {}   {} {}",
                round + 1,
                paint(Style::SideA, "A"),
                format_value(&series_a),
                paint(Style::SideB, "B"),
                format_value(&series_b),
                paint(Style::Dim, "gap"),
                paint(Style::Value, gap),
            );
        }
    }

    println!("ended: {}", store::describe_stop(&state.stop_reason));

    let tokens = state.total_tokens();
    println!(
        "{}",
        paint(
            Style::Dim,
            format!(
                "{} turns | {} in, {} out | ${:.4}",
                state.turns.len(),
                tokens.input,
                tokens.output,
                state.total_cost()
            )
        )
    );
}

/// Parse a `provider/model` argument into a reference.
fn parse_model(value: &str) -> std::result::Result<ModelRef, String> {
    value.parse()
}

/// Render a tool invocation as one line, if the part is a tool call.
///
/// Formatting is separated from the destination because the two commands want
/// different streams: `probe` keeps tool noise on stderr so stdout carries only
/// the model's text, while a debate treats tool use as part of the transcript.
fn tool_line(part: &Part) -> Option<String> {
    let Part::Tool { tool, state, .. } = part else {
        return None;
    };

    let detail = state.summary();
    Some(if detail.is_empty() {
        format!("  [{} {}]", state.status, tool)
    } else {
        format!("  [{} {}: {}]", state.status, tool, detail)
    })
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
                if let Some(line) = tool_line(&update.part.part) {
                    eprintln!("\n{line}");
                }
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
