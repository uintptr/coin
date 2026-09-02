//! Application configuration and filesystem locations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{CoinError, Result};
use crate::opencode::types::ModelRef;

/// Name of the configuration file looked for in the working directory.
pub const CONFIG_FILE: &str = "config.toml";

/// How long to wait for opencode to print its listening address.
pub const PORT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for `GET /api/health` to report healthy.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between health poll attempts during startup.
pub const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Attempts made at a single turn before the debate gives up on it.
pub const TURN_ATTEMPTS: usize = 3;

/// Delay before the first retry of a turn. It doubles with each attempt.
///
/// A turn routinely takes minutes, so a few seconds of backoff costs nothing
/// against the chance that a transient provider fault clears.
pub const TURN_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// How a turn that comes back empty is retried.
///
/// A turn can fail without the request failing. opencode answers
/// `POST /session/{id}/message` with 200 and records the provider's rejection
/// on the assistant message, so an exhausted quota or an overloaded provider
/// arrives as a turn with no text rather than as an HTTP error. Retrying
/// covers the transient half of that; the permanent half is reported rather
/// than retried, since waiting to be refused again wastes minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts per turn, including the first. Values below one are
    /// treated as one.
    pub attempts: usize,
    /// Delay before the first retry, doubling with each further attempt.
    pub backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: TURN_ATTEMPTS,
            backoff: TURN_RETRY_BACKOFF,
        }
    }
}

/// Deserialize a `provider/model` string into a [`ModelRef`].
///
/// [`ModelRef`]'s own wire form is an object, because that is what opencode's
/// prompt payload requires. A person writing a configuration file should be
/// able to put `model = "openrouter/z-ai/glm-5.3-flash"` rather than a table,
/// so the string form is parsed here. An unparseable value fails the load with
/// the file, line, and column, which beats discovering it at the first prompt.
fn deserialize_model<'de, D>(deserializer: D) -> std::result::Result<Option<ModelRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(value) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    value.parse().map(Some).map_err(serde::de::Error::custom)
}

/// Settings read from a `config.toml`.
///
/// The file exists so that choosing a model is not a recompile. Everything in
/// it is optional and overridden by the command line; an absent file is the
/// same as an empty one.
///
/// ```toml
/// [debate]
/// model = "openrouter/z-ai/glm-5.3-flash"
/// model_b = "openrouter/openai/gpt-oss-120b"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Defaults for `coin debate`.
    pub debate: DebateSettings,
}

/// Debate defaults from the configuration file.
///
/// Unknown keys are rejected rather than ignored. A misspelled `moddel` that
/// silently does nothing is worse than a startup error naming the line.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebateSettings {
    /// Model both sides argue with, in `provider/model` form.
    #[serde(deserialize_with = "deserialize_model")]
    pub model: Option<ModelRef>,
    /// Model for side B, when the two sides should differ.
    #[serde(deserialize_with = "deserialize_model")]
    pub model_b: Option<ModelRef>,
}

impl Settings {
    /// Load settings from `directory`, or from an explicit file.
    ///
    /// Kept separate from [`Settings::load`] so tests can point it at a
    /// temporary directory instead of changing the process working directory,
    /// which is global and would race across parallel tests.
    fn load_from(directory: &Path, explicit: Option<&Path>) -> Result<Self> {
        // An explicit path must exist; a conventional one need not. Reading
        // first and inspecting the error avoids a check-then-read race.
        let (path, required) = match explicit {
            Some(path) => (path.to_path_buf(), true),
            None => (directory.join(CONFIG_FILE), false),
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(source) => return Err(CoinError::io(path, source)),
        };

        toml::from_str(&text).map_err(|source| {
            CoinError::Invalid(format!("could not read {}: {source}", path.display()))
        })
    }

    /// Load settings for this run.
    ///
    /// Reads the file named by `--config` when one is given, and otherwise
    /// [`CONFIG_FILE`] in the working directory if it is present. A file the
    /// operator named explicitly is never silently skipped; a conventional one
    /// that does not exist is.
    ///
    /// # Arguments
    ///
    /// * `explicit` - Path from `--config`, if given
    ///
    /// # Returns
    ///
    /// The parsed settings, or the defaults when no file applies.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::Io`] if a named file cannot be read, and
    /// [`CoinError::Invalid`] if a file that was read cannot be parsed or
    /// holds a model that is not in `provider/model` form.
    pub fn load<P>(explicit: Option<P>) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Self::load_from(Path::new("."), explicit.as_ref().map(AsRef::as_ref))
    }
}

/// Runtime configuration for the opencode child process.
#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    /// Executable to launch. Defaults to `opencode` resolved via `PATH`.
    pub executable: PathBuf,
    /// Enable the Exa-backed `websearch` tool.
    ///
    /// opencode gates web search behind `OPENCODE_ENABLE_EXA`; it is off by
    /// default. `EXA_API_KEY` is optional, since opencode falls back to a
    /// keyless endpoint when it is unset.
    pub enable_websearch: bool,
    /// Working directory the server treats as the project root.
    pub directory: PathBuf,
}

