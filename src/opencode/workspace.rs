//! Preparation of the directory an opencode server treats as its project root.
//!
//! Debates run against a disposable scratch directory rather than the user's
//! own projects, so a debater's tool use lands somewhere throwaway.
//!
//! That directory must be a **git repository**. opencode resolves its model
//! catalog per project, and a directory outside a repository yields an empty
//! catalog: `GET /api/model` returns zero entries, so no model can be selected
//! by name and the UI's model picker would be blank. Prompting still succeeds
//! against the server's default model, which makes the failure quiet and
//! confusing. Initializing a repository is what avoids it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;

use crate::error::{CoinError, Result};

/// Whether the given directory already contains a git repository.
fn is_git_repository(directory: &Path) -> bool {
    directory.join(".git").exists()
}

/// Create the directory if needed and make it usable as an opencode project.
///
/// # Arguments
///
/// * `directory` - Directory to prepare as a workspace
///
/// # Returns
///
/// The prepared directory.
///
/// # Errors
///
/// Returns [`CoinError::Io`] if the directory cannot be created, and
/// [`CoinError::OpencodeLaunch`] if `git` cannot be executed.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> coin::error::Result<()> {
/// let workspace = coin::opencode::workspace::prepare("/tmp/coin-debate-1").await?;
/// assert!(workspace.join(".git").exists());
/// # Ok(())
/// # }
/// ```
pub async fn prepare<P>(directory: P) -> Result<PathBuf>
where
    P: AsRef<Path>,
{
    let directory = directory.as_ref();

    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|source| CoinError::io(directory, source))?;

    if is_git_repository(directory) {
        return Ok(directory.to_path_buf());
    }

    debug!(path = %directory.display(), "initializing workspace git repository");

    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(CoinError::OpencodeLaunch)?;

    if !status.success() {
        return Err(CoinError::OpencodeExited {
            status: format!("git init failed with {status}"),
        });
    }

    Ok(directory.to_path_buf())
}

/// Write an agent definition into a prepared workspace.
///
/// Agents are how coin controls a debater's persona. Verified against opencode
/// 1.18.20: a file at `.opencode/agent/<name>.md` is registered under that name
/// and its body **replaces** the built-in system prompt, rather than being
/// appended to it. Without this, debaters would inherit opencode's
/// coding-assistant prompt and behave accordingly.
///
/// Agent files are read when the server starts, so every agent must be written
/// before [`crate::opencode::process::OpencodeServer::launch`].
///
/// # Arguments
///
/// * `directory` - A workspace already prepared by [`prepare`]
/// * `name` - Agent name, which is also the file stem
/// * `description` - Short description shown in agent listings
/// * `prompt` - The system prompt the agent answers with
///
/// # Errors
///
/// Returns [`CoinError::Io`] if the file cannot be written.
pub async fn write_agent<P, N, D, S>(directory: P, name: N, description: D, prompt: S) -> Result<()>
where
    P: AsRef<Path>,
    N: AsRef<str>,
    D: AsRef<str>,
    S: AsRef<str>,
{
    let agent_dir = directory.as_ref().join(".opencode").join("agent");
    tokio::fs::create_dir_all(&agent_dir)
        .await
        .map_err(|source| CoinError::io(&agent_dir, source))?;

    let path = agent_dir.join(format!("{}.md", name.as_ref()));
    let contents = format!(
        "---\ndescription: {description}\nmode: primary\n---\n{prompt}\n",
        description = description.as_ref(),
        prompt = prompt.as_ref(),
    );

    tokio::fs::write(&path, contents)
        .await
        .map_err(|source| CoinError::io(&path, source))?;

    debug!(agent = name.as_ref(), "wrote agent definition");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a unique scratch path without creating it.
    fn scratch_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "coin-workspace-test-{label}-{}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn prepare_creates_a_git_repository() {
        // Arrange
        let path = scratch_path("create");
        let _ = tokio::fs::remove_dir_all(&path).await;

        // Act
        let prepared = prepare(&path).await.expect("workspace must be preparable");

        // Assert: without this, opencode reports an empty model catalog.
        assert!(is_git_repository(&prepared), "workspace must be a git repo");

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&path).await;
    }

    #[tokio::test]
    async fn prepare_is_idempotent() {
        // Arrange
        let path = scratch_path("idempotent");
        let _ = tokio::fs::remove_dir_all(&path).await;
        prepare(&path).await.expect("first prepare must succeed");

        // Act: a second call must not fail or reinitialize.
        let prepared = prepare(&path).await.expect("second prepare must succeed");

        // Assert
        assert!(is_git_repository(&prepared));

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&path).await;
    }

    #[test]
    fn a_plain_directory_is_not_a_repository() {
        // Arrange and act
        let temp = std::env::temp_dir();

        // Assert: guards the detection helper against always returning true.
        assert!(!is_git_repository(&temp.join("coin-definitely-not-a-repo")));
    }
}
