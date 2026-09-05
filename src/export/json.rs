//! Parsing ChatGPT export JSON out of untrusted bytes.
//!
//! Two top-level shapes occur in the wild: a bare array of conversation
//! objects (what `conversations.json` contains today) and an object wrapping
//! that array under a `conversations` key (what some re-exporters and
//! third-party tools emit). Both are accepted; anything else is reported as
//! [`Error::UnexpectedJsonShape`] rather than being coerced into nonsense.
//!
//! Element-level damage is *tolerated*, not fatal: one corrupt entry in an
//! export of two thousand conversations must not cost the user the other one
//! thousand nine hundred and ninety-nine.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ExportSource, LoadOptions};
use crate::error::{Error, Result};
use crate::model::{Conversation, ConversationSet, RawConversation};

/// UTF-8 byte-order mark. Some exporters (and every Windows text editor a user
/// might have opened the file in) prepend one; `serde_json` rejects it, so we
/// strip it before parsing.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Upper bound on the number of elements we pre-allocate for.
///
/// The element count comes from attacker-controlled JSON. It is already bounded
/// by the parsed document in memory, but a document of `[]`-heavy elements can
/// still claim a very large count cheaply, so the reservation is capped and the
/// vector is allowed to grow naturally beyond it.
const MAX_PREALLOCATED_CONVERSATIONS: usize = 4096;

/// Largest buffer we pre-allocate for a file read, regardless of the size the
/// filesystem reports. Mirrors the archive reader's reservation cap.
const MAX_PREALLOCATED_READ_BYTES: u64 = 1 << 20;

/// A `.json` export file on disk.
#[derive(Debug, Clone)]
pub struct JsonSource {
    /// Path to the export file.
    pub path: PathBuf,
    /// Safety limits. A loose `.json` file is exactly as easy to hand someone
    /// as a hostile zip, so `max_unpacked_bytes` applies here too — the flag
    /// promises a general limit, not a zip-only one.
    pub options: LoadOptions,
}

impl JsonSource {
    /// Point at a `.json` export file with default limits.
    ///
    /// No I/O happens until [`ExportSource::load`].
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::with_options(path, LoadOptions::default())
    }

    /// Point at a `.json` export file with explicit limits.
    pub fn with_options(path: impl AsRef<Path>, options: LoadOptions) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            options,
        }
    }

    /// Read the whole file, refusing anything over `max_unpacked_bytes`.
    ///
    /// Checked twice, exactly as the archive reader does it: once against the
    /// size the filesystem reports, and once against the bytes actually
    /// delivered, since a file can grow (or a synthetic filesystem can lie)
    /// between the `stat` and the read.
    fn read_capped(&self) -> Result<Vec<u8>> {
        let limit = self.options.max_unpacked_bytes;
        let file = std::fs::File::open(&self.path).map_err(|e| Error::io(&self.path, e))?;

        let reported = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if reported > limit {
            return Err(super::input_too_large(&self.path, reported, limit));
        }

        let mut bytes = Vec::with_capacity(reported.min(MAX_PREALLOCATED_READ_BYTES) as usize);
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| Error::io(&self.path, e))?;

        if bytes.len() as u64 > limit {
            // The file grew (or the filesystem lied) between the stat and the
            // read. Same refusal, and the reported size is now irrelevant.
            return Err(super::input_too_large(
                &self.path,
                bytes.len() as u64,
                limit,
            ));
        }
        Ok(bytes)
    }
}

impl ExportSource for JsonSource {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn load(&self) -> Result<ConversationSet> {
        let bytes = self.read_capped()?;
        let origin = self.describe();
        let parsed = parse_conversations_reporting(&bytes, &origin)?;
        drop(bytes);
        let warnings = parsed.warnings(&origin);
        Ok(ConversationSet {
            conversations: parsed.conversations,
            warnings,
            source: origin,
        })
    }

    fn raw_conversation(&self, conversation_id: &str) -> Result<Option<Value>> {
        let bytes = self.read_capped()?;
        find_raw_conversation(&bytes, &self.describe(), conversation_id)
    }
}