impl OpencodeConfig {
    /// Build a configuration rooted at the given project directory.
    ///
    /// # Arguments
    ///
    /// * `directory` - Directory the opencode server treats as the project root
    ///
    /// # Returns
    ///
    /// A configuration launching `opencode` from `PATH` with web search on.
    pub fn new<P>(directory: P) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            executable: PathBuf::from("opencode"),
            enable_websearch: true,
            directory: directory.as_ref().to_path_buf(),
        }
    }
}

/// Resolve the data directory used for debate workspaces and transcripts.
///
/// # Returns
///
/// `$XDG_DATA_HOME/coin` on platforms that define it, or the platform data
/// directory otherwise.
///
/// # Errors
///
/// Returns [`CoinError::MissingDirectory`] if no data directory can be
/// determined for this platform.
pub fn data_dir() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|base| base.join("coin"))
        .ok_or(CoinError::MissingDirectory("data"))
}

/// Directory holding the per-debate workspace for the given debate id.
///
/// # Arguments
///
/// * `debate_id` - Identifier of the debate
///
/// # Returns
///
/// The path to that debate's workspace, which may not exist yet.
///
/// # Errors
///
/// Propagates failures from [`data_dir`].
pub fn debate_dir<S>(debate_id: S) -> Result<PathBuf>
where
    S: AsRef<str>,
{
    Ok(data_dir()?.join("debates").join(debate_id.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private directory for one test, removed by the caller.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("coin-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory must be creatable");
        directory
    }

    fn write(directory: &Path, name: &str, contents: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, contents).expect("fixture must be writable");
        path
    }

    #[test]
    fn a_config_file_in_the_directory_supplies_both_models() {
        // Arrange
        let directory = scratch("both");
        write(
            &directory,
            CONFIG_FILE,
            "[debate]\n\
             model = \"openrouter/z-ai/glm-5.3-flash\"\n\
             model_b = \"openrouter/openai/gpt-oss-120b\"\n",
        );

        // Act
        let settings = Settings::load_from(&directory, None).expect("the file must parse");

        // Assert: the string form is what a person writes, not the object form
        // the prompt payload needs.
        assert_eq!(
            settings.debate.model.map(|model| model.to_string()),
            Some("openrouter/z-ai/glm-5.3-flash".to_string())
        );
        assert_eq!(
            settings.debate.model_b.map(|model| model.model_id),
            Some("openai/gpt-oss-120b".to_string())
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn the_committed_sample_parses() {
        // Arrange: samples/config.toml is what a person copies to start from,
        // so an example that no longer parses is worse than no example.
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("samples");

        // Act
        let settings =
            Settings::load_from(&directory, None).expect("samples/config.toml must parse");

        // Assert: model_b is commented out there on purpose, since the same
        // model on both sides is the default.
        assert_eq!(
            settings.debate.model.map(|model| model.to_string()),
            Some("openrouter/z-ai/glm-5.3-flash".to_string())
        );
        assert!(settings.debate.model_b.is_none());
    }

    #[test]
    fn an_absent_config_file_is_not_an_error() {
        // Arrange: an empty directory, which is the ordinary case.
        let directory = scratch("absent");

        // Act
        let settings = Settings::load_from(&directory, None).expect("an absent file must be fine");

        // Assert
        assert!(settings.debate.model.is_none());

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_file_named_explicitly_must_exist() {
        // Arrange: the same absence, but this time the operator asked for it
        // by name, so silently ignoring it would hide a typo.
        let directory = scratch("explicit");
        let missing = directory.join("nowhere.toml");

        // Act and assert
        assert!(Settings::load_from(&directory, Some(&missing)).is_err());

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_partial_file_leaves_the_rest_defaulted() {
        // Arrange: only one side pinned.
        let directory = scratch("partial");
        let path = write(
            &directory,
            "only-a.toml",
            "[debate]\nmodel = \"openrouter/z-ai/glm-5.3-flash\"\n",
        );

        // Act
        let settings = Settings::load_from(&directory, Some(&path)).expect("the file must parse");

        // Assert
        assert!(settings.debate.model.is_some());
        assert!(settings.debate.model_b.is_none());

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_model_that_is_not_provider_slash_model_fails_the_load() {
        // Arrange: a bare model id, which opencode cannot route.
        let directory = scratch("bad-model");
        let path = write(
            &directory,
            "bad.toml",
            "[debate]\nmodel = \"glm-5.3-flash\"\n",
        );

        // Act
        let error = Settings::load_from(&directory, Some(&path))
            .expect_err("an unroutable model must fail the load");

        // Assert: the message must name the file and the expected form, since
        // this is read at startup and never seen again.
        let text = error.to_string();
        assert!(text.contains("bad.toml"), "message was: {text}");
        assert!(text.contains("provider/model"), "message was: {text}");

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_misspelled_key_is_rejected_rather_than_ignored() {
        // Arrange: a key that does nothing is worse than one that fails.
        let directory = scratch("typo");
        let path = write(&directory, "typo.toml", "[debate]\nmoddel = \"a/b\"\n");

        // Act and assert
        let error = Settings::load_from(&directory, Some(&path))
            .expect_err("an unknown key must fail the load");
        assert!(error.to_string().contains("moddel"), "message was: {error}");

        std::fs::remove_dir_all(&directory).ok();
    }
}
