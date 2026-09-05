//! Archival Markdown rendering of a conversation's active branch.
//!
//! `render` and `render_messages` are the only entry points that touch
//! [`ConversationBranch::messages`] — everything else in this module is a
//! pure function over an already-resolved `&[BranchMessage]`, which keeps the
//! formatting logic testable independently of graph traversal.

use crate::graph::{BranchMessage, ConversationBranch};
use crate::model::{Conversation, Message, Role};
use crate::text;
use crate::timefmt::{self, TimeZoneMode};

/// Placeholder line emitted when the active branch has no messages to show.
const EMPTY_BRANCH_PLACEHOLDER: &str = "_(no messages on the active branch)_";

/// Which messages a rendered transcript includes.
///
/// The default (all flags `false`) includes only [`Role::User`] and
/// [`Role::Assistant`] messages — the conversational core. Everything else
/// (system prompts, developer messages, tool calls, unrecognised roles) is
/// opt-in, and a message hidden by ChatGPT's own UI
/// ([`Message::is_hidden`]) or carrying no renderable content
/// ([`crate::model::MessageContent::is_empty`]) is never shown regardless of
/// these flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptOptions {
    /// Include `Role::System` messages.
    pub include_system: bool,
    /// Include `Role::Tool` messages and any unrecognised (`Role::Other`) role.
    pub include_tools: bool,
    /// Include `Role::Developer` messages.
    pub include_developer: bool,
    /// Include messages ChatGPT marks `is_visually_hidden_from_conversation`.
    pub include_hidden: bool,
}

impl TranscriptOptions {
    /// Whether `message` should appear in a rendered transcript under these
    /// options.
    ///
    /// Visibility and emptiness are checked independently of role: a hidden
    /// message stays out unless `include_hidden` is set, and a message with
    /// no renderable content is never shown, no matter which role flags are
    /// set.
    pub fn includes(&self, message: &Message) -> bool {
        if message.is_hidden() && !self.include_hidden {
            return false;
        }
        if message.content.is_empty() {
            return false;
        }
        match message.role() {
            Role::User | Role::Assistant => true,
            Role::System => self.include_system,
            Role::Developer => self.include_developer,
            Role::Tool | Role::Other(_) => self.include_tools,
        }
    }
}

/// Render the active branch of `conversation` as a standalone archival
/// Markdown document: a title/id/date header, a `---` separator, then one
/// `## <Role>` section per included message in branch order.
///
/// An empty (post-filter) branch still renders the header, followed by
/// [`EMPTY_BRANCH_PLACEHOLDER`]. The result always ends with exactly one
/// trailing newline and no trailing whitespace on any line.
pub fn render(
    conversation: &Conversation,
    branch: &ConversationBranch,
    options: &TranscriptOptions,
    tz: TimeZoneMode,
) -> String {
    let messages = branch.messages(conversation);
    render_document(conversation, &messages, options, tz)
}

/// Render just the bodies of the included messages whose position in the
/// **filtered** (included) list falls within `range` — no title/id/date
/// header, no `---` separator. Used for a verbatim "recent messages" tail.
///
/// `range` is clamped to the size of the filtered list rather than
/// panicking; a range that is empty or entirely out of bounds yields an
/// empty string.
pub fn render_messages(
    conversation: &Conversation,
    branch: &ConversationBranch,
    options: &TranscriptOptions,
    range: std::ops::Range<usize>,
) -> String {
    let messages = branch.messages(conversation);
    render_range(&messages, options, range)
}