/// Result of parsing one export document.
#[derive(Debug, Clone)]
pub struct ParsedConversations {
    /// Conversations that parsed successfully, deduplicated by id, in
    /// first-occurrence order.
    pub conversations: Vec<Conversation>,
    /// Array elements that were not a usable conversation object and were
    /// therefore skipped.
    pub skipped: usize,
    /// Duplicate ids that were collapsed into a single conversation.
    pub collapsed: usize,
}

impl ParsedConversations {
    /// Warnings describing everything that was skipped or collapsed.
    pub fn warnings(&self, origin: &str) -> Vec<String> {
        [
            skipped_warning(self.skipped, origin),
            collapsed_warning(self.collapsed, origin),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Deduplicates conversations by id, keeping the freshest copy.
///
/// This is the *single* definition of "the same conversation seen twice", used
/// by the loose-JSON reader for duplicates within one document and by the
/// archive reader for duplicates across several files. Having two definitions
/// is what let `.json` and `.zip` inputs disagree about the same bytes.
///
/// # Rule
///
/// The copy with the greatest `update_time` wins; a missing `update_time`
/// counts as older than any timestamp, so a dated copy always beats an undated
/// one. On an exact tie — including two undated copies — the **first**
/// occurrence in traversal order is kept, where traversal order is document
/// order within a file and sorted-entry-name order across archive entries.
/// That makes the outcome independent of how the exporter or the archive
/// happened to order things.
#[derive(Debug, Default)]
pub(crate) struct Dedupe {
    conversations: Vec<Conversation>,
    by_id: HashMap<String, usize>,
    collapsed: usize,
}

impl Dedupe {
    /// Add conversations, collapsing any id already seen.
    ///
    /// Conversations are moved, never cloned; only the id is copied, as the
    /// index key.
    pub(crate) fn absorb(&mut self, incoming: impl IntoIterator<Item = Conversation>) {
        for conversation in incoming {
            match self.by_id.get(&conversation.id).copied() {
                Some(existing) => {
                    self.collapsed = self.collapsed.saturating_add(1);
                    let Some(slot) = self.conversations.get_mut(existing) else {
                        continue;
                    };
                    // Strictly greater: a tie leaves the incumbent in place, so
                    // the first occurrence wins. `NaN` also compares false,
                    // which lands in the same deterministic place.
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

    /// How many duplicates have been collapsed so far.
    pub(crate) fn collapsed(&self) -> usize {
        self.collapsed
    }

    pub(crate) fn into_conversations(self) -> Vec<Conversation> {
        self.conversations
    }
}

/// Sort key for "which copy of this conversation is newer".
pub(crate) fn freshness(conversation: &Conversation) -> f64 {
    conversation.update_time.unwrap_or(f64::NEG_INFINITY)
}

/// Parse an export document, discarding the count of skipped elements.
///
/// Prefer [`parse_conversations_reporting`] when the caller can surface the
/// skip count to the user.
pub fn parse_conversations(bytes: &[u8], origin: &str) -> Result<Vec<Conversation>> {
    Ok(parse_conversations_reporting(bytes, origin)?.conversations)
}

/// Parse an export document, reporting how many elements had to be skipped.
///
/// `origin` is only used for error messages — it is a file path or a
/// `archive.zip!entry.json` label, never opened.
///
/// # Errors
///
/// - [`Error::Json`] if the bytes are not valid JSON at all.
/// - [`Error::UnexpectedJsonShape`] if the document is valid JSON but is
///   neither an array nor an object carrying a `conversations` array.
pub fn parse_conversations_reporting(bytes: &[u8], origin: &str) -> Result<ParsedConversations> {
    let items = document_elements(bytes, origin)?;

    let mut deduped = Dedupe::default();
    deduped
        .conversations
        .reserve(items.len().min(MAX_PREALLOCATED_CONVERSATIONS));
    let mut skipped = 0usize;
    for (index, item) in items.into_iter().enumerate() {
        // A non-object element, or an object whose typed fields have the wrong
        // shape, costs us that one conversation and nothing else. The index is
        // the document position, so synthetic ids stay stable across loads.
        match serde_json::from_value::<RawConversation>(item) {
            // Deduplication happens here rather than in the archive reader so
            // that a `.json` file and a `.zip` of those exact bytes cannot
            // disagree about how many conversations they contain.
            Ok(raw) => deduped.absorb([raw.into_conversation(index)]),
            Err(_) => skipped = skipped.saturating_add(1),
        }
    }

    Ok(ParsedConversations {
        collapsed: deduped.collapsed(),
        conversations: deduped.into_conversations(),
        skipped,
    })
}

/// Find the *original* JSON object for one conversation id.
///
/// The domain model is deliberately lossy — unknown fields are dropped so that
/// a schema change can never fail a load. `extract --raw` exists to give the
/// user those fields back, so this returns the untouched element straight out
/// of the document.
///
/// An element matches when its `id` **or** `conversation_id` equals
/// `conversation_id`. A conversation that carried neither (and therefore got a
/// synthetic `unknown-N` id) has no original to return and yields `Ok(None)`.
///
/// When the same id appears more than once in one document, the copy with the
/// greatest `update_time` wins — the same rule
/// [`crate::export::zip`] uses when merging several files.
///
/// # Errors
///
/// Same as [`parse_conversations_reporting`]: [`Error::Json`] for invalid JSON,
/// [`Error::UnexpectedJsonShape`] for a valid document of the wrong shape. A
/// missing id is *not* an error.
pub fn find_raw_conversation(
    bytes: &[u8],
    origin: &str,
    conversation_id: &str,
) -> Result<Option<Value>> {
    let mut best: Option<Value> = None;
    for item in document_elements(bytes, origin)? {
        if !raw_id_matches(&item, conversation_id) {
            continue;
        }
        // Keep the incumbent on a tie so the earlier element wins, making the
        // result independent of how the exporter ordered duplicates.
        best = match best {
            Some(current) if raw_update_time(&current) >= raw_update_time(&item) => Some(current),
            _ => Some(item),
        };
    }
    Ok(best)
}

/// `update_time` of a raw element, with a missing or non-numeric value sorting
/// older than every real timestamp.
pub(crate) fn raw_update_time(item: &Value) -> f64 {
    item.get("update_time")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NEG_INFINITY)
}

fn raw_id_matches(item: &Value, conversation_id: &str) -> bool {
    ["id", "conversation_id"]
        .iter()
        .any(|key| item.get(key).and_then(Value::as_str) == Some(conversation_id))
}

/// Parse a document and hand back its conversation elements.
///
/// Shared by every reader so the accepted shapes — and the BOM handling — are
/// defined exactly once.
fn document_elements(bytes: &[u8], origin: &str) -> Result<Vec<Value>> {
    let bytes = bytes.strip_prefix(&UTF8_BOM).unwrap_or(bytes);

    let document: Value = serde_json::from_slice(bytes).map_err(|source| Error::Json {
        origin: origin.to_string(),
        source,
    })?;

    // `remove` rather than `get`: it moves the array out of the owned map, so
    // the elements are never cloned.
    match document {
        Value::Array(items) => Ok(items),
        Value::Object(mut map) => match map.remove("conversations") {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(unexpected_shape(origin)),
        },
        _ => Err(unexpected_shape(origin)),
    }
}

/// Human-readable warning for skipped elements, or `None` if none were skipped.
pub(crate) fn skipped_warning(skipped: usize, origin: &str) -> Option<String> {
    match skipped {
        0 => None,
        1 => Some(format!(
            "skipped 1 malformed conversation entry in {origin}"
        )),
        n => Some(format!(
            "skipped {n} malformed conversation entries in {origin}"
        )),
    }
}

/// Human-readable warning for collapsed duplicates, or `None` if there were
/// none.
///
/// Collapsing is quieter than the ambiguity error it replaces, so it must not
/// be silent: dropping a conversation without saying so is worse than either.
pub(crate) fn collapsed_warning(collapsed: usize, origin: &str) -> Option<String> {
    match collapsed {
        0 => None,
        1 => Some(format!(
            "collapsed 1 duplicate conversation id in {origin}; kept the copy with the newest update_time"
        )),
        n => Some(format!(
            "collapsed {n} duplicate conversation ids in {origin}; kept the copy with the newest update_time"
        )),
    }
}

fn unexpected_shape(origin: &str) -> Error {
    Error::UnexpectedJsonShape {
        origin: origin.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARRAY: &str = r#"[{"id": "a", "title": "First"}, {"id": "b", "title": "Second"}]"#;

    fn parse(text: &str) -> Result<Vec<Conversation>> {
        parse_conversations(text.as_bytes(), "test.json")
    }

    #[test]
    fn parses_top_level_array() {
        let conversations = parse(ARRAY).expect("array shape is valid");
        assert_eq!(conversations.len(), 2);
        assert_eq!(conversations[0].id, "a");
        assert_eq!(conversations[1].display_title(), "Second");
    }

    #[test]
    fn parses_wrapped_conversations_object() {
        let wrapped = format!(r#"{{"other": 1, "conversations": {ARRAY}}}"#);
        let conversations = parse(&wrapped).expect("wrapped shape is valid");
        assert_eq!(conversations.len(), 2);
        assert_eq!(conversations[0].id, "a");
    }

    #[test]
    fn strips_utf8_bom() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(ARRAY.as_bytes());
        let conversations =
            parse_conversations(&bytes, "bom.json").expect("a BOM must not fail the load");
        assert_eq!(conversations.len(), 2);
    }

    #[test]
    fn malformed_json_is_a_json_error() {
        let error = parse("[{").expect_err("truncated JSON must fail");
        assert!(matches!(error, Error::Json { ref origin, .. } if origin == "test.json"));
    }

    #[test]
    fn scalar_documents_are_a_shape_error() {
        for document in ["42", r#""hello""#, "null", "true"] {
            let error = parse(document).expect_err("a scalar is not an export");
            assert!(
                matches!(error, Error::UnexpectedJsonShape { .. }),
                "{document}"
            );
        }
    }

    #[test]
    fn object_without_conversations_array_is_a_shape_error() {
        for document in [r#"{"conversations": 5}"#, r#"{"items": []}"#, "{}"] {
            let error = parse(document).expect_err("no conversations array");
            assert!(
                matches!(error, Error::UnexpectedJsonShape { .. }),
                "{document}"
            );
        }
    }

    #[test]
    fn one_bad_element_does_not_kill_the_load() {
        let document = r#"[{"id": "a"}, 7, "nope", {"id": "b"}, {"mapping": 12}]"#;
        let parsed = parse_conversations_reporting(document.as_bytes(), "mixed.json")
            .expect("bad elements are skipped, not fatal");
        assert_eq!(parsed.conversations.len(), 2);
        assert_eq!(parsed.skipped, 3);
        assert_eq!(parsed.conversations[1].id, "b");
    }

    const DUPLICATES: &str = r#"[
        {"id": "dup", "title": "old", "update_time": 100.0},
        {"id": "solo", "title": "only me"},
        {"id": "dup", "title": "new", "update_time": 200.0}
    ]"#;

    #[test]
    fn duplicate_ids_collapse_to_the_freshest_copy() {
        let parsed = parse_conversations_reporting(DUPLICATES.as_bytes(), "dupes.json")
            .expect("valid document");
        assert_eq!(parsed.conversations.len(), 2);
        assert_eq!(parsed.collapsed, 1);
        assert_eq!(
            parsed.conversations[0].display_title(),
            "new",
            "the freshest copy wins"
        );
        assert_eq!(
            parsed.conversations[0].id, "dup",
            "the survivor keeps the first occurrence's position"
        );
        assert_eq!(parsed.conversations[1].id, "solo");
    }

    #[test]
    fn collapse_is_order_independent_and_ties_keep_the_first() {
        let reversed = r#"[
            {"id": "dup", "title": "new", "update_time": 200.0},
            {"id": "dup", "title": "old", "update_time": 100.0}
        ]"#;
        let parsed =
            parse_conversations_reporting(reversed.as_bytes(), "t.json").expect("valid document");
        assert_eq!(parsed.conversations[0].display_title(), "new");

        let tied = r#"[{"id": "d", "title": "first"}, {"id": "d", "title": "second"}]"#;
        let parsed =
            parse_conversations_reporting(tied.as_bytes(), "t.json").expect("valid document");
        assert_eq!(parsed.conversations.len(), 1);
        assert_eq!(
            parsed.conversations[0].display_title(),
            "first",
            "an exact tie keeps the first occurrence"
        );
    }

    #[test]
    fn collapsing_is_never_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, DUPLICATES).expect("write fixture");

        let set = JsonSource::new(&path).load().expect("load succeeds");
        assert_eq!(set.len(), 2);
        assert!(
            set.warnings
                .iter()
                .any(|w| w.contains("collapsed 1 duplicate conversation id")),
            "{:?}",
            set.warnings
        );
    }

    #[test]
    fn collapsed_warning_is_pluralized_and_optional() {
        assert_eq!(collapsed_warning(0, "x.json"), None);
        assert!(
            collapsed_warning(1, "x.json")
                .expect("singular")
                .contains("collapsed 1 duplicate conversation id in x.json")
        );
        assert!(
            collapsed_warning(3, "x.json")
                .expect("plural")
                .contains("collapsed 3 duplicate conversation ids")
        );
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        let body = format!(r#"[{{"id": "a", "title": "{}"}}]"#, "x".repeat(4096));
        std::fs::write(&path, &body).expect("write fixture");

        let source = JsonSource::with_options(
            &path,
            LoadOptions {
                max_unpacked_bytes: 64,
            },
        );
        match source.load().expect_err("over the limit") {
            Error::InputTooLarge {
                path: p,
                size,
                limit,
            } => {
                assert_eq!(p, path);
                assert_eq!(limit, 64);
                assert_eq!(size, body.len() as u64);
            }
            other => panic!("expected a size refusal, got {other:?}"),
        }
        assert!(
            source.raw_conversation("a").is_err(),
            "raw path is capped too"
        );
    }

    #[test]
    fn a_file_inside_the_limit_still_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, ARRAY).expect("write fixture");

        let set = JsonSource::with_options(
            &path,
            LoadOptions {
                max_unpacked_bytes: 4096,
            },
        )
        .load()
        .expect("inside the limit");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn new_uses_the_default_limit() {
        assert_eq!(JsonSource::new("x.json").options, LoadOptions::default());
    }

    #[test]
    fn empty_array_loads_to_nothing() {
        let parsed = parse_conversations_reporting(b"[]", "empty.json").expect("empty is valid");
        assert!(parsed.conversations.is_empty());
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn synthetic_ids_use_document_position() {
        let conversations =
            parse(r#"[{"title": "no id"}, {"title": "also none"}]"#).expect("ids are optional");
        assert_eq!(conversations[0].id, "unknown-0");
        assert_eq!(conversations[1].id, "unknown-1");
    }

    #[test]
    fn skipped_warning_is_pluralized_and_optional() {
        assert_eq!(skipped_warning(0, "x.json"), None);
        assert_eq!(
            skipped_warning(1, "x.json").as_deref(),
            Some("skipped 1 malformed conversation entry in x.json")
        );
        assert!(
            skipped_warning(4, "x.json")
                .expect("plural warning")
                .contains("4 malformed conversation entries")
        );
    }

    #[test]
    fn load_reads_from_disk_and_reports_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, ARRAY).expect("write fixture");

        let set = JsonSource::new(&path).load().expect("load succeeds");
        assert_eq!(set.len(), 2);
        assert_eq!(set.source, path.display().to_string());
        assert!(set.warnings.is_empty());
    }

    #[test]
    fn load_surfaces_skipped_entries_as_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, r#"[{"id": "a"}, 3]"#).expect("write fixture");

        let set = JsonSource::new(&path).load().expect("load succeeds");
        assert_eq!(set.len(), 1);
        assert_eq!(set.warnings.len(), 1);
        assert!(set.warnings[0].contains("skipped 1 malformed conversation entry"));
    }

    const WITH_UNKNOWN_FIELDS: &str = r#"[
        {"id": "a", "title": "First", "future_unknown_field": {"nested": [1, 2]}},
        {"conversation_id": "b", "title": "Second", "plugin_ids": ["x"]}
    ]"#;

    #[test]
    fn raw_lookup_preserves_fields_the_domain_model_drops() {
        let raw = find_raw_conversation(WITH_UNKNOWN_FIELDS.as_bytes(), "t.json", "a")
            .expect("valid document")
            .expect("id `a` is present");
        assert_eq!(raw["title"], "First");
        assert_eq!(raw["future_unknown_field"]["nested"][1], 2);

        // The same field is genuinely absent from the domain type, which is
        // the whole reason this function exists.
        let modelled =
            parse_conversations(WITH_UNKNOWN_FIELDS.as_bytes(), "t.json").expect("valid document");
        assert_eq!(modelled[0].id, "a");
    }

    #[test]
    fn raw_lookup_matches_conversation_id_too() {
        let raw = find_raw_conversation(WITH_UNKNOWN_FIELDS.as_bytes(), "t.json", "b")
            .expect("valid document")
            .expect("conversation_id `b` is present");
        assert_eq!(raw["plugin_ids"][0], "x");
    }

    #[test]
    fn raw_lookup_of_an_absent_id_is_not_an_error() {
        assert_eq!(
            find_raw_conversation(ARRAY.as_bytes(), "t.json", "nope").expect("valid document"),
            None
        );
        assert_eq!(
            find_raw_conversation(b"[]", "t.json", "a").expect("valid document"),
            None
        );
    }

    #[test]
    fn raw_lookup_works_on_the_wrapped_shape_and_ignores_junk_elements() {
        let wrapped = r#"{"conversations": [3, "x", {"id": "a", "keep": true}]}"#;
        let raw = find_raw_conversation(wrapped.as_bytes(), "t.json", "a")
            .expect("valid document")
            .expect("id `a` is present");
        assert_eq!(raw["keep"], true);
    }

    #[test]
    fn raw_lookup_prefers_the_newest_duplicate() {
        let document = r#"[
            {"id": "dup", "update_time": 100.0, "tag": "old"},
            {"id": "dup", "update_time": 200.0, "tag": "new"},
            {"id": "dup", "tag": "undated"}
        ]"#;
        let raw = find_raw_conversation(document.as_bytes(), "t.json", "dup")
            .expect("valid document")
            .expect("id `dup` is present");
        assert_eq!(raw["tag"], "new");
    }

    #[test]
    fn raw_lookup_propagates_document_errors() {
        assert!(matches!(
            find_raw_conversation(b"{oops", "t.json", "a").expect_err("invalid JSON"),
            Error::Json { .. }
        ));
        assert!(matches!(
            find_raw_conversation(b"42", "t.json", "a").expect_err("wrong shape"),
            Error::UnexpectedJsonShape { .. }
        ));
    }

    #[test]
    fn raw_lookup_reads_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, WITH_UNKNOWN_FIELDS).expect("write fixture");

        let source = JsonSource::new(&path);
        let raw = source
            .raw_conversation("a")
            .expect("lookup succeeds")
            .expect("id `a` is present");
        assert!(raw.get("future_unknown_field").is_some());
        assert_eq!(source.raw_conversation("absent").expect("lookup"), None);
    }

    #[test]
    fn load_of_a_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.json");
        let error = JsonSource::new(&path).load().expect_err("missing file");
        assert!(matches!(error, Error::Io { .. }));
    }
}
