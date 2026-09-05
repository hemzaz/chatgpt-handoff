//! Dispatch between the `.json` and `.zip` export readers.
//!
//! The format is decided by *content*, not by file name. Users rename their
//! downloads, mail clients rewrite extensions, and a ChatGPT export saved as
//! `chatgpt.json` is very often still a zip. Sniffing the magic bytes makes
//! both directions work and removes an extension-based trust boundary.

use std::io::Read;
use std::path::Path;

use super::json::JsonSource;
use super::zip::ZipSource;
use super::{ExportSource, LoadOptions};
use crate::error::{Error, Result};
use crate::model::ConversationSet;

/// Local file header (`PK\x03\x04`), end-of-central-directory for an empty
/// archive (`PK\x05\x06`), and the spanned/split marker (`PK\x07\x08`).
const ZIP_MAGICS: [&[u8; 4]; 3] = [b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08"];

/// Whether `path` holds a zip archive.
///
/// Magic bytes win over the extension; the extension is consulted only when the
/// file cannot be read at all, so that the caller still gets a sensible reader
/// (and therefore a sensible error) instead of guessing wrong.
pub fn looks_like_zip(path: &Path) -> bool {
    match read_magic(path) {
        Ok(magic) => ZIP_MAGICS.iter().any(|expected| magic == expected[..]),
        Err(_) => has_zip_extension(path),
    }
}

fn read_magic(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut magic = Vec::with_capacity(4);
    // `take` + `read_to_end` handles short reads and short files without ever
    // reading more than the signature.
    file.take(4).read_to_end(&mut magic)?;
    Ok(magic)
}

fn has_zip_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// Pick the right reader for `path` without loading it.
///
/// # Errors
///
/// [`Error::Io`] if the path does not exist or its metadata cannot be read.
pub fn open(path: &Path, options: &LoadOptions) -> Result<Box<dyn ExportSource>> {
    // Fail here, with the path in the message, rather than letting a
    // non-existent file turn into a confusing "not valid export JSON".
    std::fs::metadata(path).map_err(|source| Error::io(path, source))?;

    if looks_like_zip(path) {
        Ok(Box::new(ZipSource::new(path, *options)))
    } else {
        Ok(Box::new(JsonSource::new(path)))
    }
}

/// Open `path` and read every conversation it contains.
pub fn load(path: &Path, options: &LoadOptions) -> Result<ConversationSet> {
    open(path, options)?.load()
}

/// Return the original, unmodified JSON object for one conversation id.
///
/// Dispatches exactly like [`load`] — magic bytes, not the extension — and
/// applies the same archive protections. Unknown fields that the domain model
/// discards are preserved verbatim, which is the whole point: this backs
/// `extract --raw`.
///
/// Returns `Ok(None)` when the export simply does not contain that id. Only one
/// parsed document is ever resident at a time.
///
/// # Errors
///
/// [`Error::Io`] for an unreadable path, plus whatever the underlying reader
/// raises for a damaged archive or document.
pub fn raw_conversation(
    path: &Path,
    options: &LoadOptions,
    conversation_id: &str,
) -> Result<Option<serde_json::Value>> {
    open(path, options)?.raw_conversation(conversation_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn write_zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = ::zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).expect("start zip entry");
            writer.write_all(body.as_bytes()).expect("write zip entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    #[test]
    fn magic_bytes_beat_the_extension_in_both_directions() {
        let dir = tempfile::tempdir().expect("tempdir");

        let zip_named_json = dir.path().join("export.json");
        std::fs::write(
            &zip_named_json,
            write_zip_bytes(&[("conversations.json", r#"[{"id": "a"}]"#)]),
        )
        .expect("write fixture");
        assert!(looks_like_zip(&zip_named_json));

        let json_named_zip = dir.path().join("export.zip");
        std::fs::write(&json_named_zip, r#"[{"id": "a"}]"#).expect("write fixture");
        assert!(!looks_like_zip(&json_named_zip));
    }

    #[test]
    fn unreadable_paths_fall_back_to_the_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(looks_like_zip(&dir.path().join("absent.ZIP")));
        assert!(!looks_like_zip(&dir.path().join("absent.json")));
        assert!(!looks_like_zip(&dir.path().join("absent")));
    }

    #[test]
    fn empty_and_tiny_files_are_not_archives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = dir.path().join("empty.zip");
        std::fs::write(&empty, b"").expect("write fixture");
        assert!(!looks_like_zip(&empty));

        let tiny = dir.path().join("tiny.zip");
        std::fs::write(&tiny, b"PK").expect("write fixture");
        assert!(!looks_like_zip(&tiny));
    }

    #[test]
    fn empty_archive_signature_counts_as_a_zip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty-archive.bin");
        std::fs::write(&path, write_zip_bytes(&[])).expect("write fixture");
        assert!(looks_like_zip(&path));
    }

    #[test]
    fn load_dispatches_to_the_json_reader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, r#"[{"id": "a", "title": "hi"}]"#).expect("write fixture");

        let set = load(&path, &LoadOptions::default()).expect("json loads");
        assert_eq!(set.len(), 1);
        assert_eq!(set.source, path.display().to_string());
    }

    #[test]
    fn load_dispatches_to_the_zip_reader_despite_a_json_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("export.json");
        std::fs::write(
            &path,
            write_zip_bytes(&[("conversations.json", r#"[{"id": "a"}]"#)]),
        )
        .expect("write fixture");

        let set = load(&path, &LoadOptions::default()).expect("zip loads");
        assert_eq!(set.len(), 1);
        assert!(
            set.source.ends_with("!conversations.json"),
            "{}",
            set.source
        );
    }

    const RAW_FIXTURE: &str =
        r#"[{"id": "a", "title": "hi", "future_unknown_field": {"nested": 7}}]"#;

    #[test]
    fn raw_conversation_dispatches_to_the_json_reader() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, RAW_FIXTURE).expect("write fixture");

        let raw = raw_conversation(&path, &LoadOptions::default(), "a")
            .expect("lookup succeeds")
            .expect("id `a` is present");
        assert_eq!(raw["future_unknown_field"]["nested"], 7);
        assert_eq!(
            raw_conversation(&path, &LoadOptions::default(), "absent").expect("lookup succeeds"),
            None
        );
    }

    #[test]
    fn raw_conversation_dispatches_to_the_zip_reader_despite_a_json_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("export.json");
        std::fs::write(
            &path,
            write_zip_bytes(&[("conversations.json", RAW_FIXTURE)]),
        )
        .expect("write fixture");

        let raw = raw_conversation(&path, &LoadOptions::default(), "a")
            .expect("lookup succeeds")
            .expect("id `a` is present");
        assert_eq!(raw["future_unknown_field"]["nested"], 7);
    }

    #[test]
    fn raw_conversation_of_a_missing_path_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = raw_conversation(
            &dir.path().join("absent.json"),
            &LoadOptions::default(),
            "a",
        )
        .expect_err("missing path must fail");
        assert!(matches!(error, Error::Io { .. }));
    }

    #[test]
    fn opening_a_missing_path_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = open(&dir.path().join("absent.json"), &LoadOptions::default())
            .err()
            .expect("missing path must fail");
        assert!(matches!(error, Error::Io { .. }));
    }

    #[test]
    fn describe_reports_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, "[]").expect("write fixture");
        let source = open(&path, &LoadOptions::default()).expect("opens");
        assert_eq!(source.describe(), path.display().to_string());
    }
}
