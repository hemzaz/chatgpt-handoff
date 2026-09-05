//! Reading ChatGPT exports out of an untrusted `.zip` archive.
//!
//! # Threat model
//!
//! The archive is attacker-controlled input. Three classes of attack matter:
//!
//! 1. **Path traversal.** We never extract to disk, so a `../../.ssh/authorized_keys`
//!    entry cannot overwrite anything — but entry names *are* interpolated into
//!    warnings and error messages that land on the user's terminal, and a future
//!    refactor that does write to disk must not silently inherit a hole. Names are
//!    therefore validated up front ([`is_safe_entry_name`]) and sanitized for
//!    display ([`crate::text::sanitize_display`]).
//! 2. **Zip bombs.** A tiny archive can declare (and deliver) gigabytes. Every
//!    candidate entry is checked against `LoadOptions::max_unpacked_bytes` twice:
//!    once against the *declared* uncompressed size, and once against the bytes
//!    actually delivered, because the declared size is written by the attacker
//!    and is free to lie.
//! 3. **Malformed JSON.** Delegated to [`crate::export::json`], which tolerates
//!    per-element damage rather than failing the whole load.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ExportSource, LoadOptions};
use crate::error::{Error, Result};
use crate::export::json;
use crate::model::{Conversation, ConversationSet};
use crate::text;

/// Default cap on the uncompressed size of a single archive entry (512 MiB).
///
/// Comfortably larger than any real ChatGPT export, small enough that a zip
/// bomb cannot exhaust memory before we notice.
pub const DEFAULT_MAX_UNPACKED_BYTES: u64 = 1 << 29;

/// Longest entry name we will echo back to the user, in grapheme clusters.
const MAX_DISPLAYED_NAME: usize = 120;

/// How many unsafe entry names we name individually in the warning list.
const MAX_NAMED_UNSAFE_ENTRIES: usize = 3;

/// Largest buffer we pre-allocate for one entry, regardless of its declared
/// size. Declared sizes are attacker-controlled, so a 512 MiB claim must not
/// buy a 512 MiB allocation before a single byte has been delivered.
const MAX_PREALLOCATED_ENTRY_BYTES: u64 = 1 << 20;

/// A `.zip` ChatGPT export on disk.
#[derive(Debug, Clone)]
pub struct ZipSource {
    /// Path to the archive.
    pub path: PathBuf,
    /// Safety limits applied while reading it.
    pub options: LoadOptions,
}

impl ZipSource {
    /// Point at a `.zip` export. No I/O happens until [`ExportSource::load`].
    pub fn new(path: impl AsRef<Path>, options: LoadOptions) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            options,
        }
    }

    fn archive_error(&self, source: ::zip::result::ZipError) -> Error {
        Error::Archive {
            path: self.path.clone(),
            source,
        }
    }
}

impl ExportSource for ZipSource {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn load(&self) -> Result<ConversationSet> {
        let mut archive = self.open_archive()?;

        let scan = self.scan_entries(&mut archive)?;
        if scan.candidates.is_empty() {
            return Err(Error::NoConversationsInArchive {
                path: self.path.clone(),
            });
        }
        let Scan {
            candidates,
            mut warnings,
            ..
        } = scan;
        let mut merge = Merge::default();

        for (index, name) in &candidates {
            let bytes = self.read_entry(&mut archive, *index, name)?;
            let origin = format!("{}!{}", self.path.display(), display_name(name));
            let parsed = json::parse_conversations_reporting(&bytes, &origin)?;
            // Drop the raw bytes before parsing the next entry: only one file's
            // worth of undecoded JSON is ever resident.
            drop(bytes);
            warnings.extend(json::skipped_warning(parsed.skipped, &origin));
            merge.absorb(parsed.conversations);
        }

        if candidates.len() > 1 {
            let names: Vec<String> = candidates.iter().map(|(_, n)| display_name(n)).collect();
            warnings.push(format!(
                "merged {} conversation files from the archive: {}",
                candidates.len(),
                names.join(", ")
            ));
        }

        Ok(ConversationSet {
            conversations: merge.into_conversations(),
            source: self.source_label(&candidates),
            warnings,
        })
    }

