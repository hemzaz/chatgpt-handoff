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
    /// Title with control characters and bidi overrides removed, falling back
    /// to a placeholder. Always safe to print.
    pub fn display_title(&self) -> String {
        match self.title.as_deref().map(str::trim) {
            Some(title) if !title.is_empty() => text::sanitize_display(title).into_owned(),
            _ => "(untitled)".to_string(),
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
