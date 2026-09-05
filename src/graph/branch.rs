//! Active-branch reconstruction.
//!
//! A ChatGPT conversation is not a list of messages, it is a *tree*: every
//! regeneration, every edited prompt, forks the graph. The export hands us the
//! whole tree in `mapping` plus a single `current_node` pointer at the leaf the
//! user was last looking at. The only faithful way to recover "the conversation"
//! is therefore to start at that leaf and follow `parent` links up to the root —
//! never to iterate `mapping`, which would splice abandoned branches into the
//! transcript and produce a document the user never saw.
//!
//! Everything here is iterative and cycle-guarded. Exports are untrusted input:
//! a hostile or merely corrupt file must not blow the stack or spin forever.

use std::collections::{HashMap, HashSet};

use crate::error::GraphError;
use crate::model::{Conversation, Message};

/// Recoverable damage found while reconstructing a branch.
///
/// These are warnings rather than errors on purpose: an incomplete export is
/// still worth reading, and telling the user *what* was wrong beats refusing to
/// produce output. Fatal damage — no nodes at all, or no messages anywhere —
/// is a [`GraphError`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchWarning {
    /// The export carried no `current_node` pointer.
    MissingCurrentNode,
    /// `current_node` pointed at a node that is not in the mapping.
    CurrentNodeNotFound { id: String },
    /// A node's `parent` names an id the mapping does not contain, so the walk
    /// had to treat that node as an effective root.
    BrokenParent { node: String, parent: String },
    /// The parent chain looped back on itself; the walk stopped there.
    CycleDetected { node: String },
    /// The primary strategy could not produce a useful branch, so the longest
    /// reachable path was used instead.
    FellBackToLongestPath,
    /// The reconstructed branch contains no message-carrying node, even though
    /// the conversation has messages somewhere else in the graph.
    NoMessagesOnBranch,
    /// A guessed branch recovered far fewer messages than the graph holds.
    ///
    /// Only ever emitted on the fallback path. On the normal path a branch that
    /// omits messages is *correct* — abandoned regenerations are supposed to be
    /// left out — but once we are guessing, a guess that captures a small
    /// fraction of the graph usually means the export's `parent` links are
    /// incomplete, and the user deserves to know before trusting the output.
    PartialRecovery { recovered: usize, total: usize },
}

impl std::fmt::Display for BranchWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchWarning::MissingCurrentNode => {
                write!(f, "export has no `current_node`; guessed the active branch")
            }
            BranchWarning::CurrentNodeNotFound { id } => write!(
                f,
                "`current_node` `{id}` is not in the conversation mapping; guessed the active branch"
            ),
            BranchWarning::BrokenParent { node, parent } => write!(
                f,
                "node `{node}` points at missing parent `{parent}`; treated `{node}` as the root"
            ),
            BranchWarning::CycleDetected { node } => write!(
                f,
                "parent chain loops back to node `{node}`; stopped the walk there"
            ),
            BranchWarning::FellBackToLongestPath => {
                write!(f, "fell back to the longest path in the conversation graph")
            }
            BranchWarning::NoMessagesOnBranch => {
                write!(f, "the reconstructed branch carries no messages")
            }
            BranchWarning::PartialRecovery { recovered, total } => write!(
                f,
                "guessed branch recovered only {recovered} of {total} messages in the graph; \
                 the export's parent links look incomplete"
            ),
        }
    }
}

/// One reconstructed path through a conversation graph, root first.
#[derive(Debug, Clone)]
pub struct ConversationBranch {
    /// Node ids from the root down to the leaf. Every id exists in the
    /// conversation's mapping, and consecutive ids are a real parent → child
    /// step. Message-less nodes (the synthetic root, for instance) are kept so
    /// that graph depth stays honest.
    pub node_ids: Vec<String>,
    /// Recoverable damage found on the way.
    pub warnings: Vec<BranchWarning>,
    /// Name of the strategy that actually produced this branch — after any
    /// fallback, not the one that was asked for.
    pub strategy: &'static str,
}

/// A message on a branch, borrowed from the conversation.
///
/// Message content is often large; branches are rendered, counted and searched
/// repeatedly, so this never clones.
#[derive(Debug, Clone, Copy)]
pub struct BranchMessage<'a> {
    pub node_id: &'a str,
    pub message: &'a Message,
}

impl ConversationBranch {
    /// Number of graph nodes on the branch, including message-less ones.
    pub fn len(&self) -> usize {
        self.node_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.node_ids.is_empty()
    }

