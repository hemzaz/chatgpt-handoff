//! Nodes of the ChatGPT conversation graph.

use serde::Deserialize;

use super::message::Message;

/// One vertex of a conversation's `mapping` graph.
///
/// A node may legitimately have no message (the synthetic root that ChatGPT
/// puts at the top of every conversation is exactly this), so `message` is an
/// `Option` and callers must never assume otherwise.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationNode {
    /// Present in the payload, but we always trust the mapping key instead —
    /// see [`crate::model::conversation::Conversation`] construction.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub children: Vec<String>,
}

impl ConversationNode {
    pub fn has_message(&self) -> bool {
        self.message.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_without_message_parses() {
        let node: ConversationNode =
            serde_json::from_value(json!({"id": "root", "children": ["a"]}))
                .expect("a message-less root node is valid");
        assert!(!node.has_message());
        assert_eq!(node.parent, None);
        assert_eq!(node.children, vec!["a"]);
    }

    #[test]
    fn node_with_null_message_parses() {
        let node: ConversationNode =
            serde_json::from_value(json!({"id": "root", "message": null, "parent": null}))
                .expect("explicit nulls are valid");
        assert!(!node.has_message());
    }

    #[test]
    fn unknown_node_fields_are_ignored() {
        let node: ConversationNode =
            serde_json::from_value(json!({"id": "a", "future_field": [1, 2, 3]}))
                .expect("unknown fields must not fail");
        assert_eq!(node.id, "a");
    }
}
