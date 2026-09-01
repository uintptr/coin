//! Supervision of the `opencode serve` child process.
//!
//! Coin never handles a provider API key. It launches opencode, which already
//! holds credentials in `~/.local/share/opencode/auth.json`, and drives it over
//! HTTP. This module owns that child's lifetime.
//!
//! Two startup details are load-bearing and were established by probing a live
//! server rather than from documentation:
//!
//! 1. The port must be read from stdout. We launch with `--port 0` so the OS
//!    assigns a free port, and the server prints the address it bound.
//! 2. A health poll is **mandatory** before the first API request. Immediately
//!    after startup opencode briefly serves its web UI's HTML from API routes,
//!    so an early request returns a page instead of JSON.

use std::process::Stdio;
use std::time::Instant;

use rand::RngExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use crate::config::{HEALTH_POLL_INTERVAL, HEALTH_TIMEOUT, OpencodeConfig, PORT_DISCOVERY_TIMEOUT};
use crate::error::{CoinError, Result};
use crate::opencode::types::Health;

/// Number of random bytes used for the generated server password.
const PASSWORD_BYTES: usize = 24;

/// Username coin authenticates as. opencode defaults to this name.
const USERNAME: &str = "opencode";

/// Parse the bound address out of a server startup line.
///
/// opencode prints `opencode server listening on http://127.0.0.1:PORT`.
fn parse_listening_url(line: &str) -> Option<String> {
    let start = line.find("http://")?;
    Some(line[start..].trim().to_string())
}

/// Generate a random password for the server's basic auth.
///
/// opencode warns when `OPENCODE_SERVER_PASSWORD` is unset. We bind to loopback
/// regardless, but an unauthenticated local HTTP server is still worth avoiding
/// on a shared machine.
fn generate_password() -> String {
    let mut rng = rand::rng();
    (0..PASSWORD_BYTES)
        .map(|_| char::from(b'a' + rng.random_range(0..26)))
        .collect()
}

/// A running `opencode serve` process.
///
/// The child is killed when this value is dropped, so a panic or an early
/// return cannot leave an orphaned server behind.
#[derive(Debug)]
pub struct OpencodeServer {
    child: Child,
    base_url: String,
    password: String,
}

impl OpencodeServer {
    /// Launch an opencode server and wait until it is ready to serve requests.
    ///
    /// # Arguments
    ///
    /// * `config` - Executable, project directory, and tool settings
    ///
    /// # Returns
    ///
    /// A handle to the running server, already confirmed healthy.
    ///
    /// # Errors
    ///
    /// Returns [`CoinError::OpencodeLaunch`] if the executable cannot be
    /// started, [`CoinError::PortDiscoveryTimeout`] if it never reports an
    /// address, [`CoinError::OpencodeExited`] if it exits during startup, and
    /// [`CoinError::HealthTimeout`] if it never becomes healthy.
    pub async fn launch(config: &OpencodeConfig) -> Result<Self> {
        let password = generate_password();
        let mut command = Command::new(&config.executable);

        command
            .arg("serve")
            .arg("--port")
            .arg("0")
            .arg("--hostname")
            .arg("127.0.0.1")
            .current_dir(&config.directory)
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if config.enable_websearch {
            // Web search is the Exa-backed `websearch` tool and is off unless
            // this flag is set. EXA_API_KEY is optional; without it opencode
            // falls back to a keyless endpoint.
            command.env("OPENCODE_ENABLE_EXA", "1");
        }

        let mut child = command.spawn().map_err(CoinError::OpencodeLaunch)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoinError::EventStream("opencode stdout was not piped".to_string()))?;

