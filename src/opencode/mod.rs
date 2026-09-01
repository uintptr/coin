//! Integration with the opencode server.
//!
//! Coin drives an `opencode serve` child process over HTTP rather than calling
//! model providers directly. That inherits credential management, model
//! routing, tool execution, skill loading, and subagent spawning, and means
//! coin never handles a provider API key.
//!
//! All knowledge of opencode's wire format is confined to this module. The
//! debate engine sees only the [`client::OpencodeClient`] trait and coin's own
//! domain types, so an upstream schema change is absorbed here.

pub mod client;
pub mod events;
pub mod process;
pub mod types;
pub mod workspace;