    fn raw_conversation(&self, conversation_id: &str) -> Result<Option<Value>> {
        let mut archive = self.open_archive()?;

        let scan = self.scan_entries(&mut archive)?;
        if scan.candidates.is_empty() {
            return Err(Error::NoConversationsInArchive {
                path: self.path.clone(),
            });
        }

        // `scan.candidates` is already in sorted-name order, so which copy wins
        // does not depend on the archive's internal ordering.
        let mut best: Option<Value> = None;
        for (index, name) in &scan.candidates {
            let bytes = self.read_entry(&mut archive, *index, name)?;
            let origin = format!("{}!{}", self.path.display(), display_name(name));
            let found = json::find_raw_conversation(&bytes, &origin, conversation_id)?;
            // Only the matched element survives the call; the rest of the parsed
            // document — and now the bytes — are released before the next entry.
            drop(bytes);

            let Some(found) = found else {
                continue;
            };
            // Same tie-break as the merge: freshest wins, incumbent on a draw.
            best = match best {
                Some(current)
                    if json::raw_update_time(&current) >= json::raw_update_time(&found) =>
                {
                    Some(current)
                }
                _ => Some(found),
            };
        }
        Ok(best)
    }
}

impl ZipSource {
    fn open_archive(&self) -> Result<::zip::ZipArchive<std::io::BufReader<std::fs::File>>> {
        let file = std::fs::File::open(&self.path).map_err(|e| Error::io(&self.path, e))?;
        ::zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| self.archive_error(e))
    }

    /// Walk the central directory once, classifying every entry.
    ///
    /// Nothing is extracted here; we only need names and the directory flag.
    fn scan_entries<R: Read + std::io::Seek>(
        &self,
        archive: &mut ::zip::ZipArchive<R>,
    ) -> Result<Scan> {
        let mut scan = Scan::default();

        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|e| self.archive_error(e))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            drop(entry);

            let safe = is_safe_entry_name(&name);
            if !is_conversations_entry(&name) {
                // An unrelated entry with a hostile name is not a reason to
                // refuse the whole archive — we are never going to read it.
                if !safe {
                    scan.note_unsafe(&name);
                }
                continue;
            }
            if !safe {
                // This one we *would* have read. Refuse the archive outright
                // rather than quietly loading a file whose name lies about
                // where it lives.
                return Err(Error::UnsafeArchivePath {
                    entry: display_name(&name),
                });
            }
            scan.candidates.push((index, name));
        }

        scan.finish();
        Ok(scan)
    }

    /// Read one entry fully into memory, refusing anything over the limit.
    fn read_entry<R: Read + std::io::Seek>(
        &self,
        archive: &mut ::zip::ZipArchive<R>,
        index: usize,
        name: &str,
    ) -> Result<Vec<u8>> {
        let limit = self.options.max_unpacked_bytes;
        let mut entry = archive.by_index(index).map_err(|e| self.archive_error(e))?;

        let declared = entry.size();
        if declared > limit {
            return Err(Error::ArchiveEntryTooLarge {
                entry: display_name(name),
                declared,
                limit,
            });
        }

        let capacity = declared.min(MAX_PREALLOCATED_ENTRY_BYTES) as usize;
        let mut bytes = Vec::with_capacity(capacity);
        // `take(limit + 1)` bounds the read at one byte past the limit: the
        // header's declared size is attacker-controlled and may understate the
        // real stream, so the check above is necessary but not sufficient.
        entry
            .by_ref()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| Error::io(&self.path, e))?;

        if bytes.len() as u64 > limit {
            return Err(Error::ArchiveEntryTooLarge {
                entry: display_name(name),
                declared: bytes.len() as u64,
                limit,
            });
        }
        Ok(bytes)
    }

    /// `export.zip!conversations.json`, or `export.zip!3 files` when merged.
    fn source_label(&self, candidates: &[(usize, String)]) -> String {
        match candidates {
            [(_, only)] => format!("{}!{}", self.path.display(), display_name(only)),
            many => format!("{}!{} files", self.path.display(), many.len()),
        }
    }
}

