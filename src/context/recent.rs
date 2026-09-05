//! Selection of the verbatim conversation tail.
//!
//! The tail is the highest-value part of a handoff document: everything else
//! in `context.md` is lossy heuristic extraction, but the last N messages are
//! reproduced as-is. This module decides *how many* of them fit, and it only
//! ever cuts at message boundaries — a half-message tail would be worse than a
//! shorter whole one.

use crate::graph::BranchMessage;
use crate::text;

/// Where the verbatim tail starts and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecentSelection {
    /// Index into the slice passed to [`select_recent`] where the tail begins.
    /// `start_index .. start_index + message_count` is always a valid range
    /// over that slice, including when `message_count` is zero.
    pub start_index: usize,
    pub message_count: usize,
    /// Total grapheme clusters of plain text across the selected messages.
    pub characters: usize,
}

/// Choose the trailing run of messages that fits both limits.
///
/// Walks backward from the end, accumulating messages until either limit would
/// be exceeded; when both are given the stricter one wins. `characters` counts
/// grapheme clusters of [`crate::model::MessageContent::plain_text`], so the
/// budget means the same thing for Hebrew, emoji and ASCII.
///
/// Rules that are not obvious from the signature:
///
/// * Cuts happen at message boundaries only — never mid-message, never
///   mid-grapheme.
/// * At least one message is always selected when the slice is non-empty and
///   `max_messages >= 1`, even if that message alone busts `max_chars`. An
///   empty tail is useless to the reading model, and a truncated final message
///   is worse than an oversized one.
/// * `max_messages == 0` means zero messages, whatever `max_chars` says — an
///   explicit zero is an instruction, not a budget.
pub fn select_recent(
    messages: &[BranchMessage<'_>],
    max_messages: usize,
    max_chars: Option<usize>,
) -> RecentSelection {
    if messages.is_empty() {
        return RecentSelection {
            start_index: 0,
            message_count: 0,
            characters: 0,
        };
    }
    if max_messages == 0 {
        return RecentSelection {
            start_index: messages.len(),
            message_count: 0,
            characters: 0,
        };
    }

    let mut count = 0usize;
    let mut characters = 0usize;
    for entry in messages.iter().rev() {
        if count == max_messages {
            break;
        }
        let size = text::grapheme_count(&entry.message.content.plain_text());
        if let Some(limit) = max_chars {
            // `count > 0` is the always-include-one rule; `>` (not `>=`) makes
            // a message that lands exactly on the limit fit.
            if count > 0 && characters + size > limit {
                break;
            }
        }
        characters += size;
        count += 1;
    }

    RecentSelection {
        start_index: messages.len() - count,
        message_count: count,
        characters,
    }
}

/// Synthetic [`crate::model::Message`] fixtures shared by the `context` tests.
///
/// `graph` and `transcript` are built independently, so context tests never
/// call [`crate::graph::active_branch`] or
/// [`crate::transcript::render_messages`]; they build message slices directly
/// with these helpers instead.
#[cfg(test)]
pub(crate) mod fixtures {
    use crate::graph::BranchMessage;
    use crate::model::{Author, Message, MessageContent, Role};

    /// One message with a stable node id derived from `index`.
    pub(crate) fn message(index: usize, role: Role, text: &str) -> Message {
        Message {
            id: Some(format!("node-{index}")),
            author: Author { role, name: None },
            create_time: Some(1_757_000_000.0 + index as f64),
            content: MessageContent::Text {
                parts: vec![text.to_string()],
            },
            metadata: Default::default(),
            recipient: None,
        }
    }

    /// Build a `(role, text)` script into messages, alternating nothing —
    /// roles are explicit so tests read like the conversation they describe.
    pub(crate) fn script(turns: &[(Role, &str)]) -> Vec<Message> {
        turns
            .iter()
            .enumerate()
            .map(|(index, (role, text))| message(index, role.clone(), text))
            .collect()
    }

    /// Borrow a message slice as branch entries.
    pub(crate) fn branch(messages: &[Message]) -> Vec<BranchMessage<'_>> {
        messages
            .iter()
            .map(|message| BranchMessage {
                node_id: message.id.as_deref().unwrap_or("node"),
                message,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{branch, script};
    use super::*;
    use crate::model::Role;

    fn sized(sizes: &[usize]) -> Vec<crate::model::Message> {
        let turns: Vec<(Role, String)> = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                let role = if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                (role, "x".repeat(*size))
            })
            .collect();
        let refs: Vec<(Role, &str)> = turns
            .iter()
            .map(|(role, text)| (role.clone(), text.as_str()))
            .collect();
        script(&refs)
    }

    #[test]
    fn empty_input_selects_nothing() {
        let selection = select_recent(&[], 10, Some(100));
        assert_eq!(
            selection,
            RecentSelection {
                start_index: 0,
                message_count: 0,
                characters: 0
            }
        );
    }

    #[test]
    fn zero_max_messages_selects_nothing_and_points_past_the_end() {
        let messages = sized(&[10, 10, 10]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 0, None);
        assert_eq!(selection.message_count, 0);
        assert_eq!(selection.start_index, 3);
        assert_eq!(selection.characters, 0);
        // The reported range is still valid over the slice.
        assert!(entries.get(selection.start_index..).is_some());
    }

    #[test]
    fn zero_max_messages_wins_over_a_generous_char_budget() {
        let messages = sized(&[10, 10]);
        let entries = branch(&messages);
        assert_eq!(select_recent(&entries, 0, Some(10_000)).message_count, 0);
    }

    #[test]
    fn message_limit_takes_the_tail() {
        let messages = sized(&[5, 5, 5, 5, 5]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 2, None);
        assert_eq!(selection.start_index, 3);
        assert_eq!(selection.message_count, 2);
        assert_eq!(selection.characters, 10);
    }

    #[test]
    fn message_limit_above_the_length_takes_everything() {
        let messages = sized(&[5, 5, 5]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 99, None);
        assert_eq!(selection.start_index, 0);
        assert_eq!(selection.message_count, 3);
    }

    #[test]
    fn char_limit_cuts_at_a_message_boundary() {
        let messages = sized(&[100, 40, 40, 40]);
        let entries = branch(&messages);
        // 40 + 40 = 80 fits, adding the third would make 120.
        let selection = select_recent(&entries, 99, Some(100));
        assert_eq!(selection.message_count, 2);
        assert_eq!(selection.characters, 80);
        assert_eq!(selection.start_index, 2);
    }

    #[test]
    fn a_message_landing_exactly_on_the_limit_is_included() {
        let messages = sized(&[10, 30, 30, 40]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 99, Some(100));
        assert_eq!(selection.characters, 100);
        assert_eq!(selection.message_count, 3);
    }

    #[test]
    fn the_stricter_of_the_two_limits_wins() {
        let messages = sized(&[20, 20, 20, 20, 20]);
        let entries = branch(&messages);
        // Message limit is stricter.
        assert_eq!(select_recent(&entries, 2, Some(1_000)).message_count, 2);
        // Char limit is stricter.
        assert_eq!(select_recent(&entries, 5, Some(50)).message_count, 2);
    }

    #[test]
    fn one_oversized_message_is_still_selected() {
        let messages = sized(&[5, 5, 500]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 10, Some(10));
        assert_eq!(selection.message_count, 1);
        assert_eq!(selection.start_index, 2);
        assert_eq!(selection.characters, 500);
    }

    #[test]
    fn character_budget_counts_graphemes_not_bytes() {
        let messages = script(&[(Role::User, "שלום עולם"), (Role::Assistant, "👨‍👩‍👧‍👦👨‍👩‍👧‍👦")]);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 10, None);
        // 9 Hebrew graphemes + 2 family emoji clusters.
        assert_eq!(selection.characters, 11);
    }
}
