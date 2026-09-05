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

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::ExportSource;
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

/// A `.json` export file on disk.
#[derive(Debug, Clone)]
pub struct JsonSource {
    /// Path to the export file.
    pub path: PathBuf,
}

impl JsonSource {
    /// Point at a `.json` export file. No I/O happens until [`ExportSource::load`].
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl ExportSource for JsonSource {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn load(&self) -> Result<ConversationSet> {
        let bytes = std::fs::read(&self.path).map_err(|source| Error::io(&self.path, source))?;
        let origin = self.path.display().to_string();
        let parsed = parse_conversations_reporting(&bytes, &origin)?;
        Ok(ConversationSet {
            conversations: parsed.conversations,
            warnings: skipped_warning(parsed.skipped, &origin)
                .into_iter()
                .collect(),
            source: origin,
        })
    }

    fn raw_conversation(&self, conversation_id: &str) -> Result<Option<Value>> {
        let bytes = std::fs::read(&self.path).map_err(|source| Error::io(&self.path, source))?;
        find_raw_conversation(&bytes, &self.path.display().to_string(), conversation_id)
    }
}

/// Result of parsing one export document.
#[derive(Debug, Clone)]
pub struct ParsedConversations {
    /// Conversations that parsed successfully, in document order.
    pub conversations: Vec<Conversation>,
    /// Array elements that were not a usable conversation object and were
    /// therefore skipped.
    pub skipped: usize,
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

    let mut conversations = Vec::with_capacity(items.len().min(MAX_PREALLOCATED_CONVERSATIONS));
    let mut skipped = 0usize;
    for (index, item) in items.into_iter().enumerate() {
        // A non-object element, or an object whose typed fields have the wrong
        // shape, costs us that one conversation and nothing else. The index is
        // the document position, so synthetic ids stay stable across loads.
        match serde_json::from_value::<RawConversation>(item) {
            Ok(raw) => conversations.push(raw.into_conversation(index)),
            Err(_) => skipped = skipped.saturating_add(1),
        }
    }

    Ok(ParsedConversations {
        conversations,
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
