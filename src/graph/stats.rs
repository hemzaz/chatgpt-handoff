//! Counts describing a conversation and its active branch.
//!
//! Two different populations are measured here and it matters which is which:
//! the *graph* figures (`total_nodes`, `branch_points`, `broken_parents`,
//! `unreachable_nodes`, …) describe the whole `mapping`, while the message and
//! text figures describe only the branch that was actually reconstructed — the
//! text a reader will see.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::branch::{self, ConversationBranch};
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
    /// Nodes with more than one child: the points where the user regenerated or
    /// edited.
    ///
    /// Derived by grouping nodes on their `parent`, never by reading
    /// `children`. Reading `children` reported "0 branch points" for exports
    /// that genuinely forked but ship empty `children` lists — a false claim
    /// about the shape of the conversation, which is the question `inspect`
    /// exists to answer.
    ///
    /// `parent` alone, not a union of the two edges, for the same reason the
    /// rest of this module uses it: when the two disagree, `parent` is what
    /// branches are reconstructed from, so it is what the reported shape must
    /// describe. A union counted a fork for the `root.children = [a, e]` export
    /// whose reconstructed branch is strictly linear — a fork the reader would
    /// never see. Grouping on a single-valued edge also makes double-counting
    /// unrepresentable: each node contributes to exactly one parent's tally,
    /// so a duplicate id in some `children` list cannot inflate anything.
    pub branch_points: usize,
    /// How many alternate paths those forks created, i.e. the sum of
    /// `children - 1` over every branch point.
    pub alternative_branches: usize,
    /// Nodes whose `parent` names an id the mapping does not contain.
    pub broken_parents: usize,
    /// Nodes stranded outside the branch: their `parent` chain never reaches a
    /// genuine root, so they hang off no beginning the conversation has.
    ///
    /// Measured on `parent`, the edge branches are reconstructed from. Judging
    /// on `children` called eight nodes stranded in an export whose `parent`
    /// chain was completely intact, and simultaneously missed a real orphan
    /// whose parent id was absent from the mapping — wrong in both directions
    /// at once.
    ///
    /// **Nodes on the returned branch are excluded, deliberately.** Read
    /// literally, "never reaches a genuine root" would count the branch's own
    /// nodes whenever the branch itself is damaged — a branch rooted at a
    /// broken parent, or one made entirely of a cycle — and reporting content
    /// the user is being shown as content they are missing is incoherent. This
    /// counter sits under `Damage` and means "nodes stranded outside what you
    /// are being shown"; damage *within* the branch is already carried by
    /// `broken_parents` and the `BrokenParent` / `CycleDetected` warnings, so
    /// the exclusion de-duplicates rather than conceals. Removing it will
    /// resurrect the contradiction — it is not an oversight.
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
        }

        for children in children_by_parent(conversation).values() {
            if children.len() > 1 {
                stats.branch_points += 1;
                stats.alternative_branches += children.len() - 1;
            }
        }

        let on_branch: HashSet<&str> = branch.node_ids.iter().map(String::as_str).collect();
        stats.unreachable_nodes = branch::ungrounded_node_ids(conversation)
            .into_iter()
            .filter(|id| !on_branch.contains(id))
            .count();

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