    /// The branch's messages in order, skipping nodes that carry none.
    ///
    /// Ids absent from `conversation` are skipped rather than panicking: a
    /// branch is only ever built from one conversation, but passing a different
    /// one must degrade quietly rather than crash.
    pub fn messages<'a>(&'a self, conversation: &'a Conversation) -> Vec<BranchMessage<'a>> {
        self.node_ids
            .iter()
            .filter_map(|id| {
                let node = conversation.node(id)?;
                let message = node.message.as_ref()?;
                Some(BranchMessage {
                    node_id: id.as_str(),
                    message,
                })
            })
            .collect()
    }
}

/// A way of choosing which path through the graph is "the conversation".
///
/// The trait exists so alternate policies can be added without touching
/// callers — a "longest path containing a given node" strategy for `--node`, or
/// a "most recent by timestamp" strategy for exports with a broken
/// `current_node`. It is object safe, so strategies can be selected at runtime
/// and stored as `Box<dyn BranchStrategy>`.
pub trait BranchStrategy {
    /// Stable identifier recorded in [`ConversationBranch::strategy`].
    fn name(&self) -> &'static str;

    /// Reconstruct a branch, or explain why no branch exists at all.
    fn resolve(&self, conversation: &Conversation) -> Result<ConversationBranch, GraphError>;
}

/// The default: follow `parent` links up from `current_node`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentNodeStrategy;

/// Fallback: the deepest path reachable from any root.
#[derive(Debug, Clone, Copy, Default)]
pub struct LongestPathStrategy;

impl BranchStrategy for CurrentNodeStrategy {
    fn name(&self) -> &'static str {
        "current-node"
    }

    /// Walk up from `current_node`, falling back to [`LongestPathStrategy`]
    /// whenever that pointer cannot give a useful answer.
    ///
    /// The fallback fires in three cases: no `current_node`, a `current_node`
    /// that is not in the mapping, and a branch that turns out to carry no
    /// messages while the conversation has messages elsewhere. In all three a
    /// plausible transcript beats an error, because the usual cause is a
    /// partial or truncated export rather than a genuinely empty conversation.
    fn resolve(&self, conversation: &Conversation) -> Result<ConversationBranch, GraphError> {
        if conversation.mapping.is_empty() {
            return Err(GraphError::EmptyMapping {
                id: conversation.id.clone(),
            });
        }

        let start = match conversation.current_node.as_deref() {
            None => {
                return fall_back(conversation, vec![BranchWarning::MissingCurrentNode]);
            }
            Some(id) if conversation.node(id).is_none() => {
                return fall_back(
                    conversation,
                    vec![BranchWarning::CurrentNodeNotFound { id: id.to_string() }],
                );
            }
            Some(id) => id,
        };

        let (node_ids, mut warnings) = walk_to_root(conversation, start);

        if !carries_a_message(conversation, &node_ids) {
            // A message-less branch is only fatal when the whole conversation
            // is message-less; otherwise another branch is worth showing.
            if !conversation.mapping.values().any(|node| node.has_message()) {
                return Err(GraphError::NoMessages {
                    id: conversation.id.clone(),
                });
            }
            warnings.push(BranchWarning::NoMessagesOnBranch);
            return fall_back(conversation, warnings);
        }

        Ok(ConversationBranch {
            node_ids,
            warnings,
            strategy: self.name(),
        })
    }
}

impl BranchStrategy for LongestPathStrategy {
    fn name(&self) -> &'static str {
        "longest-path"
    }

    /// Pick the node with the longest `parent` chain, then walk back up from
    /// it.
    ///
    /// Depth is measured along `parent` — the same edge [`walk_to_root`]
    /// reconstructs with — and never along `children`; see
    /// [`longest_parent_chain`] for why measuring on the other edge silently
    /// dropped messages.
    ///
    /// Ties are broken by message count and then by lexicographically smallest
    /// id, because `HashMap` iteration order is not stable and two runs over the
    /// same file must produce the same transcript.
    fn resolve(&self, conversation: &Conversation) -> Result<ConversationBranch, GraphError> {
        if conversation.mapping.is_empty() {
            return Err(GraphError::EmptyMapping {
                id: conversation.id.clone(),
            });
        }

        let deepest = match longest_parent_chain(conversation) {
            Some(id) => id,
            // A non-empty mapping always seeds at least one node, so this is
            // unreachable in practice; report it as an empty graph rather than
            // panicking if that invariant is ever broken.
            None => {
                return Err(GraphError::EmptyMapping {
                    id: conversation.id.clone(),
                });
            }
        };

        let (node_ids, mut warnings) = walk_to_root(conversation, deepest);

        if !carries_a_message(conversation, &node_ids) {
            if !conversation.mapping.values().any(|node| node.has_message()) {
                return Err(GraphError::NoMessages {
                    id: conversation.id.clone(),
                });
            }
            warnings.push(BranchWarning::NoMessagesOnBranch);
        }

        Ok(ConversationBranch {
            node_ids,
            warnings,
            strategy: self.name(),
        })
    }
}

