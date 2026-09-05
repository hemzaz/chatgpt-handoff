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
        "{path} is {size} bytes, over the {limit} byte safety limit \
         (raise it with --max-unpacked-bytes)"
    )]
    InputTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    #[error(
        "archive entry `{entry}` declares {declared} bytes uncompressed, \
         over the {limit} byte safety limit (raise it with --max-unpacked-bytes)"
    )]
    ArchiveEntryTooLarge {
        entry: String,
        declared: u64,
        limit: u64,
    },

    /// The declared size passed the check but the stream did not.
    ///
    /// Kept distinct from [`Error::ArchiveEntryTooLarge`] because the two call
    /// for opposite responses: an honest oversize is fixed by raising the
    /// limit, while a header that understated its entry means the archive is
    /// corrupt or hostile and raising the limit is exactly the wrong move.
    #[error(
        "archive entry `{entry}` declared only {declared} bytes but delivered \
         more than the {limit} byte safety limit; the archive is corrupt or hostile"
    )]
    ArchiveEntrySizeMismatch {
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
        "{query} is ambiguous; {} candidates matched:\n{}\n{}",
        candidates.len(),
        render_candidates(candidates),
        disambiguation_hint(candidates)
    )]
    Ambiguous {
        query: String,
        candidates: Vec<AmbiguousCandidate>,
    },
}

/// Suggest a selector that can actually distinguish these candidates.
///
/// `--conversation ID` is the usual advice, but it is a dead end when the
/// candidates share an id — which happens when one export contains the same
/// conversation twice. Point at something that can discriminate instead.
fn disambiguation_hint(candidates: &[AmbiguousCandidate]) -> String {
    let ids_are_distinct = candidates
        .iter()
        .map(|candidate| &candidate.id)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        == candidates.len();

    if ids_are_distinct {
        "hint: re-run with --conversation ID, or --pick to choose interactively".to_string()
    } else {
        "hint: these candidates share a conversation id; select with --title TITLE, \
         or --pick to choose interactively"
            .to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, title: &str) -> AmbiguousCandidate {
        AmbiguousCandidate {
            id: id.to_string(),
            title: title.to_string(),
            score: 100,
        }
    }

    #[test]
    fn ambiguity_lists_every_candidate() {
        let err = SelectError::Ambiguous {
            query: "query \"notes\"".to_string(),
            candidates: vec![
                candidate("aaa-111", "Notes one"),
                candidate("bbb-222", "Notes two"),
            ],
        };
        let rendered = err.to_string();
        for expected in ["aaa-111", "bbb-222", "Notes one", "Notes two"] {
            assert!(
                rendered.contains(expected),
                "{expected} missing from:\n{rendered}"
            );
        }
    }

    #[test]
    fn hint_points_at_the_id_when_ids_can_discriminate() {
        let err = SelectError::Ambiguous {
            query: "q".to_string(),
            candidates: vec![candidate("aaa-111", "One"), candidate("bbb-222", "Two")],
        };
        assert!(err.to_string().contains("--conversation ID"));
    }

    #[test]
    fn hint_points_elsewhere_when_candidates_share_an_id() {
        // One export containing the same conversation twice: advising
        // `--conversation ID` would send the user in a circle.
        let err = SelectError::Ambiguous {
            query: "q".to_string(),
            candidates: vec![
                candidate("same-id", "Older copy"),
                candidate("same-id", "Newer copy"),
            ],
        };
        let rendered = err.to_string();
        assert!(rendered.contains("share a conversation id"), "{rendered}");
        assert!(rendered.contains("--title"), "{rendered}");
        assert!(!rendered.contains("--conversation ID"), "{rendered}");
    }

    #[test]
    fn candidate_titles_are_sanitized_before_display() {
        let escape = char::from_u32(27).unwrap_or('?');
        let err = SelectError::Ambiguous {
            query: "q".to_string(),
            candidates: vec![
                candidate("a", &format!("hostile{escape}[31m")),
                candidate("b", "plain"),
            ],
        };
        assert!(!err.to_string().contains(escape));
    }

    #[test]
    fn output_exists_error_names_the_escape_hatch() {
        let err = Error::OutputExists {
            path: std::path::PathBuf::from("handoff/context.md"),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("handoff/context.md"));
        assert!(rendered.contains("--force"));
    }
}

/// One candidate rendered when a selector is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousCandidate {
    pub id: String,
    pub title: String,
    pub score: u32,
}
