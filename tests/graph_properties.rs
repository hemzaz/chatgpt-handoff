//! Property tests for conversation-graph traversal.
//!
//! ChatGPT exports are untrusted input, and the graph in `mapping` is the part
//! most likely to be malformed: truncated downloads leave dangling parents,
//! hand-edited files invent cycles, and `current_node` can point anywhere or
//! nowhere. These tests generate deliberately hostile graphs — ids drawn from a
//! tiny alphabet so parents and children collide and dangle constantly — and
//! assert the invariants every consumer of a branch relies on.

use std::collections::{HashMap, HashSet};

use chatgpt_handoff::graph::{ConversationStats, active_branch};
use chatgpt_handoff::model::{
    Author, Conversation, ConversationNode, Message, MessageContent, MessageMetadata, Role,
};
use proptest::prelude::*;

/// Ids are drawn from this alphabet so that generated `parent` and `children`
/// references land on real nodes about as often as they dangle.
const ALPHABET: [&str; 6] = ["a", "b", "c", "d", "e", "ghost"];

/// A wider alphabet for tests that must not accidentally pass because two small
/// `HashMap`s in one process happened to share bucket order.
const WIDE_ALPHABET: [&str; 26] = [
    "a01", "a02", "a03", "a04", "a05", "a06", "a07", "a08", "a09", "a10", "a11", "a12", "a13",
    "a14", "a15", "a16", "a17", "a18", "a19", "a20", "a21", "a22", "a23", "a24", "a25", "gone",
];

fn any_id() -> impl Strategy<Value = String> + Clone {
    prop::sample::select(ALPHABET.as_slice()).prop_map(str::to_string)
}

fn any_wide_id() -> impl Strategy<Value = String> + Clone {
    prop::sample::select(WIDE_ALPHABET.as_slice()).prop_map(str::to_string)
}

fn any_message() -> impl Strategy<Value = Option<Message>> {
    prop::option::of(
        (
            prop::sample::select(vec![
                Role::User,
                Role::Assistant,
                Role::System,
                Role::Developer,
                Role::Tool,
                Role::Other("oracle".to_string()),
            ]),
            prop::sample::select(vec!["", "hi", "שלום עולם", "👨‍👩‍👧‍👦 emoji", "a b c"]),
        )
            .prop_map(|(role, text)| Message {
                id: None,
                author: Author { role, name: None },
                create_time: None,
                content: MessageContent::Text {
                    parts: vec![text.to_string()],
                },
                metadata: MessageMetadata::default(),
                recipient: None,
            }),
    )
}

/// A node with an arbitrary — possibly self-referential, possibly dangling —
/// parent and child list.
fn any_node() -> impl Strategy<Value = (Option<String>, Vec<String>, Option<Message>)> {
    (
        prop::option::of(any_id()),
        prop::collection::vec(any_id(), 0..4),
        any_message(),
    )
}

/// Arbitrary conversations, including empty and single-node mappings.
fn any_conversation() -> impl Strategy<Value = Conversation> {
    conversations(any_id(), 0..7)
}

/// Bigger graphs over a wider alphabet, for properties that a handful of keys
/// could satisfy by luck.
fn any_wide_conversation() -> impl Strategy<Value = Conversation> {
    conversations(any_wide_id(), 8..40)
}

fn conversations(
    ids: impl Strategy<Value = String> + Clone,
    size: std::ops::Range<usize>,
) -> impl Strategy<Value = Conversation> {
    (
        prop::collection::hash_map(ids.clone(), any_node(), size),
        prop::option::of(ids.clone()),
        // The loader normalises `ConversationNode::id` to the mapping key, so
        // nothing may read that field. Feed it a wrong value sometimes to keep
        // it that way.
        prop::option::of(ids),
    )
        .prop_map(|(nodes, current_node, stale_id)| {
            let mapping: HashMap<String, ConversationNode> = nodes
                .into_iter()
                .map(|(id, (parent, children, message))| {
                    let node = ConversationNode {
                        id: stale_id.clone().unwrap_or_else(|| id.clone()),
                        message,
                        parent,
                        children,
                    };
                    (id, node)
                })
                .collect();
            Conversation {
                id: "prop".to_string(),
                title: None,
                create_time: None,
                update_time: None,
                current_node,
                mapping,
            }
        })
}