/// Each node's children, derived by grouping every node on its `parent`.
///
/// Never reads `children`. `parent` is single-valued, so every node lands in
/// exactly one bucket: a fork cannot be counted twice, and a duplicated id in
/// some `children` list cannot be counted at all. Parents absent from the
/// mapping are dropped — those nodes are orphans, not children of anything.
/// O(V).
fn children_by_parent(conversation: &Conversation) -> HashMap<&str, HashSet<&str>> {
    let mut children_of: HashMap<&str, HashSet<&str>> = HashMap::new();

    for (id, node) in &conversation.mapping {
        // Not a let-chain: Cargo.toml declares rust-version 1.85 and those only
        // stabilised in 1.88.
        if let Some(parent) = node
            .parent
            .as_deref()
            .filter(|parent| conversation.node(parent).is_some())
        {
            children_of.entry(parent).or_default().insert(id.as_str());
        }
    }

    children_of
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
        // `orphan` and `lost` hang off ids the export never included, so neither
        // reaches a genuine root: they really are stranded. The old
        // children-based count called them roots and reported 0.
        assert_eq!(stats.unreachable_nodes, 2);
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

    /// Case A: intact `parent` chain, every `children` empty. The old
    /// children-based traversal reported 8 of 9 nodes unreachable while all 9
    /// were on the active branch — a node cannot be both.
    #[test]
    fn intact_parent_chain_with_empty_children_has_no_unreachable_nodes() {
        let mut nodes: Vec<NodeSpec<'_>> = vec![("root", None, vec![], None)];
        let ids: Vec<String> = (1..=8).map(|i| format!("n{i}")).collect();
        let parents: Vec<String> = std::iter::once("root".to_string())
            .chain(ids.iter().take(7).cloned())
            .collect();
        for (index, id) in ids.iter().enumerate() {
            nodes.push((
                id.as_str(),
                Some(parents[index].as_str()),
                vec![],
                message(Role::User, "hi"),
            ));
        }
        let conversation = conversation(None, nodes);

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.branch_depth, 9);
        assert_eq!(stats.total_nodes, 9);
        assert_eq!(stats.unreachable_nodes, 0);
    }

    /// Case B: `ghost` hangs off an id the export never included and appears in
    /// nobody's `children`. The old traversal treated it as a root and reported
    /// zero unreachable nodes — the textbook orphan, missed.
    #[test]
    fn an_orphan_with_a_missing_parent_is_counted_unreachable() {
        let conversation = conversation(
            Some("n1"),
            vec![
                ("root", None, vec!["n1"], None),
                ("n1", Some("root"), vec![], message(Role::User, "hi")),
                (
                    "ghost",
                    Some("missing-node"),
                    vec![],
                    message(Role::User, "stranded"),
                ),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.branch_depth, 2);
        assert_eq!(stats.broken_parents, 1);
        assert_eq!(stats.unreachable_nodes, 1);
    }

    /// The invariant behind both cases: whatever the graph looks like, a node
    /// being rendered can never also be reported as content you are missing.
    #[test]
    fn no_node_on_the_branch_is_ever_counted_unreachable() {
        let graphs = vec![
            // A branch whose own root has a missing parent.
            conversation(
                Some("a1"),
                vec![
                    (
                        "u1",
                        Some("ghost-root"),
                        vec!["a1"],
                        message(Role::User, "q"),
                    ),
                    ("a1", Some("u1"), vec![], message(Role::Assistant, "a")),
                ],
            ),
            // A branch made entirely of a cycle.
            conversation(
                Some("c"),
                vec![
                    ("a", Some("c"), vec!["b"], message(Role::User, "a")),
                    ("b", Some("a"), vec!["c"], message(Role::Assistant, "b")),
                    ("c", Some("b"), vec!["a"], message(Role::User, "c")),
                ],
            ),
            // A healthy conversation with an orphan alongside it.
            conversation(
                Some("n1"),
                vec![
                    ("root", None, vec!["n1"], None),
                    ("n1", Some("root"), vec![], message(Role::User, "hi")),
                    ("ghost", Some("nowhere"), vec![], message(Role::User, "x")),
                ],
            ),
        ];

        for conversation in graphs {
            let branch = active_branch(&conversation).expect("resolves");
            let stats = ConversationStats::compute(&conversation, &branch);
            let off_branch = conversation
                .mapping
                .len()
                .saturating_sub(branch.node_ids.len());
            assert!(
                stats.unreachable_nodes <= off_branch,
                "counted {} unreachable but only {off_branch} nodes are off the branch",
                stats.unreachable_nodes
            );
        }
    }

    /// A regeneration recorded only through `parent` — every `children` list
    /// empty — is still a fork. The children-only count called this shape
    /// "0 branch points", a false claim about the conversation's shape.
    #[test]
    fn a_fork_recorded_only_through_parent_is_still_a_branch_point() {
        let conversation = conversation(
            Some("gen2"),
            vec![
                ("root", None, vec![], None),
                ("u1", Some("root"), vec![], message(Role::User, "q")),
                (
                    "gen1",
                    Some("u1"),
                    vec![],
                    message(Role::Assistant, "first"),
                ),
                (
                    "gen2",
                    Some("u1"),
                    vec![],
                    message(Role::Assistant, "second"),
                ),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.branch_points, 1);
        assert_eq!(stats.alternative_branches, 1);
        assert_eq!(stats.unreachable_nodes, 0);
    }

    /// A fork both edges record is one fork. Merging the two must not
    /// double-count it.
    #[test]
    fn a_fork_recorded_by_both_edges_counts_once() {
        let conversation = conversation(
            Some("gen2"),
            vec![
                ("root", None, vec!["u1"], None),
                (
                    "u1",
                    Some("root"),
                    vec!["gen1", "gen2"],
                    message(Role::User, "q"),
                ),
                (
                    "gen1",
                    Some("u1"),
                    vec![],
                    message(Role::Assistant, "first"),
                ),
                (
                    "gen2",
                    Some("u1"),
                    vec![],
                    message(Role::Assistant, "second"),
                ),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.branch_points, 1);
        assert_eq!(stats.alternative_branches, 1);
    }

    /// A node listed in someone's `children` but whose own `parent` is missing
    /// is still stranded. `children` does not rescue it: `parent` is the edge
    /// branches are reconstructed from, so a node with no usable `parent` is
    /// on no branch a reader will ever see.
    #[test]
    fn a_children_reference_does_not_rescue_a_node_with_a_missing_parent() {
        let conversation = conversation(
            Some("n1"),
            vec![
                ("root", None, vec!["n1", "odd"], None),
                ("n1", Some("root"), vec![], message(Role::User, "hi")),
                ("odd", Some("missing"), vec![], message(Role::User, "odd")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(stats.broken_parents, 1);
        assert_eq!(stats.unreachable_nodes, 1);
        // And `root` is not a branch point: only `n1` really parents to it.
        assert_eq!(stats.branch_points, 0);
    }

    /// The two edges disagreeing must not invent a fork. Here `root` lists `e`
    /// as a child, but `e`'s parent is `d`, so the reconstructed branch is
    /// strictly linear — a union of the edges reported "1 branch point" for a
    /// conversation that never forked.
    #[test]
    fn a_children_only_edge_does_not_invent_a_branch_point() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["a", "e"], None),
                ("a", Some("root"), vec!["b"], message(Role::User, "one")),
                ("b", Some("a"), vec!["c"], message(Role::Assistant, "two")),
                ("c", Some("b"), vec!["d"], message(Role::User, "three")),
                ("d", Some("c"), vec!["e"], message(Role::Assistant, "four")),
                ("e", Some("d"), vec![], message(Role::User, "five")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        let stats = ConversationStats::compute(&conversation, &branch);
        assert_eq!(branch.node_ids, ["root", "a", "b", "c", "d", "e"]);
        assert_eq!(stats.branch_points, 0);
        assert_eq!(stats.alternative_branches, 0);
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
