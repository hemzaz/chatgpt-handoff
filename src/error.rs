//! Domain error types.
//!
//! The library layer never uses `anyhow`; every fallible operation returns a
//! concrete [`Error`]. `anyhow` is reserved for the binary boundary in
//! `main.rs`, where errors are only formatted for humans.

use std::path::PathBuf;

/// Result alias used throughout the library.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level library error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{origin} is not valid ChatGPT export JSON")]
    Json {
        origin: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "{origin} does not look like a ChatGPT export: expected a JSON array of conversations \
         or an object with a `conversations` array"
    )]
    UnexpectedJsonShape { origin: String },

    #[error("failed to read archive {path}")]
    Archive {
        path: PathBuf,
        #[source]
        source: zip::result::ZipError,
    },

    #[error(
        "no `conversations.json` found in archive {path}; \
         is this really a ChatGPT data export?"
    )]
    NoConversationsInArchive { path: PathBuf },

    #[error(
        "archive entry `{entry}` declares {declared} bytes uncompressed, \
         over the {limit} byte safety limit (raise it with --max-unpacked-bytes)"
    )]
    ArchiveEntryTooLarge {
        entry: String,
        declared: u64,
        limit: u64,
    },

    #[error("archive entry `{entry}` has an unsafe path and was refused")]
    UnsafeArchivePath { entry: String },

    #[error(transparent)]
    Graph(#[from] GraphError),

    #[error(transparent)]
    Select(#[from] SelectError),

    #[error("{path} already exists\nhint: pass --force to overwrite")]
    OutputExists { path: PathBuf },
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn write(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Write {
            path: path.into(),
            source,
        }
    }
}

/// Errors that make it impossible to produce *any* conversation branch.
///
/// Recoverable graph damage is reported as a [`crate::graph::BranchWarning`]
/// instead, so that partially broken exports still yield useful output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("conversation `{id}` has an empty node mapping")]
    EmptyMapping { id: String },

    #[error("conversation `{id}` contains no messages on any branch")]
    NoMessages { id: String },
}

/// Errors from resolving a user-supplied conversation selector.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("no conversation matches {query}")]
    NotFound { query: String },

    #[error("the export contains no conversations")]
    Empty,

    #[error("no conversation selected\nhint: pass --conversation ID, --title TITLE, or a query")]
    NoSelector,

    #[error(
        "{query} is ambiguous; {} candidates matched:\n{}\nhint: re-run with --conversation ID, \
         or --pick to choose interactively",
        candidates.len(),
        render_candidates(candidates)
    )]
    Ambiguous {
        query: String,
        candidates: Vec<AmbiguousCandidate>,
    },
}

/// Render ambiguous candidates as an indented, score-ordered list.
///
/// Refusing to guess is only half the job: the user also has to be able to see
/// what matched, or they have no way to narrow the selector.
fn render_candidates(candidates: &[AmbiguousCandidate]) -> String {
    candidates
        .iter()
        .map(|candidate| {
            format!(
                "  {:>3}  {}  {}",
                candidate.score,
                crate::text::truncate_graphemes(&candidate.id, 12),
                crate::text::sanitize_display(&candidate.title)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One candidate rendered when a selector is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousCandidate {
    pub id: String,
    pub title: String,
    pub score: u32,
}
