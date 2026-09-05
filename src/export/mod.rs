//! Loading ChatGPT exports from untrusted `.json` and `.zip` inputs.
//!
//! Everything below this module treats its input as hostile: an export is a
//! file the user downloaded from the internet, and nothing in it — sizes,
//! entry names, JSON shapes, text content — may be trusted. The rules live
//! next to the code that applies them:
//!
//! - [`json`] tolerates per-conversation damage instead of failing the load.
//! - [`zip`] validates entry names and enforces an unpacked-size ceiling.
//! - [`loader`] decides the format from magic bytes rather than the extension.
pub mod json;
pub mod loader;
pub mod zip;

use crate::error::{Error, Result};
use crate::model::ConversationSet;

/// Build the "this input is over the safety limit" error.
///
/// Every size refusal in this module funnels through here, for two reasons.
/// First, the caller supplies a `label` that says *which* of the two checks
/// fired — the input's own declared size, or the bytes it actually delivered
/// after lying about that size — because "we refused it" without "and here is
/// why" is useless to someone debugging a rejected export. Second, this reuses
/// [`Error::ArchiveEntryTooLarge`] for loose `.json` files too, which is a
/// deliberate compromise: `error.rs` is not ours to edit, and a matchable error
/// with one inaccurate word ("archive") beats an unmatchable one. Swapping in a
/// dedicated variant is a change to this function alone.
pub(crate) fn size_refusal(label: String, bytes: u64, limit: u64) -> Error {
    Error::ArchiveEntryTooLarge {
        entry: label,
        declared: bytes,
        limit,
    }
}

/// Safety limits applied while reading an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadOptions {
    /// Ceiling on the uncompressed size of any single archive entry, in bytes.
    /// Both the size the archive *declares* and the bytes it actually delivers
    /// are checked against it.
    pub max_unpacked_bytes: u64,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_unpacked_bytes: zip::DEFAULT_MAX_UNPACKED_BYTES,
        }
    }
}

/// A place conversations can be read from. Implementations must treat their
/// input as hostile.
pub trait ExportSource {
    /// Human-readable description of where this source reads from.
    fn describe(&self) -> String;

    /// Read every conversation the source contains.
    ///
    /// Recoverable damage is reported through [`ConversationSet::warnings`];
    /// only damage that leaves no usable conversations is an error.
    fn load(&self) -> Result<ConversationSet>;

    /// Return the original, unmodified JSON object for one conversation id.
    ///
    /// The domain model drops unknown fields on purpose; this is the escape
    /// hatch that hands them back. `Ok(None)` means the id is simply not in
    /// this source — only a damaged or misshapen document is an error.
    fn raw_conversation(&self, conversation_id: &str) -> Result<Option<serde_json::Value>>;
}

pub use loader::{load, open, raw_conversation};
