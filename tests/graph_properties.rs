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

fn any_id() -> impl Strategy<Value = String> {
    prop::sample::select(ALPHABET.as_slice()).prop_map(str::to_string)
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
    (
        prop::collection::hash_map(any_id(), any_node(), 0..7),
        prop::option::of(any_id()),
    )
        .prop_map(|(nodes, current_node)| {
            let mapping: HashMap<String, ConversationNode> = nodes
                .into_iter()
                .map(|(id, (parent, children, message))| {
                    let node = ConversationNode {
                        // The loader guarantees `id` equals the mapping key.
                        id: id.clone(),
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

    /// `HashMap` iteration order is not stable across runs, so every choice the
    /// traversal makes among alternatives must be explicitly ordered.
    #[test]
    fn resolution_is_deterministic(conversation in any_conversation()) {
        let first = active_branch(&conversation);
        let second = active_branch(&conversation);
        match (first, second) {
            (Ok(a), Ok(b)) => {
                prop_assert_eq!(a.node_ids, b.node_ids);
                prop_assert_eq!(a.warnings, b.warnings);
                prop_assert_eq!(a.strategy, b.strategy);
            }
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            _ => prop_assert!(false, "resolution flipped between Ok and Err"),
        }
    }

    /// Re-running on a structurally identical conversation — a fresh `HashMap`
    /// built in a different insertion order — must give the same branch.
    #[test]
    fn resolution_ignores_hashmap_insertion_order(conversation in any_conversation()) {
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

    /// Stats must be total functions over whatever the branch turned out to be.
    #[test]
    fn stats_never_panic_and_stay_within_the_graph(conversation in any_conversation()) {
        let Ok(branch) = active_branch(&conversation) else {
            return Ok(());
        };
        let stats = ConversationStats::compute(&conversation, &branch);

        prop_assert_eq!(stats.total_nodes, conversation.mapping.len());
        prop_assert!(stats.active_branch_messages <= stats.total_nodes);
        prop_assert!(stats.active_branch_messages <= stats.nodes_with_messages);
        prop_assert!(stats.branch_depth <= stats.total_nodes);
        prop_assert!(stats.active_branch_messages <= stats.branch_depth);
        prop_assert!(stats.unreachable_nodes <= stats.total_nodes);
        prop_assert!(stats.broken_parents <= stats.total_nodes);
        prop_assert_eq!(
            stats.user_messages
                + stats.assistant_messages
                + stats.system_messages
                + stats.developer_messages
                + stats.tool_messages
                + stats.other_messages,
            stats.active_branch_messages
        );
        prop_assert!(stats.words <= stats.characters);
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