/// Reconstruct the branch the user was last looking at.
///
/// This is [`CurrentNodeStrategy`] with its built-in fallbacks, and is what
/// every command in the tool should use unless the user asked for something
/// else explicitly.
pub fn active_branch(conversation: &Conversation) -> Result<ConversationBranch, GraphError> {
    CurrentNodeStrategy.resolve(conversation)
}

/// Run [`LongestPathStrategy`] and prepend the warnings that led us here.
///
/// The returned branch reports the *fallback* strategy name, so a caller can
/// always tell how the output was actually produced.
fn fall_back(
    conversation: &Conversation,
    mut warnings: Vec<BranchWarning>,
) -> Result<ConversationBranch, GraphError> {
    let mut branch = LongestPathStrategy.resolve(conversation)?;
    warnings.push(BranchWarning::FellBackToLongestPath);
    warnings.append(&mut branch.warnings);

    let recovered = branch.messages(conversation).len();
    let total = conversation
        .mapping
        .values()
        .filter(|node| node.has_message())
        .count();
    // Half is the line between "this conversation forks a lot", which is normal
    // and not worth a warning, and "we recovered a sliver of the graph", which
    // means the export is damaged in a way the user needs to see.
    if recovered * 2 < total {
        warnings.push(BranchWarning::PartialRecovery { recovered, total });
    }

    branch.warnings = warnings;
    Ok(branch)
}

/// Follow `parent` links from `start` up to a root, returning ids root-first.
///
/// Iterative and visited-guarded: a cycle stops the walk with a
/// [`BranchWarning::CycleDetected`] instead of recursing forever, and a parent
/// missing from the mapping ends the walk with a
/// [`BranchWarning::BrokenParent`], treating the current node as an effective
/// root. Cost is O(branch depth).
fn walk_to_root<'a>(
    conversation: &'a Conversation,
    start: &'a str,
) -> (Vec<String>, Vec<BranchWarning>) {
    let mut ids: Vec<&'a str> = Vec::new();
    let mut warnings = Vec::new();
    let mut visited: HashSet<&'a str> = HashSet::new();
    let mut cursor = Some(start);

    while let Some(id) = cursor {
        let Some(node) = conversation.node(id) else {
            // Only reachable if a caller passes an id from another
            // conversation; stop rather than fabricate a node.
            break;
        };
        if !visited.insert(id) {
            warnings.push(BranchWarning::CycleDetected {
                node: id.to_string(),
            });
            break;
        }
        ids.push(id);

        cursor = match node.parent.as_deref() {
            None => None,
            Some(parent) if conversation.node(parent).is_none() => {
                warnings.push(BranchWarning::BrokenParent {
                    node: id.to_string(),
                    parent: parent.to_string(),
                });
                None
            }
            Some(parent) if visited.contains(parent) => {
                warnings.push(BranchWarning::CycleDetected {
                    node: parent.to_string(),
                });
                None
            }
            Some(parent) => Some(parent),
        };
    }

    ids.reverse();
    (ids.into_iter().map(str::to_string).collect(), warnings)
}

/// True when at least one node on the path carries a message.
fn carries_a_message(conversation: &Conversation, node_ids: &[String]) -> bool {
    node_ids
        .iter()
        .filter_map(|id| conversation.node(id))
        .any(|node| node.has_message())
}

/// What a node's `parent` chain looks like: how many nodes
/// [`walk_to_root`] would collect starting there, how many carry a message, and
/// whether the chain ends cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chain {
    length: usize,
    messages: usize,
    /// True when the chain terminates at a genuine root — a node whose `parent`
    /// is `None`. False when it dies at a parent missing from the mapping or
    /// runs into a cycle, i.e. when the node is not connected to any beginning
    /// the conversation actually has.
    grounded: bool,
}