/// Pure counterpart of [`render`]: builds the full document from an
/// already-resolved slice of branch messages.
fn render_document(
    conversation: &Conversation,
    messages: &[BranchMessage<'_>],
    options: &TranscriptOptions,
    tz: TimeZoneMode,
) -> String {
    let included = included_messages(messages, options);
    let title = text::collapse_whitespace(&conversation.display_title());
    let created = timefmt::format(conversation.create_time, tz);
    let updated = timefmt::format(conversation.update_time, tz);

    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&title);
    out.push_str("\n\n");
    out.push_str("Conversation ID: ");
    out.push_str(&conversation.id);
    out.push('\n');
    out.push_str("Created: ");
    out.push_str(&created);
    out.push('\n');
    out.push_str("Updated: ");
    out.push_str(&updated);
    out.push_str("\n\n---\n\n");

    if included.is_empty() {
        out.push_str(EMPTY_BRANCH_PLACEHOLDER);
    } else {
        out.push_str(&join_sections(&included));
    }

    finalize_document(&out)
}

/// Pure counterpart of [`render_messages`]: slices the filtered list from an
/// already-resolved slice of branch messages.
fn render_range(
    messages: &[BranchMessage<'_>],
    options: &TranscriptOptions,
    range: std::ops::Range<usize>,
) -> String {
    let included = included_messages(messages, options);
    let len = included.len();
    let start = range.start.min(len);
    let end = range.end.min(len);
    if start >= end {
        return String::new();
    }
    finalize_fragment(&join_sections(&included[start..end]))
}

/// Filter `messages` down to those [`TranscriptOptions::includes`] keeps,
/// preserving branch order.
fn included_messages<'a>(
    messages: &[BranchMessage<'a>],
    options: &TranscriptOptions,
) -> Vec<BranchMessage<'a>> {
    messages
        .iter()
        .copied()
        .filter(|bm| options.includes(bm.message))
        .collect()
}