/// Outcome of the central-directory scan.
#[derive(Debug, Default)]
struct Scan {
    /// `(archive index, entry name)` for every entry we intend to read.
    candidates: Vec<(usize, String)>,
    warnings: Vec<String>,
    unsafe_names: Vec<String>,
    unsafe_count: usize,
}

impl Scan {
    fn note_unsafe(&mut self, name: &str) {
        self.unsafe_count = self.unsafe_count.saturating_add(1);
        if self.unsafe_names.len() < MAX_NAMED_UNSAFE_ENTRIES {
            self.unsafe_names.push(display_name(name));
        }
    }

    /// Sort candidates and fold the unsafe-name tally into one warning.
    ///
    /// Sorting by name makes a merge of several conversation files produce the
    /// same result regardless of the order the archive happens to store them
    /// in; the index is the tie-break for duplicate names.
    fn finish(&mut self) {
        self.candidates
            .sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        if self.unsafe_count > 0 {
            // One bounded warning, not one per entry: a hostile archive can
            // contain a million bad names and must not flood the terminal.
            let mut message = format!(
                "ignored {} archive {} with unsafe paths: {}",
                self.unsafe_count,
                if self.unsafe_count == 1 {
                    "entry"
                } else {
                    "entries"
                },
                self.unsafe_names.join(", ")
            );
            if self.unsafe_count > self.unsafe_names.len() {
                message.push_str(", …");
            }
            self.warnings.push(message);
        }
    }
}

/// Accumulates conversations from several files, deduplicating by id.
#[derive(Debug, Default)]
struct Merge {
    conversations: Vec<Conversation>,
    by_id: HashMap<String, usize>,
}

impl Merge {
    /// Add conversations, keeping the freshest copy of any duplicate id.
    ///
    /// Conversations are moved, never cloned; only the id is copied, as the
    /// index key.
    fn absorb(&mut self, incoming: Vec<Conversation>) {
        for conversation in incoming {
            match self.by_id.get(&conversation.id).copied() {
                Some(existing) => {
                    let Some(slot) = self.conversations.get_mut(existing) else {
                        continue;
                    };
                    if freshness(&conversation) > freshness(slot) {
                        *slot = conversation;
                    }
                }
                None => {
                    self.by_id
                        .insert(conversation.id.clone(), self.conversations.len());
                    self.conversations.push(conversation);
                }
            }
        }
    }

    fn into_conversations(self) -> Vec<Conversation> {
        self.conversations
    }
}

/// Sort key for "which copy of this conversation is newer".
///
/// A missing `update_time` counts as older than any timestamp, so a dated copy
/// always beats an undated one. `NaN` compares false against everything, which
/// leaves the incumbent in place — deterministic, and the best available answer.
fn freshness(conversation: &Conversation) -> f64 {
    conversation.update_time.unwrap_or(f64::NEG_INFINITY)
}

/// Render an entry name for human consumption.
///
/// Entry names are attacker-controlled and end up on a terminal, so control
/// characters and bidi overrides are stripped and the length is bounded.
fn display_name(name: &str) -> String {
    text::truncate_graphemes(&text::sanitize_display(name), MAX_DISPLAYED_NAME).into_owned()
}

/// Whether an archive entry name is safe to act on.
///
/// Rejects absolute paths, drive-qualified paths, any `..` component, empty
/// components, and control characters. Both `/` and `\` count as separators:
/// a zip written on Windows can use either, and a checker that only knows
/// about `/` is trivially bypassed with `..\..\`.
pub fn is_safe_entry_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // NUL and other control characters have no legitimate place in a path and
    // are the classic truncation / terminal-injection vector.
    if name.chars().any(char::is_control) {
        return false;
    }
    // Absolute: rooted at `/`, `\`, or a Windows drive letter (`C:` — also
    // catches the `C:foo` drive-relative form).
    let bytes = name.as_bytes();
    match bytes {
        [b'/' | b'\\', ..] => return false,
        [drive, b':', ..] if drive.is_ascii_alphabetic() => return false,
        _ => {}
    }
    name.split(['/', '\\'])
        .all(|component| !component.is_empty() && component != "..")
}