/// The node whose `parent` chain is longest — the leaf the fallback should
/// render from.
///
/// **This must measure along `parent`, not `children`.** `parent` and
/// `children` are independent fields in the export and routinely disagree:
/// truncated exports ship a complete `parent` chain with every `children` list
/// empty, which is precisely the shape this fallback exists to serve. Measuring
/// on `children` and then reconstructing with [`walk_to_root`] (which follows
/// `parent`) scored every node at depth 0 in that case and silently rendered a
/// single message out of eight. Measure with the edge you reconstruct with.
///
/// Using `parent` also removes route ambiguity for free. `children` is
/// multi-valued, so a node reachable by two routes got whichever depth the
/// breadth-first search found *first* — the shortest — which let a diamond
/// exclude the real leaf. `parent` is `Option<String>`: exactly one edge out of
/// each node, so a node's chain length is a property of the node itself and
/// there is no shorter or longer route to disagree about.
///
/// Cost is O(V). Each node's chain is computed once and memoized; a walk stops
/// as soon as it reaches a node whose chain is already known, so the total work
/// is bounded by the number of nodes rather than the sum of the chain lengths.
///
/// Ties are resolved by message count and then by smallest id, so the result is
/// stable across runs despite `HashMap` ordering.
fn longest_parent_chain(conversation: &Conversation) -> Option<&str> {
    let chains = parent_chains(conversation);

    let mut sorted_ids: Vec<&str> = conversation.mapping.keys().map(String::as_str).collect();
    sorted_ids.sort_unstable();

    let mut best: Option<(&str, Chain)> = None;
    for id in sorted_ids {
        let Some(&chain) = chains.get(id) else {
            continue;
        };
        let is_better = match best {
            None => true,
            // `sorted_ids` ascends, so a strict improvement is the only way to
            // displace the incumbent; that makes the smallest id win ties.
            Some((_, incumbent)) => {
                (chain.length, chain.messages) > (incumbent.length, incumbent.messages)
            }
        };
        if is_better {
            best = Some((id, chain));
        }
    }

    best.map(|(id, _)| id)
}

/// Ids whose `parent` chain never reaches a genuine root.
///
/// A node is *grounded* when following `parent` from it terminates at a node
/// with no parent. It is ungrounded when the chain dies at an id missing from
/// the mapping, or runs into a cycle — in both cases the node is detached from
/// any beginning the conversation actually has.
///
/// This is deliberately the same relation [`walk_to_root`] traverses. Defining
/// connectivity on `children` instead is what made `unreachable_nodes` report
/// eight stranded nodes for a conversation whose `parent` chain was completely
/// intact, while missing a real orphan whose parent id was absent.
///
/// Shares the memoized [`parent_chains`] walk, so it stays O(V), iterative and
/// cycle-guarded.
pub(super) fn ungrounded_node_ids(conversation: &Conversation) -> HashSet<&str> {
    parent_chains(conversation)
        .into_iter()
        .filter(|(_, chain)| !chain.grounded)
        .map(|(id, _)| id)
        .collect()
}