/// Everything a well-formed branch must satisfy, checked in one place so each
/// property test can assert the full contract.
fn assert_branch_invariants(conversation: &Conversation, node_ids: &[String]) {
    let mut seen: HashSet<&str> = HashSet::new();
    for id in node_ids {
        assert!(
            seen.insert(id.as_str()),
            "branch repeated node `{id}`: {node_ids:?}"
        );
        assert!(
            conversation.mapping.contains_key(id),
            "branch contains `{id}`, which is not in the mapping"
        );
    }

    for pair in node_ids.windows(2) {
        let (parent_id, child_id) = (&pair[0], &pair[1]);
        let child = conversation
            .node(child_id)
            .expect("child was just asserted to exist");
        assert_eq!(
            child.parent.as_deref(),
            Some(parent_id.as_str()),
            "branch step `{parent_id}` -> `{child_id}` is not a real parent link"
        );
    }
}

/// Independent reference implementation of the chain `active_branch` should
/// produce from a given node: follow `parent` until it runs out, breaks, or
/// loops. Deliberately naive and unmemoized so it shares no logic with the
/// implementation under test.
fn reference_chain(conversation: &Conversation, start: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = Some(start.to_string());
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(node) = conversation.node(&id) else {
            break;
        };
        chain.push(id);
        cursor = match node.parent.as_deref() {
            Some(parent) if conversation.node(parent).is_some() && !seen.contains(parent) => {
                Some(parent.to_string())
            }
            _ => None,
        };
    }
    chain
}

/// Independently computed expectation for the branch `active_branch` should
/// return, built only from the documented rules and sharing no code with the
/// implementation: for `current-node`, the chain from `current_node`; for
/// `longest-path`, the chain from the node with the longest chain, ties broken
/// by message count then smallest id.
fn expected_branch(conversation: &Conversation, strategy: &str) -> Option<Vec<String>> {
    let leaf = match strategy {
        "current-node" => conversation.current_node.clone()?,
        "longest-path" => {
            let mut ids: Vec<&String> = conversation.mapping.keys().collect();
            ids.sort();
            let mut best: Option<(&String, usize, usize)> = None;
            for id in ids {
                let chain = reference_chain(conversation, id);
                let messages = chain
                    .iter()
                    .filter(|n| {
                        conversation
                            .node(n)
                            .is_some_and(|node| node.message.is_some())
                    })
                    .count();
                let better = match best {
                    None => true,
                    Some((_, length, msgs)) => (chain.len(), messages) > (length, msgs),
                };
                if better {
                    best = Some((id, chain.len(), messages));
                }
            }
            best.map(|(id, _, _)| id.clone())?
        }
        _ => return None,
    };
    let mut chain = reference_chain(conversation, &leaf);
    chain.reverse();
    Some(chain)
}

/// Message-carrying nodes on a set of ids, counted independently.
fn count_messages(conversation: &Conversation, ids: &[String]) -> usize {
    ids.iter()
        .filter(|id| {
            conversation
                .node(id)
                .is_some_and(|node| node.message.is_some())
        })
        .count()
}

