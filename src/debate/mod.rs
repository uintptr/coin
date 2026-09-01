//! The debate engine.
//!
//! A debate is two opencode sessions arguing a proposition under a format that
//! decides how turns are structured and when to stop. The goal is truth-seeking
//! rather than persuasion: both sides are told that conceding a point they
//! cannot support is a success, and formats are chosen to make convergence, or
//! the precise point of irreducible disagreement, visible.
//!
//! See `PROJECT_SPECS.md` sections 4 and 6 for the design.

pub mod credence;
pub mod engine;
pub mod format;
pub mod parse;
pub mod state;

use crate::debate::format::{DebateFormat, FormatId};

/// Build the format implementation for an identifier.
///
/// # Arguments
///
/// * `id` - Which format to construct
///
/// # Returns
///
/// The format, or `None` if it is specified but not yet implemented.
pub fn format_for(id: FormatId) -> Option<Box<dyn DebateFormat>> {
    match id {
        FormatId::Credence => Some(Box::new(credence::CredenceFormat)),
        // Crux-finding, classic rounds, and the claim ledger are step 7.
        FormatId::Crux | FormatId::Classic | FormatId::Ledger => None,
    }
}