/// Chain length and message count for every node, measured along `parent`.
///
/// Iterative and memoized. The `parent` relation is a functional graph — one
/// edge out of every node — so each walk ends in exactly one of four ways: a
/// node with no parent, a parent missing from the mapping, a node whose chain
/// is already known, or a node already on the walk (a cycle).
///
/// The cycle case is the only subtle one. [`walk_to_root`] entering a cycle
/// collects every distinct node in it and then stops, so every node *on* a
/// cycle has the same chain — the cycle itself — and a node hanging off a cycle
/// has its tail plus the whole cycle. Assigning the cycle's own length to each
/// of its members makes the ordinary `1 + parent` rule produce exactly that for
/// the tail nodes below it.
fn parent_chains(conversation: &Conversation) -> HashMap<&str, Chain> {
    let mut known: HashMap<&str, Chain> = HashMap::with_capacity(conversation.mapping.len());
    let mut sorted_ids: Vec<&str> = conversation.mapping.keys().map(String::as_str).collect();
    sorted_ids.sort_unstable();

    let mut path: Vec<&str> = Vec::new();
    let mut position: HashMap<&str, usize> = HashMap::new();

    for start in sorted_ids {
        if known.contains_key(start) {
            continue;
        }
        path.clear();
        position.clear();

        // Metrics of the node just above the walk: zero when the walk ended at
        // a root or a broken parent, the memoized chain when it met a known
        // node, the cycle's own chain when it met itself.
        // Overwritten on every path that actually terminates; `false` is the
        // conservative default so an unforeseen exit reports damage rather
        // than hiding it.
        let mut above = Chain {
            length: 0,
            messages: 0,
            grounded: false,
        };
        let mut cycle_at = None;
        let mut cursor = Some(start);

        while let Some(id) = cursor {
            if let Some(&chain) = known.get(id) {
                above = chain;
                break;
            }
            if let Some(&index) = position.get(id) {
                cycle_at = Some(index);
                break;
            }
            let Some(node) = conversation.node(id) else {
                // Unreachable: every id walked comes from the mapping.
                break;
            };
            position.insert(id, path.len());
            path.push(id);
            cursor = match node.parent.as_deref() {
                Some(parent) if conversation.node(parent).is_some() => Some(parent),
                // A genuine root: the chain ends cleanly.
                None => {
                    above = Chain {
                        length: 0,
                        messages: 0,
                        grounded: true,
                    };
                    None
                }
                // A parent the export never included. This node is an effective
                // root for walking purposes, but it is *not* grounded: the real
                // beginning of the conversation is missing.
                Some(_) => {
                    above = Chain {
                        length: 0,
                        messages: 0,
                        grounded: false,
                    };
                    None
                }
            };
        }

        if let Some(index) = cycle_at {
            let cycle = Chain {
                length: path.len() - index,
                messages: path[index..]
                    .iter()
                    .filter(|id| conversation.node(id).is_some_and(|n| n.has_message()))
                    .count(),
                // A cycle never reaches a root, so nothing on or below it is
                // grounded.
                grounded: false,
            };
            for &id in &path[index..] {
                known.insert(id, cycle);
            }
            above = cycle;
            path.truncate(index);
        }

        // Unwind from the topmost node down to `start`, each one adding itself
        // to whatever sits above it.
        for &id in path.iter().rev() {
            above = Chain {
                length: above.length + 1,
                messages: above.messages
                    + usize::from(conversation.node(id).is_some_and(|n| n.has_message())),
                grounded: above.grounded,
            };
            known.insert(id, above);
        }
    }

    known
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, ConversationNode, Message, MessageContent, MessageMetadata, Role};

    fn message(role: Role, text: &str) -> Message {
        Message {
            id: None,
            author: Author { role, name: None },
            create_time: None,
            content: MessageContent::Text {
                parts: vec![text.to_string()],
            },
            metadata: MessageMetadata::default(),
            recipient: None,
        }
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
            title: Some("test".to_string()),
            create_time: None,
            update_time: None,
            current_node: current_node.map(str::to_string),
            mapping,
        }
    }

    fn user(text: &str) -> Option<Message> {
        Some(message(Role::User, text))
    }

    fn assistant(text: &str) -> Option<Message> {
        Some(message(Role::Assistant, text))
    }

    fn texts(branch: &ConversationBranch, conversation: &Conversation) -> Vec<String> {
        branch
            .messages(conversation)
            .iter()
            .map(|m| m.message.content.plain_text())
            .collect()
    }

    #[test]
    fn simple_linear_conversation() {
        let conversation = conversation(
            Some("a2"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec!["a1"], user("hello")),
                ("a1", Some("u1"), vec!["u2"], assistant("hi")),
                ("u2", Some("a1"), vec!["a2"], user("more")),
                ("a2", Some("u2"), vec![], assistant("sure")),
            ],
        );

        let branch = active_branch(&conversation).expect("linear branch resolves");
        assert_eq!(branch.node_ids, ["root", "u1", "a1", "u2", "a2"]);
        assert_eq!(branch.strategy, "current-node");
        assert!(branch.warnings.is_empty());
        assert_eq!(
            texts(&branch, &conversation),
            ["hello", "hi", "more", "sure"]
        );
    }

    #[test]
    fn alternate_branch_excludes_the_path_not_taken() {
        // u1 forks into two assistant answers; current_node picks the second.
        let conversation = conversation(
            Some("a-taken"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec!["a-other", "a-taken"], user("q")),
                (
                    "a-other",
                    Some("u1"),
                    vec!["tail"],
                    assistant("abandoned answer"),
                ),
                ("tail", Some("a-other"), vec![], user("abandoned follow-up")),
                ("a-taken", Some("u1"), vec![], assistant("kept answer")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch resolves");
        assert_eq!(branch.node_ids, ["root", "u1", "a-taken"]);
        let rendered = texts(&branch, &conversation);
        assert_eq!(rendered, ["q", "kept answer"]);
        assert!(!rendered.iter().any(|t| t.contains("abandoned")));
    }

    #[test]
    fn regenerated_assistant_response_keeps_only_the_current_one() {
        // Classic regeneration: same parent, three sibling answers.
        let conversation = conversation(
            Some("gen3"),
            vec![
                ("root", None, vec!["u1"], None),
                (
                    "u1",
                    Some("root"),
                    vec!["gen1", "gen2", "gen3"],
                    user("write a haiku"),
                ),
                ("gen1", Some("u1"), vec![], assistant("first attempt")),
                ("gen2", Some("u1"), vec![], assistant("second attempt")),
                ("gen3", Some("u1"), vec![], assistant("third attempt")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch resolves");
        assert_eq!(branch.node_ids, ["root", "u1", "gen3"]);
        assert_eq!(
            texts(&branch, &conversation),
            ["write a haiku", "third attempt"]
        );
    }

    #[test]
    fn message_less_nodes_stay_on_the_branch_but_yield_no_messages() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["mid"], None),
                ("mid", Some("root"), vec!["u1"], None),
                ("u1", Some("mid"), vec![], user("only message")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch resolves");
        assert_eq!(branch.node_ids, ["root", "mid", "u1"]);
        assert_eq!(branch.len(), 3);
        assert_eq!(branch.messages(&conversation).len(), 1);
    }

    #[test]
    fn orphan_node_is_not_spliced_into_the_active_branch() {
        let conversation = conversation(
            Some("a1"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec!["a1"], user("q")),
                ("a1", Some("u1"), vec![], assistant("a")),
                // Detached from everything, parent-less, never referenced.
                ("orphan", None, vec![], user("stray")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch resolves");
        assert_eq!(branch.node_ids, ["root", "u1", "a1"]);
        assert!(!branch.node_ids.contains(&"orphan".to_string()));
    }

    #[test]
    fn broken_parent_pointer_ends_the_walk_with_a_warning() {
        let conversation = conversation(
            Some("a1"),
            vec![
                ("u1", Some("ghost-root"), vec!["a1"], user("q")),
                ("a1", Some("u1"), vec![], assistant("a")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch still resolves");
        assert_eq!(branch.node_ids, ["u1", "a1"]);
        assert_eq!(
            branch.warnings,
            [BranchWarning::BrokenParent {
                node: "u1".to_string(),
                parent: "ghost-root".to_string(),
            }]
        );
        assert!(branch.warnings[0].to_string().contains("ghost-root"));
    }

    #[test]
    fn graph_cycle_terminates_the_walk() {
        let conversation = conversation(
            Some("c"),
            vec![
                ("a", Some("c"), vec!["b"], user("a")),
                ("b", Some("a"), vec!["c"], assistant("b")),
                ("c", Some("b"), vec!["a"], user("c")),
            ],
        );

        let branch = active_branch(&conversation).expect("branch resolves despite the cycle");
        assert_eq!(branch.node_ids, ["a", "b", "c"]);
        assert!(
            branch
                .warnings
                .iter()
                .any(|w| matches!(w, BranchWarning::CycleDetected { .. }))
        );
        // The cycle must not duplicate nodes.
        let mut unique = branch.node_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), branch.node_ids.len());
    }

    #[test]
    fn self_referencing_parent_is_a_cycle_not_a_hang() {
        let conversation = conversation(Some("a"), vec![("a", Some("a"), vec!["a"], user("loop"))]);

        let branch = active_branch(&conversation).expect("branch resolves");
        assert_eq!(branch.node_ids, ["a"]);
        assert!(branch.warnings.contains(&BranchWarning::CycleDetected {
            node: "a".to_string()
        }));
    }

    #[test]
    fn missing_current_node_falls_back_to_the_longest_path() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["short", "u1"], None),
                ("short", Some("root"), vec![], assistant("shallow")),
                ("u1", Some("root"), vec!["a1"], user("q")),
                ("a1", Some("u1"), vec!["u2"], assistant("a")),
                ("u2", Some("a1"), vec![], user("deepest")),
            ],
        );

        let branch = active_branch(&conversation).expect("fallback resolves");
        assert_eq!(branch.strategy, "longest-path");
        assert_eq!(branch.node_ids, ["root", "u1", "a1", "u2"]);
        assert_eq!(
            branch.warnings,
            [
                BranchWarning::MissingCurrentNode,
                BranchWarning::FellBackToLongestPath
            ]
        );
    }

    #[test]
    fn dangling_current_node_falls_back_to_the_longest_path() {
        let conversation = conversation(
            Some("does-not-exist"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec![], user("q")),
            ],
        );

        let branch = active_branch(&conversation).expect("fallback resolves");
        assert_eq!(branch.strategy, "longest-path");
        assert_eq!(branch.node_ids, ["root", "u1"]);
        assert_eq!(
            branch.warnings,
            [
                BranchWarning::CurrentNodeNotFound {
                    id: "does-not-exist".to_string()
                },
                BranchWarning::FellBackToLongestPath
            ]
        );
    }

    #[test]
    fn empty_mapping_is_an_error() {
        let conversation = conversation(Some("a"), vec![]);
        assert_eq!(
            active_branch(&conversation).err(),
            Some(GraphError::EmptyMapping {
                id: "conv-1".to_string()
            })
        );
    }

    #[test]
    fn a_conversation_without_any_message_is_an_error() {
        let conversation = conversation(
            Some("b"),
            vec![("a", None, vec!["b"], None), ("b", Some("a"), vec![], None)],
        );
        assert_eq!(
            active_branch(&conversation).err(),
            Some(GraphError::NoMessages {
                id: "conv-1".to_string()
            })
        );
    }

    #[test]
    fn message_less_current_branch_falls_back_when_messages_exist_elsewhere() {
        let conversation = conversation(
            Some("empty-leaf"),
            vec![
                ("root", None, vec!["empty-leaf", "u1"], None),
                ("empty-leaf", Some("root"), vec![], None),
                ("u1", Some("root"), vec!["a1"], user("q")),
                ("a1", Some("u1"), vec![], assistant("a")),
            ],
        );

        let branch = active_branch(&conversation).expect("fallback finds the messages");
        assert_eq!(branch.strategy, "longest-path");
        assert_eq!(branch.node_ids, ["root", "u1", "a1"]);
        assert_eq!(
            branch.warnings,
            [
                BranchWarning::NoMessagesOnBranch,
                BranchWarning::FellBackToLongestPath
            ]
        );
    }

    #[test]
    fn longest_path_reaches_nodes_trapped_in_a_cycle() {
        // No node qualifies as a root: every parent is present in the mapping.
        let conversation = conversation(
            None,
            vec![
                ("a", Some("b"), vec!["b"], user("a")),
                ("b", Some("a"), vec!["a"], assistant("b")),
            ],
        );

        let branch = active_branch(&conversation).expect("cycle-only graphs still resolve");
        assert!(!branch.node_ids.is_empty());
        assert!(
            branch
                .node_ids
                .iter()
                .all(|id| conversation.node(id).is_some())
        );
    }

    /// Regression: a truncated export ships a complete `parent` chain with
    /// every `children` list empty. Measuring depth on `children` scored every
    /// node at 0 and rendered 1 message out of 8.
    #[test]
    fn intact_parent_chain_with_empty_children_recovers_every_message() {
        let mut nodes: Vec<NodeSpec<'_>> = vec![("root", None, vec![], None)];
        let ids: Vec<String> = (1..=8).map(|i| format!("n{i}")).collect();
        let parents: Vec<String> = std::iter::once("root".to_string())
            .chain(ids.iter().take(7).cloned())
            .collect();
        for (index, id) in ids.iter().enumerate() {
            nodes.push((
                id.as_str(),
                Some(parents[index].as_str()),
                // Deliberately empty: this is what a truncated export looks like.
                vec![],
                user(&format!("message number {} content here", index + 1)),
            ));
        }
        let conversation = conversation(None, nodes);

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.strategy, "longest-path");
        assert_eq!(
            branch.node_ids,
            ["root", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8"]
        );
        assert_eq!(branch.messages(&conversation).len(), 8);
        // Full recovery, so no partial-recovery warning.
        assert!(
            !branch
                .warnings
                .iter()
                .any(|w| matches!(w, BranchWarning::PartialRecovery { .. }))
        );
    }

    /// Regression: `root.children` also names `e`, which is really the leaf of
    /// `root -> a -> b -> c -> d -> e`. A breadth-first search over `children`
    /// fixed `e` at depth 1 and dropped the final message.
    #[test]
    fn diamond_children_shortcut_does_not_truncate_the_branch() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["a", "e"], None),
                ("a", Some("root"), vec!["b"], user("one")),
                ("b", Some("a"), vec!["c"], assistant("two")),
                ("c", Some("b"), vec!["d"], user("three")),
                ("d", Some("c"), vec!["e"], assistant("four")),
                (
                    "e",
                    Some("d"),
                    vec![],
                    user("FINAL MESSAGE that must appear"),
                ),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.node_ids, ["root", "a", "b", "c", "d", "e"]);
        assert!(
            texts(&branch, &conversation)
                .iter()
                .any(|t| t.contains("FINAL MESSAGE")),
            "the deepest message-carrying leaf must be on the branch"
        );
    }

    /// The chain length of a node hanging off a cycle is its tail plus the
    /// whole cycle, which is exactly what `walk_to_root` collects from it.
    #[test]
    fn chain_length_of_a_tail_below_a_cycle_includes_the_whole_cycle() {
        let conversation = conversation(
            None,
            vec![
                ("a", Some("c"), vec![], user("a")),
                ("b", Some("a"), vec![], user("b")),
                ("c", Some("b"), vec![], user("c")),
                ("tail", Some("a"), vec![], user("tail")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        // walk_to_root from `tail`: tail, a, c, b — then b's parent `a` is
        // already visited, so the walk stops. Four nodes, the deepest chain.
        assert_eq!(branch.len(), 4);
        assert_eq!(branch.node_ids.last().map(String::as_str), Some("tail"));
    }

    /// A guess that recovers a sliver of the graph has to say so.
    #[test]
    fn a_fallback_that_recovers_little_warns_about_it() {
        // Six message nodes, each its own root: the best chain holds one.
        let nodes: Vec<NodeSpec<'_>> = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(|id| (id, None, vec![], user("stranded")))
            .collect();
        let conversation = conversation(None, nodes);

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.messages(&conversation).len(), 1);
        assert!(branch.warnings.contains(&BranchWarning::PartialRecovery {
            recovered: 1,
            total: 6
        }));
        let rendered = branch
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("1 of 6"));
    }

    /// A conversation that merely forks is not "partial recovery" — the warning
    /// must not cry wolf on healthy exports.
    #[test]
    fn an_ordinary_fork_does_not_warn_about_partial_recovery() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec!["gen1", "gen2"], user("q")),
                ("gen1", Some("u1"), vec![], assistant("first")),
                ("gen2", Some("u1"), vec![], assistant("second")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.messages(&conversation).len(), 2);
        assert!(
            !branch
                .warnings
                .iter()
                .any(|w| matches!(w, BranchWarning::PartialRecovery { .. }))
        );
    }

    #[test]
    fn longest_path_ties_break_deterministically() {
        // Two equally deep, equally message-rich leaves: `alpha` must win.
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["zulu", "alpha"], None),
                ("alpha", Some("root"), vec![], user("x")),
                ("zulu", Some("root"), vec![], user("x")),
            ],
        );

        for _ in 0..16 {
            let branch = active_branch(&conversation).expect("resolves");
            assert_eq!(branch.node_ids, ["root", "alpha"]);
        }
    }

    #[test]
    fn longest_path_prefers_the_branch_with_more_messages_at_equal_depth() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["a1", "b1"], None),
                ("a1", Some("root"), vec!["a2"], None),
                ("a2", Some("a1"), vec![], user("only one")),
                ("b1", Some("root"), vec!["b2"], user("one")),
                ("b2", Some("b1"), vec![], user("two")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.node_ids, ["root", "b1", "b2"]);
    }

    #[test]
    fn children_naming_unknown_ids_are_skipped() {
        let conversation = conversation(
            None,
            vec![
                ("root", None, vec!["ghost", "u1"], None),
                ("u1", Some("root"), vec!["also-ghost"], user("q")),
            ],
        );

        let branch = active_branch(&conversation).expect("resolves");
        assert_eq!(branch.node_ids, ["root", "u1"]);
    }

    #[test]
    fn messages_borrow_and_report_node_ids() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec![], user("hello")),
            ],
        );
        let branch = active_branch(&conversation).expect("resolves");
        let messages = branch.messages(&conversation);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].node_id, "u1");
        assert_eq!(messages[0].message.role(), &Role::User);
    }

    #[test]
    fn strategies_are_usable_as_trait_objects() {
        let conversation = conversation(
            Some("u1"),
            vec![
                ("root", None, vec!["u1"], None),
                ("u1", Some("root"), vec![], user("hello")),
            ],
        );
        let strategies: Vec<Box<dyn BranchStrategy>> =
            vec![Box::new(CurrentNodeStrategy), Box::new(LongestPathStrategy)];
        for strategy in &strategies {
            let branch = strategy.resolve(&conversation).expect("resolves");
            assert_eq!(branch.node_ids, ["root", "u1"]);
            assert_eq!(branch.strategy, strategy.name());
        }
    }

    #[test]
    fn warning_messages_are_human_readable_one_liners() {
        let warnings = [
            BranchWarning::MissingCurrentNode,
            BranchWarning::CurrentNodeNotFound { id: "x".into() },
            BranchWarning::BrokenParent {
                node: "n".into(),
                parent: "p".into(),
            },
            BranchWarning::CycleDetected { node: "n".into() },
            BranchWarning::FellBackToLongestPath,
            BranchWarning::NoMessagesOnBranch,
            BranchWarning::PartialRecovery {
                recovered: 1,
                total: 8,
            },
        ];
        for warning in warnings {
            let rendered = warning.to_string();
            assert!(!rendered.is_empty());
            assert!(!rendered.contains('\n'));
        }
    }

    #[test]
    fn deep_chains_do_not_blow_the_stack() {
        let depth = 100_000;
        let mut mapping = HashMap::new();
        for index in 0..depth {
            mapping.insert(
                format!("n{index}"),
                ConversationNode {
                    id: format!("n{index}"),
                    message: (index % 2 == 0).then(|| message(Role::User, "x")),
                    parent: (index > 0).then(|| format!("n{}", index - 1)),
                    children: if index + 1 < depth {
                        vec![format!("n{}", index + 1)]
                    } else {
                        vec![]
                    },
                },
            );
        }
        let conversation = Conversation {
            id: "deep".to_string(),
            title: None,
            create_time: None,
            update_time: None,
            current_node: Some(format!("n{}", depth - 1)),
            mapping,
        };

        let branch = active_branch(&conversation).expect("deep chains resolve");
        assert_eq!(branch.len(), depth);
    }
}