/// Whether an archive entry is one of the export's conversation JSON files.
///
/// Real exports ship `conversations.json`, but a user who unzipped and re-zipped
/// (or downloaded twice) can end up with `conversations (1).json` or a
/// `chatgpt-export/conversations.json` prefix, so the final path component is
/// matched against `conversations*.json` and `*conversations.json`
/// case-insensitively.
///
/// macOS resource forks (`__MACOSX/…`, `._name`) are byte-for-byte *not* JSON
/// and are excluded explicitly, since `__MACOSX/conversations.json` would
/// otherwise match and fail to parse.
pub fn is_conversations_entry(name: &str) -> bool {
    let mut components = name.split(['/', '\\']).filter(|c| !c.is_empty());
    if components
        .clone()
        .any(|c| c.eq_ignore_ascii_case("__MACOSX"))
    {
        return false;
    }
    let Some(final_component) = components.next_back() else {
        return false;
    };
    if final_component.starts_with('.') {
        return false;
    }

    let lower = final_component.to_ascii_lowercase();
    let Some(stem) = lower.strip_suffix(".json") else {
        return false;
    };
    stem.starts_with("conversations") || stem.ends_with("conversations")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// Build an in-memory zip, then write it to a temp dir and return the path.
    fn write_zip(dir: &tempfile::TempDir, name: &str, entries: &[(&str, &str)]) -> PathBuf {
        let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = ::zip::write::SimpleFileOptions::default();
        for (entry_name, body) in entries {
            writer
                .start_file(*entry_name, options)
                .expect("start zip entry");
            writer
                .write_all(body.as_bytes())
                .expect("write zip entry body");
        }
        let buffer = writer.finish().expect("finish zip").into_inner();

        let path = dir.path().join(name);
        std::fs::write(&path, buffer).expect("write archive to disk");
        path
    }

    fn load(path: &Path, options: LoadOptions) -> Result<ConversationSet> {
        ZipSource::new(path, options).load()
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for name in [
            "../../etc/passwd",
            "..",
            "../conversations.json",
            "/abs/path",
            "\\abs\\path",
            "C:\\win",
            "c:relative",
            "a/../../b",
            "a\\..\\b",
            "a//b",
            "a/",
            "",
            "bad\u{0}name.json",
            "bell\u{7}.json",
        ] {
            assert!(!is_safe_entry_name(name), "should be rejected: {name:?}");
        }
    }

    #[test]
    fn accepts_ordinary_entry_names() {
        for name in [
            "conversations.json",
            "chatgpt-export/conversations.json",
            "a/b/c.json",
            "with space.json",
            "שיחות.json",
            "a..b/c.json",
        ] {
            assert!(is_safe_entry_name(name), "should be accepted: {name:?}");
        }
    }

    #[test]
    fn recognises_conversation_files() {
        for name in [
            "conversations.json",
            "foo/conversations.json",
            "conversations (1).json",
            "Conversations.JSON",
            "chatgpt-export/my_conversations.json",
        ] {
            assert!(is_conversations_entry(name), "should match: {name:?}");
        }
    }

    #[test]
    fn ignores_non_conversation_files() {
        for name in [
            "chat.html",
            "__MACOSX/conversations.json",
            "__macosx/conversations.json",
            "._conversations.json",
            "foo/._conversations.json",
            "conversations.json.bak",
            "conversations",
            "user.json",
            "",
        ] {
            assert!(!is_conversations_entry(name), "should not match: {name:?}");
        }
    }

    #[test]
    fn loads_conversations_from_an_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[
                ("chat.html", "<html></html>"),
                (
                    "conversations.json",
                    r#"[{"id": "a", "title": "First"}, {"id": "b"}]"#,
                ),
            ],
        );

        let set = load(&path, LoadOptions::default()).expect("archive loads");
        assert_eq!(set.len(), 2);
        assert_eq!(set.source, format!("{}!conversations.json", path.display()));
        assert!(set.warnings.is_empty(), "{:?}", set.warnings);
    }

    #[test]
    fn loads_from_a_nested_export_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[("chatgpt-export/conversations.json", r#"[{"id": "a"}]"#)],
        );
        let set = load(&path, LoadOptions::default()).expect("nested archive loads");
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn archive_without_conversations_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(&dir, "export.zip", &[("chat.html", "<html></html>")]);

        let error = load(&path, LoadOptions::default()).expect_err("no candidates");
        assert!(matches!(error, Error::NoConversationsInArchive { .. }));
    }

    #[test]
    fn oversized_entry_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!(r#"[{{"id": "a", "title": "{}"}}]"#, "x".repeat(4096));
        let path = write_zip(&dir, "export.zip", &[("conversations.json", &body)]);

        let error = load(
            &path,
            LoadOptions {
                max_unpacked_bytes: 64,
            },
        )
        .expect_err("declared size is over the limit");
        match error {
            Error::ArchiveEntryTooLarge {
                entry,
                declared,
                limit,
            } => {
                assert_eq!(entry, "conversations.json");
                assert_eq!(limit, 64);
                assert!(declared > 64, "declared {declared}");
            }
            other => panic!("expected ArchiveEntryTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn generous_limit_allows_a_normal_export() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[("conversations.json", r#"[{"id": "a"}]"#)],
        );
        assert!(
            load(
                &path,
                LoadOptions {
                    max_unpacked_bytes: 4096
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn duplicate_ids_across_files_keep_the_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[
                (
                    "conversations.json",
                    r#"[{"id": "dup", "title": "old", "update_time": 100.0}, {"id": "solo"}]"#,
                ),
                (
                    "conversations (1).json",
                    r#"[{"id": "dup", "title": "new", "update_time": 200.0}]"#,
                ),
            ],
        );

        let set = load(&path, LoadOptions::default()).expect("merge succeeds");
        assert_eq!(set.len(), 2, "duplicate id must collapse to one entry");
        let dup = set.find_by_id("dup").expect("dup survives");
        assert_eq!(dup.display_title(), "new");
        assert_eq!(set.source, format!("{}!2 files", path.display()));
        assert!(
            set.warnings
                .iter()
                .any(|w| w.contains("merged 2 conversation files")),
            "{:?}",
            set.warnings
        );
    }

    #[test]
    fn merge_order_does_not_depend_on_archive_order() {
        let newest = r#"[{"id": "dup", "title": "new", "update_time": 200.0}]"#;
        let oldest = r#"[{"id": "dup", "title": "old", "update_time": 100.0}]"#;

        for entries in [
            [
                ("conversations.json", oldest),
                ("conversations (1).json", newest),
            ],
            [
                ("conversations.json", newest),
                ("conversations (1).json", oldest),
            ],
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let (a, b) = (entries[0], entries[1]);
            let path = write_zip(&dir, "export.zip", &[a, b]);
            let set = load(&path, LoadOptions::default()).expect("merge succeeds");
            assert_eq!(set.len(), 1);
            assert_eq!(
                set.find_by_id("dup").expect("dup survives").display_title(),
                "new"
            );
        }
    }

    #[test]
    fn missing_update_time_loses_to_a_dated_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[
                (
                    "conversations.json",
                    r#"[{"id": "dup", "title": "undated"}]"#,
                ),
                (
                    "conversations (1).json",
                    r#"[{"id": "dup", "title": "dated", "update_time": 1.0}]"#,
                ),
            ],
        );
        let set = load(&path, LoadOptions::default()).expect("merge succeeds");
        assert_eq!(
            set.find_by_id("dup").expect("dup survives").display_title(),
            "dated"
        );
    }

    #[test]
    fn malformed_entries_inside_an_archive_become_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[("conversations.json", r#"[{"id": "a"}, 12]"#)],
        );
        let set = load(&path, LoadOptions::default()).expect("bad element is tolerated");
        assert_eq!(set.len(), 1);
        assert!(
            set.warnings
                .iter()
                .any(|w| w.contains("malformed conversation entry")),
            "{:?}",
            set.warnings
        );
    }

    #[test]
    fn unparseable_json_inside_an_archive_is_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(&dir, "export.zip", &[("conversations.json", "{not json")]);
        let error = load(&path, LoadOptions::default()).expect_err("invalid JSON");
        match error {
            Error::Json { origin, .. } => assert!(
                origin.ends_with("!conversations.json"),
                "origin should name the entry, got {origin}"
            ),
            other => panic!("expected Json error, got {other:?}"),
        }
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_an_archive_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not.zip");
        std::fs::write(&path, b"definitely not a zip").expect("write fixture");
        let error = load(&path, LoadOptions::default()).expect_err("not an archive");
        assert!(matches!(error, Error::Archive { .. }));
    }

    #[test]
    fn a_missing_archive_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = load(&dir.path().join("absent.zip"), LoadOptions::default())
            .expect_err("missing archive");
        assert!(matches!(error, Error::Io { .. }));
    }

    #[test]
    fn raw_conversation_survives_the_archive_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[
                ("chat.html", "<html></html>"),
                (
                    "conversations.json",
                    r#"[{"id": "a", "title": "First", "future_unknown_field": {"nested": [1, 2]}}]"#,
                ),
            ],
        );

        let source = ZipSource::new(&path, LoadOptions::default());
        let raw = source
            .raw_conversation("a")
            .expect("lookup succeeds")
            .expect("id `a` is present");
        assert_eq!(raw["future_unknown_field"]["nested"][1], 2);
        assert_eq!(raw["title"], "First");
    }

    #[test]
    fn raw_conversation_of_an_absent_id_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(
            &dir,
            "export.zip",
            &[("conversations.json", r#"[{"id": "a"}]"#)],
        );
        let found = ZipSource::new(&path, LoadOptions::default())
            .raw_conversation("nope")
            .expect("lookup succeeds");
        assert_eq!(found, None);
    }

    #[test]
    fn raw_conversation_prefers_the_newest_duplicate_across_files() {
        let newest = r#"[{"id": "dup", "update_time": 200.0, "tag": "new"}]"#;
        let oldest = r#"[{"id": "dup", "update_time": 100.0, "tag": "old"}]"#;

        for entries in [
            [
                ("conversations.json", oldest),
                ("conversations (1).json", newest),
            ],
            [
                ("conversations.json", newest),
                ("conversations (1).json", oldest),
            ],
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = write_zip(&dir, "export.zip", &[entries[0], entries[1]]);
            let raw = ZipSource::new(&path, LoadOptions::default())
                .raw_conversation("dup")
                .expect("lookup succeeds")
                .expect("id `dup` is present");
            assert_eq!(raw["tag"], "new");
        }
    }

    #[test]
    fn raw_conversation_respects_the_size_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!(r#"[{{"id": "a", "title": "{}"}}]"#, "x".repeat(4096));
        let path = write_zip(&dir, "export.zip", &[("conversations.json", &body)]);

        let error = ZipSource::new(
            &path,
            LoadOptions {
                max_unpacked_bytes: 64,
            },
        )
        .raw_conversation("a")
        .expect_err("declared size is over the limit");
        assert!(matches!(error, Error::ArchiveEntryTooLarge { .. }));
    }

    #[test]
    fn raw_conversation_without_conversation_files_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_zip(&dir, "export.zip", &[("chat.html", "<html></html>")]);
        let error = ZipSource::new(&path, LoadOptions::default())
            .raw_conversation("a")
            .expect_err("no candidates");
        assert!(matches!(error, Error::NoConversationsInArchive { .. }));
    }

    #[test]
    fn entry_names_are_sanitized_before_display() {
        assert!(!display_name("evil\u{202e}gnp.exe.json").contains('\u{202e}'));
        assert!(!display_name("bell\u{7}.json").contains('\u{7}'));
        assert!(text::grapheme_count(&display_name(&"x".repeat(500))) <= MAX_DISPLAYED_NAME + 1);
    }

    #[test]
    fn freshness_orders_missing_timestamps_last() {
        let parse =
            |body: &str| json::parse_conversations(body.as_bytes(), "t").expect("fixture parses");
        let dated = parse(r#"[{"id": "a", "update_time": 1.0}]"#);
        let undated = parse(r#"[{"id": "a"}]"#);
        let (Some(dated), Some(undated)) = (dated.first(), undated.first()) else {
            panic!("fixtures produce one conversation each");
        };
        assert!(freshness(dated) > freshness(undated));
    }
}