/// Join rendered `## <Role>` sections with a blank line between each.
fn join_sections(messages: &[BranchMessage<'_>]) -> String {
    messages
        .iter()
        .map(render_section)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render one message as a `## <Role>` heading followed by its body.
fn render_section(bm: &BranchMessage<'_>) -> String {
    let heading = heading_for(bm.message);
    let body = bm.message.content.render_markdown();
    format!("## {heading}\n\n{body}")
}

/// The heading text for a message: the role's title, except `Role::Tool`
/// messages use the tool name (from `author.name`) when present, e.g.
/// `Tool (browser)`.
fn heading_for(message: &Message) -> String {
    if *message.role() == Role::Tool {
        if let Some(name) = message.author.name.as_deref() {
            let clean = text::collapse_whitespace(&text::sanitize_display(name));
            let clean = clean.trim();
            if !clean.is_empty() {
                let clean = text::truncate_graphemes(clean, 40);
                return format!("Tool ({clean})");
            }
        }
    }
    message.role().heading()
}

/// Normalize a full document: strip trailing whitespace from every line and
/// ensure exactly one trailing newline.
fn finalize_document(s: &str) -> String {
    let mut normalized = s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    normalized.push('\n');
    normalized
}

/// Normalize a fragment (no header/footer expectations): an effectively
/// empty fragment becomes `""`; otherwise the same rules as
/// [`finalize_document`] apply.
fn finalize_fragment(s: &str) -> String {
    if s.trim().is_empty() {
        String::new()
    } else {
        finalize_document(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn msg(value: serde_json::Value) -> Message {
        serde_json::from_value(value).expect("valid message json")
    }

    fn bm<'a>(node_id: &'a str, message: &'a Message) -> BranchMessage<'a> {
        BranchMessage { node_id, message }
    }

    fn test_conversation() -> Conversation {
        Conversation {
            id: "conv-1".to_string(),
            title: Some("Test Title".to_string()),
            create_time: Some(0.0),
            update_time: Some(0.0),
            current_node: None,
            mapping: HashMap::new(),
        }
    }

    fn text_message(role: &str, text: &str) -> Message {
        msg(json!({
            "author": {"role": role},
            "content": {"content_type": "text", "parts": [text]}
        }))
    }

    #[test]
    fn default_options_render_only_user_and_assistant_in_branch_order() {
        let user = text_message("user", "hi");
        let system = text_message("system", "sys");
        let assistant = text_message("assistant", "hello");
        let messages = vec![bm("n1", &user), bm("n2", &system), bm("n3", &assistant)];

        let included = included_messages(&messages, &TranscriptOptions::default());

        assert_eq!(
            included.iter().map(|bm| bm.node_id).collect::<Vec<_>>(),
            vec!["n1", "n3"]
        );
    }

    #[test]
    fn each_include_flag_adds_exactly_one_role() {
        let system = text_message("system", "s");
        let developer = text_message("developer", "d");
        let tool = text_message("tool", "t");
        let messages = vec![bm("s", &system), bm("d", &developer), bm("t", &tool)];

        let base = TranscriptOptions::default();
        assert!(included_messages(&messages, &base).is_empty());

        let with_system = TranscriptOptions {
            include_system: true,
            ..base
        };
        assert_eq!(
            included_messages(&messages, &with_system)
                .iter()
                .map(|bm| bm.node_id)
                .collect::<Vec<_>>(),
            vec!["s"]
        );

        let with_developer = TranscriptOptions {
            include_developer: true,
            ..base
        };
        assert_eq!(
            included_messages(&messages, &with_developer)
                .iter()
                .map(|bm| bm.node_id)
                .collect::<Vec<_>>(),
            vec!["d"]
        );

        let with_tools = TranscriptOptions {
            include_tools: true,
            ..base
        };
        assert_eq!(
            included_messages(&messages, &with_tools)
                .iter()
                .map(|bm| bm.node_id)
                .collect::<Vec<_>>(),
            vec!["t"]
        );
    }

    #[test]
    fn hidden_messages_excluded_unless_include_hidden() {
        let hidden = msg(json!({
            "author": {"role": "user"},
            "content": {"content_type": "text", "parts": ["hi"]},
            "metadata": {"is_visually_hidden_from_conversation": true}
        }));
        let messages = vec![bm("h", &hidden)];

        assert!(included_messages(&messages, &TranscriptOptions::default()).is_empty());

        let with_hidden = TranscriptOptions {
            include_hidden: true,
            ..Default::default()
        };
        assert_eq!(included_messages(&messages, &with_hidden).len(), 1);
    }

    #[test]
    fn empty_content_messages_never_appear() {
        let empty = text_message("user", "   ");
        let messages = vec![bm("e", &empty)];
        let all_on = TranscriptOptions {
            include_system: true,
            include_tools: true,
            include_developer: true,
            include_hidden: true,
        };

        assert!(included_messages(&messages, &all_on).is_empty());
    }

    #[test]
    fn image_message_renders_marker_and_text_without_leaking_pointer() {
        let m = msg(json!({
            "author": {"role": "user"},
            "content": {
                "content_type": "multimodal_text",
                "parts": [
                    {"content_type": "image_asset_pointer", "asset_pointer": "file-service://secret"},
                    "describe this"
                ]
            }
        }));

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.contains("[image attachment]"));
        assert!(rendered.contains("describe this"));
        assert!(!rendered.contains("file-service"));
    }

    #[test]
    fn unknown_content_type_renders_marker_without_raw_json() {
        let m = msg(json!({
            "author": {"role": "assistant"},
            "content": {"content_type": "sea_shanty", "verses": 12}
        }));

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.contains("[sea_shanty content omitted]"));
        assert!(!rendered.contains("verses"));
    }

    #[test]
    fn code_message_fence_defeats_hostile_backticks() {
        let m = msg(json!({
            "author": {"role": "assistant"},
            "content": {
                "content_type": "code",
                "language": "python",
                "text": "```\nnot really the end\n```"
            }
        }));

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.contains("````python"));
        assert!(rendered.trim_end().ends_with("````"));
    }

    #[test]
    fn hebrew_content_round_trips_byte_identical() {
        let hebrew = "שלום עולם, מה שלומך?";
        let m = text_message("user", hebrew);

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.contains(hebrew));
    }

    #[test]
    fn tool_message_heading_shows_tool_name() {
        let m = msg(json!({
            "author": {"role": "tool", "name": "browser"},
            "content": {"content_type": "text", "parts": ["result"]}
        }));

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.starts_with("## Tool (browser)"));
    }

    #[test]
    fn tool_message_without_name_falls_back_to_role_heading() {
        let m = text_message("tool", "result");

        let rendered = render_section(&bm("n", &m));

        assert!(rendered.starts_with("## Tool\n"));
    }

    #[test]
    fn render_range_slices_the_filtered_list() {
        let m1 = text_message("user", "one");
        let system = text_message("system", "sys");
        let m2 = text_message("assistant", "two");
        let m3 = text_message("user", "three");
        let messages = vec![bm("1", &m1), bm("s", &system), bm("2", &m2), bm("3", &m3)];
        let options = TranscriptOptions::default();

        // Filtered list is [1, 2, 3] (system dropped) — range 1..3 is [2, 3].
        let rendered = render_range(&messages, &options, 1..3);
        assert!(rendered.contains("two"));
        assert!(rendered.contains("three"));
        assert!(!rendered.contains("one"));
        assert!(!rendered.contains("sys"));
    }

    #[test]
    // `3..1` is deliberately reversed: a caller must not be able to panic us.
    #[allow(clippy::reversed_empty_ranges)]
    fn render_range_clamps_out_of_bounds_instead_of_panicking() {
        let m1 = text_message("user", "one");
        let messages = vec![bm("1", &m1)];
        let options = TranscriptOptions::default();

        assert_eq!(render_range(&messages, &options, 10..20), "");
        assert_eq!(render_range(&messages, &options, 1..1), "");
        assert_eq!(render_range(&messages, &options, 3..1), "");
    }

    #[test]
    fn document_ends_with_single_newline_and_no_trailing_whitespace() {
        let m = text_message("user", "hi  ");
        let conversation = test_conversation();
        let messages = vec![bm("n", &m)];

        let rendered = render_document(
            &conversation,
            &messages,
            &TranscriptOptions::default(),
            TimeZoneMode::Utc,
        );

        assert!(rendered.ends_with('\n'));
        assert!(!rendered.ends_with("\n\n"));
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end());
        }
    }

    #[test]
    fn document_header_has_the_documented_shape() {
        let conversation = test_conversation();
        let m = text_message("user", "hi");
        let messages = vec![bm("n", &m)];

        let rendered = render_document(
            &conversation,
            &messages,
            &TranscriptOptions::default(),
            TimeZoneMode::Utc,
        );

        assert!(rendered.starts_with("# Test Title\n\nConversation ID: conv-1\n"));
        assert!(rendered.contains("\n\n---\n\n## User\n\nhi\n"));
    }

    #[test]
    fn empty_branch_produces_placeholder() {
        let conversation = test_conversation();
        let messages: Vec<BranchMessage<'_>> = vec![];

        let rendered = render_document(
            &conversation,
            &messages,
            &TranscriptOptions::default(),
            TimeZoneMode::Utc,
        );

        assert!(rendered.contains(EMPTY_BRANCH_PLACEHOLDER));
    }

    #[test]
    fn consecutive_same_role_messages_each_get_their_own_heading() {
        let m1 = text_message("assistant", "first");
        let m2 = text_message("assistant", "second");
        let conversation = test_conversation();
        let messages = vec![bm("1", &m1), bm("2", &m2)];

        let rendered = render_document(
            &conversation,
            &messages,
            &TranscriptOptions::default(),
            TimeZoneMode::Utc,
        );

        assert_eq!(rendered.matches("## Assistant").count(), 2);
    }

    #[test]
    fn title_with_newline_and_hash_stays_on_one_line() {
        let mut conversation = test_conversation();
        conversation.title = Some("Line one\nLine # two".to_string());
        let messages: Vec<BranchMessage<'_>> = vec![];

        let rendered = render_document(
            &conversation,
            &messages,
            &TranscriptOptions::default(),
            TimeZoneMode::Utc,
        );

        let first_line = rendered.lines().next().expect("at least one line");
        assert_eq!(first_line, "# Line one Line # two");
    }
}