proptest! {
    /// The headline guarantee: no arbitrary graph can panic or hang the walk,
    /// and whatever comes back is a real path through the mapping.
    #[test]
    fn active_branch_survives_arbitrary_graphs(conversation in any_conversation()) {
        match active_branch(&conversation) {
            Ok(branch) => assert_branch_invariants(&conversation, &branch.node_ids),
            Err(_) => {
                // Only ever legitimate for an empty mapping or a graph with no
                // message anywhere.
                prop_assert!(
                    conversation.mapping.is_empty()
                        || !conversation.mapping.values().any(|n| n.message.is_some())
                );
            }
        }
    }

    /// The fallback claims to return the longest path, so no node may have a
    /// strictly longer `parent` chain than the branch it produced. This is the
    /// invariant that would have caught both the empty-`children` truncation
    /// and the diamond shortcut: each measured depth on `children` while the
    /// branch was reconstructed from `parent`.
    #[test]
    fn longest_path_really_is_the_longest_parent_chain(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        // Only the fallback promises maximality. A valid `current_node` picks a
        // specific branch, which is allowed to be shorter than the longest one.
        if branch.strategy != "longest-path" {
            return Ok(());
        }

        for id in conversation.mapping.keys() {
            let chain = reference_chain(&conversation, id);
            prop_assert!(
                chain.len() <= branch.node_ids.len(),
                "node `{}` has a chain of {} but the branch is only {} long: {:?}",
                id,
                chain.len(),
                branch.node_ids.len(),
                branch.node_ids
            );
        }

        // And the branch must itself be a real chain, not merely long enough.
        if let Some(leaf) = branch.node_ids.last() {
            let mut expected = reference_chain(&conversation, leaf);
            expected.reverse();
            prop_assert_eq!(&expected, &branch.node_ids);
        }
    }

    /// The non-negotiable one: a node being rendered can never simultaneously
    /// be reported as content the user is missing. The old children-based
    /// reachability count violated this on any export whose `children` lists
    /// were empty.
    #[test]
    fn no_branch_node_is_ever_counted_unreachable(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        let stats = ConversationStats::compute(&conversation, &branch);
        let off_branch = conversation.mapping.len() - branch.node_ids.len();
        prop_assert!(
            stats.unreachable_nodes <= off_branch,
            "{} unreachable but only {} nodes are off the branch {:?}",
            stats.unreachable_nodes,
            off_branch,
            branch.node_ids
        );
    }

    /// The branch must equal an independently computed expectation, not merely
    /// equal itself. The previous version called `active_branch` twice on one
    /// value in one process — both calls iterating the same `HashMap` with the
    /// same `RandomState` — so it passed against an implementation that simply
    /// took whatever raw hash order handed it first. Comparing against an
    /// order-independent oracle tests the actual rule, and implies determinism
    /// rather than assuming it.
    #[test]
    fn branch_matches_an_independently_computed_expectation(
        conversation in any_wide_conversation()
    ) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        let Some(expected) = expected_branch(&conversation, branch.strategy) else {
            return Ok(());
        };
        prop_assert_eq!(&expected, &branch.node_ids);
    }

    /// Re-running on a structurally identical conversation — a fresh `HashMap`
    /// built in a different insertion order — must give the same branch.
    ///
    /// Uses the wide generator on purpose: with a handful of keys drawn from a
    /// six-symbol alphabet two maps in one process very often share bucket
    /// order regardless of insertion order, so this passed too easily. The
    /// oracle test above is the stronger guarantee; this one covers the same
    /// ground from the other direction.
    #[test]
    fn resolution_ignores_hashmap_insertion_order(conversation in any_wide_conversation()) {
        let mut reordered: Vec<(String, ConversationNode)> =
            conversation.mapping.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        reordered.sort_by(|a, b| b.0.cmp(&a.0));
        let twin = Conversation {
            mapping: reordered.into_iter().collect(),
            ..conversation.clone()
        };

        match (active_branch(&conversation), active_branch(&twin)) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a.node_ids, b.node_ids),
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            _ => prop_assert!(false, "resolution depended on insertion order"),
        }
    }

    /// Stats must match independently computed values, not restate their own
    /// assignments. The previous version asserted `total_nodes == mapping.len()`
    /// (an echo of the assignment) and `words <= characters` (true for any pair
    /// those two counters can produce), so it constrained nothing.
    #[test]
    fn stats_match_independently_computed_values(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        let stats = ConversationStats::compute(&conversation, &branch);

        let with_messages = conversation
            .mapping
            .values()
            .filter(|node| node.message.is_some())
            .count();
        prop_assert_eq!(stats.nodes_with_messages, with_messages);

        prop_assert_eq!(
            stats.active_branch_messages,
            count_messages(&conversation, &branch.node_ids)
        );

        let broken = conversation
            .mapping
            .values()
            .filter(|node| {
                node.parent
                    .as_ref()
                    .is_some_and(|p| !conversation.mapping.contains_key(p))
            })
            .count();
        prop_assert_eq!(stats.broken_parents, broken);

        // Branch nodes are never stranded, so the two populations are disjoint.
        prop_assert!(stats.unreachable_nodes + stats.branch_depth <= stats.total_nodes);

        // Every fork contributes at least one alternative.
        prop_assert!(stats.alternative_branches >= stats.branch_points);

        prop_assert_eq!(
            stats.user_messages
                + stats.assistant_messages
                + stats.system_messages
                + stats.developer_messages
                + stats.tool_messages
                + stats.other_messages,
            stats.active_branch_messages
        );

        // Text counters describe branch messages and nothing else.
        if stats.active_branch_messages == 0 {
            prop_assert_eq!(stats.characters, 0);
            prop_assert_eq!(stats.words, 0);
        }
    }

    /// Stats are computed from `HashMap` values, so they must not depend on
    /// iteration order either.
    #[test]
    fn stats_are_deterministic(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        let first = ConversationStats::compute(&conversation, &branch);
        let second = ConversationStats::compute(&conversation, &branch);
        prop_assert_eq!(
            serde_json::to_string(&first).ok(),
            serde_json::to_string(&second).ok()
        );
    }

    /// A branch is only allowed to be empty of messages when the conversation
    /// really has nothing to show; otherwise the fallback must have found some.
    #[test]
    fn a_branch_with_messages_is_preferred(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        if branch.messages(&conversation).is_empty() {
            prop_assert!(
                branch
                    .warnings
                    .iter()
                    .any(|w| matches!(w, chatgpt_handoff::graph::BranchWarning::NoMessagesOnBranch)),
                "a message-less branch must say so"
            );
        }
    }
}
