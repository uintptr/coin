//! Error types shared across the application.
//!
//! [`CoinError`] is the single error type returned by fallible operations in
//! this crate. Variants carry enough context to be actionable without the
//! caller needing to inspect a source chain.
//!
//! Rust note for Python developers: `thiserror` generates the `Display` and
//! `std::error::Error` implementations from the `#[error(...)]` attributes, so
//! this enum behaves like an exception hierarchy without any runtime cost. The
//! `#[from]` attribute generates a `From` conversion, which is what lets the
//! `?` operator turn a `reqwest::Error` into a `CoinError` automatically.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by coin.
#[derive(Debug, Error)]
pub enum CoinError {
    /// The `opencode` executable could not be found or failed to launch.
    #[error("failed to launch opencode: {0}. Is opencode installed and on PATH?")]
    OpencodeLaunch(#[source] std::io::Error),

    /// The opencode server exited before it became usable.
    #[error("opencode server exited during startup with status {status}")]
    OpencodeExited {
        /// Exit status reported by the child process.
        status: String,
    },

    /// The server did not print a listening address within the timeout.
    #[error("timed out after {seconds}s waiting for opencode to report its port")]
    PortDiscoveryTimeout {
        /// How long the caller waited.
        seconds: u64,
    },

    /// The server printed a port but never became healthy.
    ///
    /// opencode briefly serves its web UI from API routes immediately after
    /// startup, so a health poll is mandatory before the first real request.
    #[error("timed out after {seconds}s waiting for opencode to become healthy at {url}")]
    HealthTimeout {
        /// Base URL that was polled.
        url: String,
        /// How long the caller waited.
        seconds: u64,
    },

    /// An HTTP request to the opencode server failed at the transport level.
    #[error("opencode request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The opencode server returned a non-success status.
    #[error("opencode returned {status} for {method} {path}: {body}")]
    OpencodeStatus {
        /// HTTP method of the failing request.
        method: String,
        /// Path of the failing request.
        path: String,
        /// Status code returned.
        status: u16,
        /// Response body, truncated for readability.
        body: String,
    },

    /// A response body could not be deserialized into the expected shape.
    #[error("could not decode opencode response for {context}: {source}")]
    Decode {
        /// What was being decoded, for example `"session.create"`.
        context: String,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// The event stream ended or failed before the operation completed.
    #[error("opencode event stream failed: {0}")]
    EventStream(String),

    /// A model reported an error while producing a turn.
    #[error("session {session_id} reported an error: {message}")]
    Session {
        /// Session that failed.
        session_id: String,
        /// Message reported by opencode.
        message: String,
    },

    /// A filesystem operation failed.
    #[error("filesystem error at {path}: {source}")]
    Io {
        /// Path involved in the failure.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A command line argument or configuration value was not usable.
    ///
    /// Carries a complete, user-facing sentence: these are reported straight to
    /// the operator, who does not need a prefix naming an internal subsystem.
    #[error("{0}")]
    Invalid(String),

    /// A required directory could not be determined from the environment.
    #[error("could not determine the {0} directory for this platform")]
    MissingDirectory(&'static str),
}

/// Convenience alias for results produced by this crate.
pub type Result<T> = std::result::Result<T, CoinError>;

impl CoinError {
    /// Build an [`CoinError::Io`] from a path and the underlying error.
    ///
    /// # Arguments
    ///
    /// * `path` - Path the failed operation was acting on
    /// * `source` - IO error reported by the standard library
    ///
    /// # Returns
    ///
    /// An [`CoinError::Io`] carrying both, so the message names the file.
    pub fn io<P>(path: P, source: std::io::Error) -> Self
    where
        P: Into<PathBuf>,
    {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether retrying the failed operation could plausibly succeed.
    ///
    /// Transport failures and the server's own overload responses are worth
    /// another attempt. Everything else fails identically however many times
    /// it is asked: a rejected payload stays rejected, a missing directory
    /// stays missing, and retrying only delays the report.
    ///
    /// # Returns
    ///
    /// `true` if the operation is worth another attempt.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(source) => source.is_timeout() || source.is_connect() || source.is_request(),
            Self::OpencodeStatus { status, .. } => matches!(status, 408 | 429 | 500..=599),
            Self::OpencodeLaunch(_)
            | Self::OpencodeExited { .. }
            | Self::PortDiscoveryTimeout { .. }
            | Self::HealthTimeout { .. }
            | Self::Decode { .. }
            | Self::EventStream(_)
            | Self::Session { .. }
            | Self::Io { .. }
            | Self::Invalid(_)
            | Self::MissingDirectory(_) => false,
        }
    }

    /// Build a [`CoinError::Decode`] from a context label and a serde error.
    ///
    /// # Arguments
    ///
    /// * `context` - Short label naming what was being decoded
    /// * `source` - The serde error that occurred
    ///
    /// # Returns
    ///
    /// A [`CoinError::Decode`] naming the operation that failed.
    pub fn decode<S>(context: S, source: serde_json::Error) -> Self
    where
        S: Into<String>,
    {
        Self::Decode {
            context: context.into(),
            source,
        }
    }
}
