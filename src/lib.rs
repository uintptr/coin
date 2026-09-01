//! Coin runs a structured debate between two LLM debaters over a proposition,
//! streaming it to a local web UI so the reasoning can be watched and steered.
//!
//! The design goal is truth-seeking rather than persuasion. See
//! `PROJECT_SPECS.md` for the full specification.
//!
//! This crate is split so the binary stays thin:
//!
//! - [`config`] resolves runtime settings and filesystem locations
//! - [`error`] defines the single error type used throughout
//! - [`opencode`] owns all knowledge of the opencode server
//! - [`debate`] holds the format-agnostic engine and the formats themselves

pub mod config;
pub mod debate;
pub mod error;
pub mod opencode;
