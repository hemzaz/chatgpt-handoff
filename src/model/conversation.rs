//! The conversation aggregate and the export-level container.

use std::collections::HashMap;

use serde::Deserialize;

use super::node::ConversationNode;
use crate::text;

/// A single ChatGPT conversation: metadata plus the message graph.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub create_time: Option<f64>,
    pub update_time: Option<f64>,
    /// Leaf of the branch the user was last looking at. Absent in some exports.
    pub current_node: Option<String>,
    pub mapping: HashMap<String, ConversationNode>,
}

impl Conversation {
    /// Title, made safe to print: control characters and bidi overrides
    /// removed, all whitespace collapsed to single spaces, with a placeholder
    /// when there is no usable title.
    ///
    /// Whitespace collapsing is a security measure, not cosmetics. A title is
    /// attacker-supplied, and an embedded newline in a table cell fabricates a
    /// whole extra row of output — a conversation that never existed, with an
    /// id and date of the attacker's choosing. A title is a single-line field,
    /// so flattening it costs nothing and closes that hole.
    pub fn display_title(&self) -> String {
        match self.title.as_deref().map(str::trim) {
            Some(title) if !title.is_empty() => {
                let flattened = text::collapse_whitespace(title);
                let safe = text::sanitize_display(&flattened);
                if safe.trim().is_empty() {
                    "(untitled)".to_string()
                } else {
                    safe.into_owned()
                }
            }
            _ => "(untitled)".to_string(),
        }
    }

    /// Conversation id, made safe to print.
    ///
    /// Ids are every bit as attacker-controlled as titles — they are just
    /// strings in the export — so they get the same treatment. Use this
    /// anywhere an id reaches a terminal or a generated document; use the raw
    /// [`Conversation::id`] for lookups, comparisons, and JSON payloads, where
    /// fidelity matters and no terminal interprets the bytes.
    pub fn display_id(&self) -> String {
        let flattened = text::collapse_whitespace(&self.id);
        let safe = text::sanitize_display(&flattened);
        if safe.trim().is_empty() {
            "(unnamed)".to_string()
        } else {
            safe.into_owned()
        }
    }

    pub fn node(&self, id: &str) -> Option<&ConversationNode> {
        self.mapping.get(id)
    }

    /// Nodes with no parent, or whose parent is missing from the mapping.
    /// Sorted for deterministic traversal.
    pub fn roots(&self) -> Vec<&str> {
        let mut roots: Vec<&str> = self
            .mapping
            .iter()
            .filter(|(_, node)| match &node.parent {
                None => true,
                Some(parent) => !self.mapping.contains_key(parent),
            })
            .map(|(id, _)| id.as_str())
            .collect();
        roots.sort_unstable();
        roots
    }
}

/// Wire format for one conversation. Kept separate from [`Conversation`] so the
/// domain type stays clean and the tolerant-parsing rules live in one place.
#[derive(Debug, Deserialize)]
pub(crate) struct RawConversation {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    create_time: Option<f64>,
    #[serde(default)]
    update_time: Option<f64>,
    #[serde(default)]
    current_node: Option<String>,
    #[serde(default)]
    mapping: HashMap<String, ConversationNode>,
}

impl RawConversation {
    /// Normalize into the domain type.
    ///
    /// `index` supplies a synthetic id when the export has neither `id` nor
    /// `conversation_id`; dropping such a conversation would be worse than
    /// giving it a stable positional handle.
    pub(crate) fn into_conversation(mut self, index: usize) -> Conversation {
        let id = self
            .id
            .take()
            .or_else(|| self.conversation_id.take())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("unknown-{index}"));

        // The mapping key is authoritative: node payloads sometimes carry a
        // stale or absent `id`, and every traversal indexes by key.
        for (key, node) in self.mapping.iter_mut() {
            node.id = key.clone();
        }

        Conversation {
            id,
            title: self.title,
            create_time: self.create_time,
            update_time: self.update_time,
            current_node: self.current_node.filter(|n| !n.is_empty()),
            mapping: self.mapping,
        }
    }
}

/// Everything loaded from one export source.
#[derive(Debug, Clone)]
pub struct ConversationSet {
    pub conversations: Vec<Conversation>,
    /// Human-readable description of where this came from, e.g.
    /// `conversations.json` or `export.zip!conversations.json`.
    pub source: String,
    /// Non-fatal problems encountered while loading.
    pub warnings: Vec<String>,
}

impl ConversationSet {
    pub fn find_by_id(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn len(&self) -> usize {
        self.conversations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.conversations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(value: serde_json::Value) -> Conversation {
        serde_json::from_value::<RawConversation>(value)
            .expect("tolerant parse")
            .into_conversation(0)
    }

    #[test]
    fn falls_back_to_conversation_id_then_synthetic_id() {
        assert_eq!(raw(json!({"conversation_id": "abc"})).id, "abc");
        assert_eq!(
            raw(json!({"id": "xyz", "conversation_id": "abc"})).id,
            "xyz"
        );
        assert_eq!(raw(json!({})).id, "unknown-0");
        assert_eq!(raw(json!({"id": ""})).id, "unknown-0");
    }

    #[test]
    fn mapping_key_overrides_stale_node_id() {
        let conversation = raw(json!({
            "id": "c1",
            "mapping": {"real-key": {"id": "stale-id", "children": []}}
        }));
        assert_eq!(
            conversation.node("real-key").map(|n| n.id.as_str()),
            Some("real-key")
        );
    }

    #[test]
    fn display_title_is_sanitized_and_has_a_fallback() {
        assert_eq!(raw(json!({"title": "  "})).display_title(), "(untitled)");
        assert_eq!(raw(json!({})).display_title(), "(untitled)");
        let hostile = raw(json!({"title": "ok\u{202e}evil"}));
        assert!(!hostile.display_title().contains('\u{202e}'));
    }

    #[test]
    fn display_fields_cannot_fabricate_extra_output_rows() {
        let escape = char::from_u32(27).unwrap_or('?');
        let hostile = raw(json!({
            "id": format!("id{escape}[31mRED\nFAKE-ROW  1999-01-01  Injected"),
            "title": "benign\nFAKE-ROW  1999-01-01  Injected via title",
        }));
        for rendered in [hostile.display_title(), hostile.display_id()] {
            assert!(!rendered.contains('\n'), "newline survived: {rendered:?}");
            assert!(!rendered.contains(escape), "escape survived: {rendered:?}");
        }
        // The raw id is preserved for lookups and JSON.
        assert!(hostile.id.contains('\n'));
    }

    #[test]
    fn display_fields_fall_back_when_sanitizing_empties_them() {
        let only_controls = raw(json!({"id": "\u{202e}\u{202d}", "title": "\u{202e}"}));
        assert_eq!(only_controls.display_id(), "(unnamed)");
        assert_eq!(only_controls.display_title(), "(untitled)");
    }

    #[test]
    fn roots_include_nodes_with_dangling_parents() {
        let conversation = raw(json!({
            "id": "c1",
            "mapping": {
                "root": {"parent": null, "children": ["a"]},
                "a": {"parent": "root", "children": []},
                "orphan": {"parent": "ghost", "children": []}
            }
        }));
        assert_eq!(conversation.roots(), vec!["orphan", "root"]);
    }

    #[test]
    fn empty_current_node_is_treated_as_absent() {
        assert_eq!(raw(json!({"current_node": ""})).current_node, None);
    }
}
