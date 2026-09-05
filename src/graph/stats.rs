//! Counts describing a conversation and its active branch.
//!
//! Two different populations are measured here and it matters which is which:
//! the *graph* figures (`total_nodes`, `branch_points`, `broken_parents`,
//! `unreachable_nodes`, …) describe the whole `mapping`, while the message and
//! text figures describe only the branch that was actually reconstructed — the
//! text a reader will see.

use std::collections::{HashSet, VecDeque};

use serde::Serialize;

use super::branch::ConversationBranch;
use crate::model::{Conversation, Role};
use crate::text;

/// Statistics over one conversation and one reconstructed branch.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConversationStats {
    /// Every node in the mapping, on the active branch or not.
    pub total_nodes: usize,
    /// Nodes anywhere in the mapping that carry a message.
    pub nodes_with_messages: usize,
    /// Messages on the active branch.
    pub active_branch_messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub system_messages: usize,
    pub developer_messages: usize,
    pub tool_messages: usize,
    /// Messages on the branch whose role we do not model.
    pub other_messages: usize,
    /// Grapheme clusters of message text on the branch — not bytes and not code
    /// points, both of which lie about Hebrew, Arabic and emoji.
    pub characters: usize,
    /// Unicode-aware word count of message text on the branch.
    pub words: usize,
    /// Graph nodes on the branch, including message-less ones such as the
    /// synthetic root, so this is always >= `active_branch_messages`.
    pub branch_depth: usize,
    /// Nodes with more than one distinct child present in the mapping: the
    /// points where the user regenerated or edited.
    pub branch_points: usize,
    /// How many alternate paths those forks created, i.e. the sum of
    /// `children - 1` over every branch point.
    pub alternative_branches: usize,
    /// Nodes whose `parent` names an id the mapping does not contain.
    pub broken_parents: usize,
    /// Nodes not reachable from any root by following `children`.
    pub unreachable_nodes: usize,
}

impl ConversationStats {
    /// Measure `conversation` and the branch reconstructed from it.
    ///
    /// Graph figures are a single pass over the mapping plus one traversal for
    /// reachability, so the whole thing is O(V + E) — never quadratic, because
    /// a single export can hold conversations with tens of thousands of nodes.
    ///
    /// Duplicate ids in a node's `children` list count once: a repeated id is
    /// the same path listed twice, not a second alternative the user could have
    /// taken.
    pub fn compute(conversation: &Conversation, branch: &ConversationBranch) -> Self {
        let mut stats = ConversationStats {
            total_nodes: conversation.mapping.len(),
            branch_depth: branch.node_ids.len(),
            ..ConversationStats::default()
        };

        let mut distinct_children: HashSet<&str> = HashSet::new();
        for node in conversation.mapping.values() {
            if node.has_message() {
                stats.nodes_with_messages += 1;
            }
            if node
                .parent
                .as_ref()
                .is_some_and(|parent| !conversation.mapping.contains_key(parent))
            {
                stats.broken_parents += 1;
            }

            distinct_children.clear();
            distinct_children.extend(
                node.children
                    .iter()
                    .map(String::as_str)
                    .filter(|child| conversation.mapping.contains_key(*child)),
            );
            if distinct_children.len() > 1 {
                stats.branch_points += 1;
                stats.alternative_branches += distinct_children.len() - 1;
            }
        }

        stats.unreachable_nodes = stats.total_nodes - reachable_count(conversation);

        for entry in branch.messages(conversation) {
            stats.active_branch_messages += 1;
            match entry.message.role() {
                Role::User => stats.user_messages += 1,
                Role::Assistant => stats.assistant_messages += 1,
                Role::System => stats.system_messages += 1,
                Role::Developer => stats.developer_messages += 1,
                Role::Tool => stats.tool_messages += 1,
                Role::Other(_) => stats.other_messages += 1,
            }
            let plain = entry.message.content.plain_text();
            stats.characters += text::grapheme_count(&plain);
            stats.words += text::word_count(&plain);
        }

        stats
    }
}

