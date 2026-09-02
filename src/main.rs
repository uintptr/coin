//! Command line entry point for coin.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::time::timeout;
use tracing::error;
use tracing_subscriber::EnvFilter;

use coin::config::{DebateSettings, OpencodeConfig, RetryPolicy, Settings, data_dir, debate_dir};
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
use coin::store::{self, Transcript};
use coin::term::{self, Style, paint};

/// How long to wait for the event stream to observe completion after the
/// prompt returns. The idle event normally arrives first, so this is a
/// backstop rather than an expected wait.
const EVENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Model debaters use unless `config.toml` or the command line says otherwise.
///
/// Chosen by benchmarking candidates on a real debate. It costs roughly a
/// fifteenth of the server default (kimi-k3) while still stating
/// well-calibrated confidences, which is what the credence format depends on:
/// cheaper models were faster still but reported low confidence in positions
/// the evidence plainly supported, which corrupts the convergence reading.
///
/// Routed through OpenRouter rather than DigitalOcean. Same weights, so the
/// benchmark still holds, at half the price and without the daily token quota
/// that silently emptied a real debate's later rounds.
const DEFAULT_DEBATE_MODEL: &str = "openrouter/z-ai/glm-5.3-flash";

/// Structured debate between two LLM debaters, streamed to a web UI.
#[derive(Debug, Parser)]
#[command(name = "coin", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Read defaults from this file instead of `config.toml` in the working
    /// directory. A file named here must exist.
    #[arg(long, value_name = "FILE", global = true)]
    config: Option<PathBuf>,
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
        /// The question under dispute, or a file containing it.
        #[arg(short, long)]
        question: String,

        /// The case assigned to side A, or a file containing it.
        #[arg(short = 'a', long)]
        position_a: String,

        /// The case assigned to side B, or a file containing it.
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

/// File extensions treated as naming a text file.
const TEXT_EXTENSIONS: [&str; 3] = ["txt", "md", "text"];

/// Whether a value that does not exist on disk was probably meant as a path.
///
/// A debate position is prose and almost always contains spaces, so a
/// whitespace-free value that carries a separator or a text extension is far
/// more likely to be a mistyped filename than an argument. Reporting that as a
/// missing file beats silently debating the literal string `notes/postion.md`.
///
/// Whitespace is checked first so a genuine question containing a slash, such
/// as "Is TCP/IP better than X?", stays a literal string.
fn looks_like_path(value: &str) -> bool {
    if value.contains(char::is_whitespace) || value.is_empty() {
        return false;
    }

    value.contains('/')
        || value.starts_with('~')
        || Path::new(value)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                TEXT_EXTENSIONS
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            })
}

/// Resolve an argument that may name a file or may be the text itself.
///
/// Long positions are easier to keep in a file than to paste into a shell, so
/// each of the topic arguments accepts either. An existing file is read; any
/// other value is used verbatim.
///
/// # Arguments
///
/// * `label` - Name of the argument, used in error messages
/// * `value` - The raw argument
///
/// # Returns
///
/// The file's contents, trimmed, or the original value.
///
/// # Errors
///
/// Returns [`CoinError::Io`] if an existing file cannot be read, and
/// [`CoinError::EventStream`] if the file is empty or if the value looks like a
/// path that does not exist.
async fn text_or_file(label: &str, value: String) -> Result<String> {
    let path = Path::new(&value);

    if path.is_file() {
        let contents = tokio::fs::read_to_string(path)
            .await
            .map_err(|source| CoinError::io(path, source))?;

        if contents.trim().is_empty() {
            return Err(CoinError::Invalid(format!(
                "{label} file {} is empty",
                path.display()
            )));
        }
        return Ok(contents.trim().to_string());
    }

    // A missing file is only an error when the value was clearly meant as one.
    if looks_like_path(&value) {
        return Err(CoinError::Invalid(format!(
            "{label} looks like a file path but {value} does not exist; \
             pass the text directly if that was intended"
        )));
    }

    Ok(value)
}

/// Settle which model each side argues with.
///
/// Three sources, in falling order of precedence: the command line, the
/// configuration file, and the built-in default. `-m` is documented as setting
/// **both** sides, so it also overrides a `model_b` pinned in a file;
/// `--model-b` is how to ask for two different models on one run.
///
/// # Arguments
///
/// * `flag_a` - `-m`, the model for both sides
/// * `flag_b` - `--model-b`, side B only
/// * `settings` - Debate defaults from the configuration file
///
/// # Returns
///
/// The models for side A and side B, in that order.
///
/// # Errors
///
/// Returns [`CoinError::Invalid`] if the built-in default is not in
/// `provider/model` form, which would be a bug in this file.
fn resolve_models(
    flag_a: Option<ModelRef>,
    flag_b: Option<ModelRef>,
    settings: DebateSettings,
) -> Result<(ModelRef, ModelRef)> {
    let built_in = DEFAULT_DEBATE_MODEL.parse().map_err(CoinError::Invalid)?;
    let model_a = flag_a.clone().or(settings.model).unwrap_or(built_in);
    let model_b = flag_b
        .or(flag_a)
        .or(settings.model_b)
        .unwrap_or_else(|| model_a.clone());

    Ok((model_a, model_b))
}