        let base_url = match timeout(PORT_DISCOVERY_TIMEOUT, read_listening_url(stdout)).await {
            Ok(Ok(url)) => url,
            Ok(Err(err)) => {
                // The stream ended without an address. Report the exit status
                // if the process has already died, which is the common cause.
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(CoinError::OpencodeExited {
                        status: status.to_string(),
                    });
                }
                return Err(err);
            }
            Err(_elapsed) => {
                return Err(CoinError::PortDiscoveryTimeout {
                    seconds: PORT_DISCOVERY_TIMEOUT.as_secs(),
                });
            }
        };

        debug!(url = %base_url, "opencode reported its listening address");

        let server = Self {
            child,
            base_url,
            password,
        };
        server.wait_until_healthy().await?;
        Ok(server)
    }

    /// Base URL of the running server, for example `http://127.0.0.1:51234`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Username for basic authentication.
    pub fn username(&self) -> &str {
        USERNAME
    }

    /// Generated password for basic authentication.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Poll `GET /api/health` until the server reports healthy.
    ///
    /// This is not optional. opencode serves its web UI from API routes for a
    /// short window after binding, so requests issued before the server is
    /// healthy can return HTML where JSON is expected.
    async fn wait_until_healthy(&self) -> Result<()> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/health", self.base_url);
        let deadline = Instant::now() + HEALTH_TIMEOUT;

        while Instant::now() < deadline {
            let response = client
                .get(&url)
                .basic_auth(USERNAME, Some(&self.password))
                .send()
                .await;

            // A health probe against a server that is still binding will fail
            // at the transport level. That is expected, so retry rather than
            // propagate until the deadline passes.
            if let Ok(response) = response
                && response.status().is_success()
                && let Ok(health) = response.json::<Health>().await
                && health.healthy
            {
                debug!("opencode reported healthy");
                return Ok(());
            }

            sleep(HEALTH_POLL_INTERVAL).await;
        }

        Err(CoinError::HealthTimeout {
            url: self.base_url.clone(),
            seconds: HEALTH_TIMEOUT.as_secs(),
        })
    }

    /// Terminate the server and wait for it to exit.
    ///
    /// Dropping the handle also kills the child. Calling this explicitly lets
    /// the caller await the exit rather than leaving it to the runtime.
    ///
    /// # Errors
    ///
    /// Never returns an error; failures to signal an already-dead process are
    /// logged and ignored.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Err(err) = self.child.kill().await {
            warn!(error = %err, "failed to signal the opencode server during shutdown");
        }
        Ok(())
    }
}

/// Read lines from the child's stdout until one carries a listening address.
async fn read_listening_url(stdout: tokio::process::ChildStdout) -> Result<String> {
    let mut lines = BufReader::new(stdout).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|err| CoinError::EventStream(format!("reading opencode stdout: {err}")))?
    {
        debug!(line = %line, "opencode stdout");
        if let Some(url) = parse_listening_url(&line) {
            return Ok(url);
        }
    }

    Err(CoinError::EventStream(
        "opencode stdout closed before reporting a listening address".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_listening_line_opencode_prints() {
        // Arrange: the exact line observed from opencode 1.18.20.
        let line = "opencode server listening on http://127.0.0.1:4199";

        // Act
        let url = parse_listening_url(line);

        // Assert
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:4199"));
    }

    #[test]
    fn ignores_lines_without_an_address() {
        // Arrange: the unsecured-server warning precedes the listening line.
        let line = "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.";

        // Act and assert
        assert_eq!(parse_listening_url(line), None);
    }

    #[test]
    fn trims_trailing_whitespace_from_the_address() {
        // Arrange
        let line = "opencode server listening on http://127.0.0.1:4199  \r";

        // Act
        let url = parse_listening_url(line);

        // Assert
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:4199"));
    }

    #[test]
    fn generated_passwords_have_the_expected_shape() {
        // Act
        let password = generate_password();

        // Assert
        assert_eq!(password.len(), PASSWORD_BYTES);
        assert!(password.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn generated_passwords_differ_between_calls() {
        // Act and assert: a fixed password would defeat the point.
        assert_ne!(generate_password(), generate_password());
    }
}