/// Number of nodes reachable from any root by following `children`.
///
/// Iterative and visited-guarded, so cycles and diamond-shaped `children`
/// references cost one visit each rather than looping or exploding.
fn reachable_count(conversation: &Conversation) -> usize {
    let mut seen: HashSet<&str> = HashSet::with_capacity(conversation.mapping.len());
    let mut queue: VecDeque<&str> = VecDeque::new();

    for root in conversation.roots() {
        if seen.insert(root) {
            queue.push_back(root);
        }
    }

    while let Some(id) = queue.pop_front() {
        let Some(node) = conversation.node(id) else {
            continue;
        };
        for child in &node.children {
            let Some(child_node) = conversation.node(child) else {
                // `children` may name ids the export never included.
                continue;
            };
            if seen.insert(child_node.id.as_str()) {
                queue.push_back(child_node.id.as_str());
            }
        }
    }

    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::active_branch;
    use crate::model::{Author, ConversationNode, Message, MessageContent, MessageMetadata, Role};
    use std::collections::HashMap;

    fn message(role: Role, text: &str) -> Option<Message> {
        Some(Message {
            id: None,
            author: Author { role, name: None },
            create_time: None,
            content: MessageContent::Text {
                parts: vec![text.to_string()],
            },
            metadata: MessageMetadata::default(),
            recipient: None,
        })
    }

    /// One node of a test graph: `(id, parent, children, message)`.
    type NodeSpec<'a> = (&'a str, Option<&'a str>, Vec<&'a str>, Option<Message>);

    fn conversation(current_node: Option<&str>, nodes: Vec<NodeSpec<'_>>) -> Conversation {
        let mut mapping = HashMap::new();
        for (id, parent, children, message) in nodes {
            mapping.insert(
                id.to_string(),
                ConversationNode {
                    id: id.to_string(),
                    message,
                    parent: parent.map(str::to_string),
                    children: children.into_iter().map(str::to_string).collect(),
                },
            );
        }
        Conversation {
            id: "conv-1".to_string(),
            title: None,
            create_time: None,
            update_time: None,
            current_node: current_node.map(str::to_string),
            mapping,
        }
    }

    #[test]
    fn stats_on_a_known_graph() {
        let conversation = conversation(
            Some("a2"),
            vec![
                ("root", None, vec!["u1"], None),
                (
                    "u1",
                    Some("root"),
                    vec!["a1", "a1-alt"],
                    message(Role::User, "hello there"),
                ),
                ("a1", Some("u1"), vec!["u2"], message(Role::Assistant, "hi")),
                (
                    "a1-alt",
                    Some("u1"),
                    vec![],
                    message(Role::Assistant, "discarded"),
                ),
                (
                    "u2",
                    Some("a1"),
                    vec!["a2"],
                    message(Role::User, "and then"),
                ),
                ("a2", Some("u2"), vec![], message(Role::Assistant, "done")),
                (
                    "orphan",
                    Some("ghost"),
                    vec![],
                    message(Role::Tool, "tool out"),
                ),
                ("lost", Some("nowhere-either"), vec![], None),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);

        assert_eq!(stats.total_nodes, 8);
        assert_eq!(stats.nodes_with_messages, 6);
        assert_eq!(stats.branch_depth, 5); // root, u1, a1, u2, a2
        assert_eq!(stats.active_branch_messages, 4);
        assert_eq!(stats.user_messages, 2);
        assert_eq!(stats.assistant_messages, 2);
        assert_eq!(stats.tool_messages, 0); // the tool message is off-branch
        assert_eq!(stats.branch_points, 1);
        assert_eq!(stats.alternative_branches, 1);
        assert_eq!(stats.broken_parents, 2); // orphan + lost
        assert_eq!(stats.unreachable_nodes, 0); // both dangling nodes are roots
        assert_eq!(stats.words, 2 + 1 + 2 + 1);
        assert_eq!(
            stats.characters,
            "hello there".len() + 2 + "and then".len() + 4
        );
    }

    #[test]
    fn unreachable_nodes_are_counted() {
        // `x` and `y` point at each other, so neither is a root and neither is
        // reachable from one.
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec![], message(Role::User, "hi")),
                ("x", Some("y"), vec!["y"], None),
                ("y", Some("x"), vec!["x"], None),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.total_nodes, 4);
        assert_eq!(stats.unreachable_nodes, 2);
    }

    #[test]
    fn duplicate_children_are_not_alternative_branches() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1", "u1", "ghost"], None),
                ("u1", Some("root"), vec![], message(Role::User, "hi")),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.branch_points, 0);
        assert_eq!(stats.alternative_branches, 0);
    }

    #[test]
    fn character_counts_are_grapheme_based_for_hebrew_and_emoji() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1"], None),
                // 4 Hebrew letters, a space, and one family emoji that is a
                // single grapheme cluster built from many code points.
                ("u1", Some("root"), vec![], message(Role::User, "שלום 👨‍👩‍👧‍👦")),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);

        assert_eq!(stats.characters, 6);
        assert!(stats.characters < "שלום 👨‍👩‍👧‍👦".len());
        assert_eq!(stats.words, 1);
    }

    #[test]
    fn unknown_roles_land_in_other_messages() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1"], None),
                (
                    "u1",
                    Some("root"),
                    vec![],
                    message(Role::Other("oracle".to_string()), "cryptic"),
                ),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.other_messages, 1);
        assert_eq!(stats.active_branch_messages, 1);
    }

    #[test]
    fn every_role_is_counted_separately() {
        let conversation = conversation(
            Some("e"),
            vec![
                ("a", None, vec!["b"], message(Role::System, "sys")),
                ("b", Some("a"), vec!["c"], message(Role::Developer, "dev")),
                ("c", Some("b"), vec!["d"], message(Role::User, "usr")),
                ("d", Some("c"), vec!["e"], message(Role::Assistant, "ast")),
                ("e", Some("d"), vec![], message(Role::Tool, "tool")),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.system_messages, 1);
        assert_eq!(stats.developer_messages, 1);
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_messages, 1);
        assert_eq!(stats.other_messages, 0);
        assert_eq!(stats.active_branch_messages, 5);
    }
}
