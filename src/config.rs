//! Application configuration and filesystem locations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CoinError, Result};

/// How long to wait for opencode to print its listening address.
pub const PORT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for `GET /api/health` to report healthy.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between health poll attempts during startup.
pub const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