/// Dispatch the parsed command.
async fn run(cli: Cli) -> Result<()> {
    let settings = Settings::load(cli.config.as_deref())?;

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
            let (model_a, model_b) = resolve_models(model, model_b, settings.debate)?;
            let config = DebateConfig {
                topic: Topic::new(
                    text_or_file("question", question).await?,
                    text_or_file("position A", position_a).await?,
                    text_or_file("position B", position_b).await?,
                ),
                max_rounds,
                model_a: Some(model_a),
                model_b: Some(model_b),
                retry: RetryPolicy::default(),
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
        return Err(CoinError::Invalid(format!(
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
            // A turn that failed never reached its completion, so the colour
            // opened when it started is still open. Closing it here keeps a
            // failed debate from tinting everything printed after it.
            Progress::Finished { .. } => print!("{}", term::reset()),
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

    server.shutdown().await?;

    // A debate that ended because a side could not answer is a failure rather
    // than a result, so anything scripting coin sees a non-zero exit. The
    // transcript above is saved first: the turns that did happen still cost
    // money and are still worth reading.
    match state.stop_reason {
        Some(StopReason::Failed { side, message }) => Err(CoinError::Session {
            session_id: engine.session_id(side).to_string(),
            message,
        }),
        Some(_) | None => Ok(()),
    }
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

    let rows = state.credence_rounds();

    if !rows.is_empty() {
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
        for row in &rows {
            let cell = |value: Option<Credence>| {
                value.map_or_else(|| "  -".to_string(), |credence| format!("{credence:>3}"))
            };
            let gap = row
                .gap()
                .map_or_else(|| "  -".to_string(), |gap| format!("{gap:>3}"));
            println!(
                "  round {}   {} {}   {} {}   {} {}",
                row.round,
                paint(Style::SideA, "A"),
                cell(row.a),
                paint(Style::SideB, "B"),
                cell(row.b),
                paint(Style::Dim, "gap"),
                paint(Style::Value, gap),
            );
        }
    }

    // A debate can complete with most of its structure unreadable, which
    // silently weakens every number above. Say so rather than letting a
    // sparse table look like a short debate.
    let unreadable = state.unreadable_turns();
    if unreadable > 0 {
        println!(
            "{}",
            paint(
                Style::Value,
                format!(
                    "caution: {unreadable} of {} turns produced no readable \
                     structured block, so the table omits them",
                    state.turns.len()
                )
            )
        );
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
        return Err(CoinError::Invalid(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `provider/model` string in a test.
    fn model(value: &str) -> ModelRef {
        value.parse().expect("test model must parse")
    }

    /// Settings pinning the given models.
    fn settings(side_a: Option<&str>, side_b: Option<&str>) -> DebateSettings {
        DebateSettings {
            model: side_a.map(model),
            model_b: side_b.map(model),
        }
    }

    #[test]
    fn with_no_flags_and_no_file_both_sides_get_the_built_in_default() {
        // Arrange and act
        let (a, b) = resolve_models(None, None, settings(None, None)).expect("must resolve");

        // Assert: the same model on both sides isolates the argument from model
        // capability, which is why it is the default.
        assert_eq!(a.to_string(), DEFAULT_DEBATE_MODEL);
        assert_eq!(b, a);
    }

    #[test]
    fn the_file_supplies_both_sides_when_the_command_line_is_silent() {
        // Arrange
        let file = settings(Some("openrouter/z-ai/glm-5.3-flash"), Some("a/b"));

        // Act
        let (a, b) = resolve_models(None, None, file).expect("must resolve");

        // Assert
        assert_eq!(a.to_string(), "openrouter/z-ai/glm-5.3-flash");
        assert_eq!(b.to_string(), "a/b");
    }

    #[test]
    fn a_file_pinning_only_one_model_still_gives_both_sides_a_model() {
        // Arrange
        let file = settings(Some("provider/only"), None);

        // Act
        let (a, b) = resolve_models(None, None, file).expect("must resolve");

        // Assert
        assert_eq!(a.to_string(), "provider/only");
        assert_eq!(b, a);
    }

    #[test]
    fn the_command_line_beats_the_file_on_both_sides() {
        // Arrange: -m is documented as setting both sides, so it must override
        // a model_b the file pinned rather than producing a surprise pairing.
        let file = settings(Some("file/a"), Some("file/b"));

        // Act
        let (a, b) = resolve_models(Some(model("flag/x")), None, file).expect("must resolve");

        // Assert
        assert_eq!(a.to_string(), "flag/x");
        assert_eq!(b.to_string(), "flag/x");
    }

    #[test]
    fn model_b_on_the_command_line_differs_the_sides() {
        // Arrange
        let file = settings(Some("file/a"), Some("file/b"));

        // Act
        let (a, b) = resolve_models(None, Some(model("flag/y")), file).expect("must resolve");

        // Assert: A still comes from the file, B from the flag.
        assert_eq!(a.to_string(), "file/a");
        assert_eq!(b.to_string(), "flag/y");
    }

    #[test]
    fn both_flags_together_win_outright() {
        // Arrange
        let file = settings(Some("file/a"), Some("file/b"));

        // Act
        let (a, b) = resolve_models(Some(model("flag/x")), Some(model("flag/y")), file)
            .expect("must resolve");

        // Assert
        assert_eq!(a.to_string(), "flag/x");
        assert_eq!(b.to_string(), "flag/y");
    }

    #[test]
    fn the_built_in_default_is_routable() {
        // Arrange and act: a typo here would only surface at the first debate.
        let parsed = model(DEFAULT_DEBATE_MODEL);

        // Assert: the model id keeps its own slash, so only the first one
        // separates the provider.
        assert_eq!(parsed.provider_id, "openrouter");
        assert_eq!(parsed.model_id, "z-ai/glm-5.3-flash");
    }

    /// Write a scratch file and return its path.
    async fn scratch_file(label: &str, contents: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("coin-arg-test-{label}-{}.md", std::process::id()));
        tokio::fs::write(&path, contents)
            .await
            .expect("scratch file must be writable");
        path
    }

    #[tokio::test]
    async fn an_existing_file_is_read() {
        // Arrange
        let path = scratch_file("read", "  The case for X.\n").await;

        // Act
        let resolved = text_or_file("position A", path.display().to_string())
            .await
            .expect("an existing file must be read");

        // Assert: contents are trimmed, since a trailing newline is an
        // artefact of the file rather than part of the argument.
        assert_eq!(resolved, "The case for X.");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn ordinary_prose_is_used_verbatim() {
        // Act
        let resolved = text_or_file("question", "Is X true?".to_string())
            .await
            .expect("prose must pass through");

        // Assert
        assert_eq!(resolved, "Is X true?");
    }

    #[tokio::test]
    async fn a_question_containing_a_slash_stays_a_string() {
        // Arrange: this is the case that makes a naive path check wrong.
        let question = "Is TCP/IP better than the OSI model?".to_string();

        // Act
        let resolved = text_or_file("question", question.clone())
            .await
            .expect("a question with a slash must not be treated as a path");

        // Assert
        assert_eq!(resolved, question);
    }

    #[tokio::test]
    async fn a_mistyped_path_is_reported_rather_than_debated() {
        // Arrange: the footgun of the file-or-string rule is that a typo
        // silently becomes the argument.
        let result = text_or_file("position A", "notes/postion.md".to_string()).await;

        // Assert
        let error = result.expect_err("a missing path must be reported");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_file_is_rejected() {
        // Arrange: an empty position would produce a meaningless debate.
        let path = scratch_file("empty", "   \n").await;

        // Act
        let result = text_or_file("question", path.display().to_string()).await;

        // Assert
        let error = result.expect_err("an empty file must be rejected");
        assert!(error.to_string().contains("empty"), "{error}");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[test]
    fn path_like_values_are_recognised() {
        assert!(looks_like_path("notes/question.md"));
        assert!(looks_like_path("question.txt"));
        assert!(looks_like_path("~/debates/a.md"));
        assert!(looks_like_path("./a.text"));
    }

    #[test]
    fn prose_is_not_mistaken_for_a_path() {
        // Anything with whitespace is prose, whatever else it contains.
        assert!(!looks_like_path("Is X true?"));
        assert!(!looks_like_path("Is TCP/IP better?"));
        assert!(!looks_like_path("see notes.md for detail"));
        assert!(!looks_like_path(""));
    }

    #[test]
    fn a_bare_word_is_not_a_path() {
        // A single word with no separator or text extension is more likely a
        // terse position than a filename.
        assert!(!looks_like_path("yes"));
        assert!(!looks_like_path("X"));
    }
}
