//! Prompt mode: the same offline document, plus the prompt that lets any LLM
//! produce a materially better one.
//!
//! Nothing in this module calls a model or touches the network. Prompt mode
//! produces two artifacts:
//!
//! * `context.md` — byte-for-byte the
//!   [deterministic](crate::context::deterministic) document, annotated with a
//!   note saying it is the local heuristic baseline.
//! * `summarization-prompt.md` — a vendor-neutral prompt the user pastes into
//!   ChatGPT, Claude, or anything else together with `transcript.md`, whose
//!   output replaces `context.md`.
//!
//! Splitting it this way keeps the tool honest: it ships something useful
//! offline and is explicit that the better path runs through a model it did
//! not call itself.

use super::{
    ContextDocument, ContextGenerator, ContextOptions, DeterministicContextGenerator,
    SECTION_ORDER, visible_messages,
};
use crate::error::Result;
use crate::graph::{BranchMessage, ConversationBranch};
use crate::model::Conversation;
use crate::text;
use crate::timefmt;

use crate::context::template::EMPTY_SECTION_BODY;

/// Note inserted into the `Conversation` section in prompt mode.
///
/// It goes inside an existing section rather than a new one because
/// [`SECTION_ORDER`] is a contract — the document is always exactly 14
/// sections, whichever generator built it.
const PROMPT_MODE_NOTE: &str = "\
_This file is the **local heuristic baseline**. For a materially better handoff, paste \
`summarization-prompt.md` together with `transcript.md` into any capable LLM and use its output in \
place of this file._";

/// Generator that emits the local baseline document alongside a prompt for
/// producing a better one. Never calls a model.
#[derive(Debug, Clone, Copy, Default)]
pub struct PromptContextGenerator;

impl ContextGenerator for PromptContextGenerator {
    fn name(&self) -> &'static str {
        "prompt"
    }

    fn generate(
        &self,
        conversation: &Conversation,
        branch: &ConversationBranch,
        options: &ContextOptions,
    ) -> Result<ContextDocument> {
        let mut document = DeterministicContextGenerator.generate(conversation, branch, options)?;
        annotate(&mut document);
        Ok(document)
    }
}

/// Append the prompt-mode note to the document's `Conversation` section.
pub(crate) fn annotate(document: &mut ContextDocument) {
    let Some(body) = document.section("Conversation").map(|s| s.body.clone()) else {
        return;
    };
    let trimmed = body.trim_end();
    let annotated = if trimmed.is_empty() {
        PROMPT_MODE_NOTE.to_string()
    } else {
        format!("{trimmed}\n\n{PROMPT_MODE_NOTE}")
    };
    document.set_section("Conversation", annotated);
}

/// Size of the conversation, so the target model can calibrate how much
/// compression the job actually needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptStats {
    pub(crate) message_count: usize,
    pub(crate) word_count: usize,
}

impl PromptStats {
    fn from_messages(messages: &[BranchMessage<'_>]) -> Self {
        PromptStats {
            message_count: messages.len(),
            word_count: messages
                .iter()
                .map(|entry| text::word_count(&entry.message.content.plain_text()))
                .sum(),
        }
    }
}

/// Build the prompt a user pastes into any model alongside `transcript.md`.
///
/// The prompt is vendor-neutral (no system/user role markup, no
/// provider-specific syntax) and compact, because it is prepended to a
/// transcript that is often near a context limit already. The 14 required
/// headings are generated from [`SECTION_ORDER`], so the prompt can never ask
/// for a structure this crate does not itself produce.
pub fn summarization_prompt(
    conversation: &Conversation,
    branch: &ConversationBranch,
    options: &ContextOptions,
) -> String {
    let messages = visible_messages(conversation, branch, options);
    build_prompt(
        conversation,
        &PromptStats::from_messages(&messages),
        options,
    )
}

/// The prompt body, separated from graph traversal so it can be tested
/// directly against known statistics.
pub(crate) fn build_prompt(
    conversation: &Conversation,
    stats: &PromptStats,
    options: &ContextOptions,
) -> String {
    let headings = SECTION_ORDER
        .iter()
        .map(|heading| format!("## {heading}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "# Task\n\
         \n\
         You are given `transcript.md`: the complete history of a ChatGPT conversation that hit \
         its length limit. Produce a single Markdown document, `context.md`, that lets a model \
         with no memory of this conversation **continue** it.\n\
         \n\
         ## Source conversation\n\
         \n\
         - Title: {title}\n\
         - Original conversation ID: {id}\n\
         - Created: {created}\n\
         - Last updated: {updated}\n\
         - Size: {messages} messages, roughly {words} words\n\
         \n\
         ## Rules\n\
         \n\
         1. Produce a continuation-oriented context, not a generic summary. Ask of every line: \
         \"would the next turn be worse without this?\"\n\
         2. Do not summarize away critical facts. Specific names, numbers, versions, paths, \
         identifiers, and constraints survive verbatim; prose around them may be compressed.\n\
         3. Distinguish facts from speculation. State established facts plainly; mark anything \
         proposed, assumed, or unverified as such. Never promote a hypothesis into a fact.\n\
         4. Preserve every decision already made, the terminology and named entities the \
         conversation uses, and the user's stated preferences and constraints. A model that \
         reopens a settled decision or renames a settled concept has failed.\n\
         5. Preserve unresolved disagreements and open questions, including who holds which \
         position. Do not resolve them yourself.\n\
         6. Identify approaches that were tried, rejected, or superseded, and say briefly why, so \
         the next model does not propose them again.\n\
         7. Give extra weight to the most recent part of the conversation: the current state \
         matters more than the opening, and the last few turns matter most of all.\n\
         8. Do not repeat the full transcript. `context.md` is a working brief, not an archive; \
         the transcript remains available separately.\n\
         9. Emit every heading below, in this exact order, even when a section is empty. Under an \
         empty section write `{empty}` — an omitted section is indistinguishable from an \
         overlooked one.\n\
         10. Output only the document: no preamble, no commentary, no outer code fence.\n\
         \n\
         ## Required structure\n\
         \n\
         Begin with the title line `# Conversation Handoff`, then these headings in this order:\n\
         \n\
         ```\n\
         {headings}\n\
         ```\n\
         \n\
         `## Recent Conversation` holds the final exchanges reproduced closely enough that the \
         next turn can pick them up directly.\n\
         \n\
         `## Continuation Instructions` ends the document with exactly this block:\n\
         \n\
         ```\n\
         This document summarizes a previous ChatGPT conversation that reached its length limit.\n\
         Treat the information above as prior conversation context.\n\
         Do not restart the discussion from scratch.\n\
         The complete historical transcript is available in `transcript.md` and should only be \
         consulted when historical detail is required.\n\
         ```\n",
        title = text::collapse_whitespace(&conversation.display_title()),
        id = text::sanitize_display(&conversation.id),
        created = timefmt::format(conversation.create_time, options.timezone),
        updated = timefmt::format(conversation.update_time, options.timezone),
        messages = stats.message_count,
        words = stats.word_count,
        empty = EMPTY_SECTION_BODY,
        headings = headings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConversationNode;
    use std::collections::HashMap;

    fn conversation() -> Conversation {
        Conversation {
            id: "abc-123".to_string(),
            title: Some("Migrating the exporter".to_string()),
            create_time: Some(1_757_071_924.0),
            update_time: Some(1_757_171_924.0),
            current_node: None,
            mapping: HashMap::<String, ConversationNode>::new(),
        }
    }

    fn prompt() -> String {
        build_prompt(
            &conversation(),
            &PromptStats {
                message_count: 214,
                word_count: 48_300,
            },
            &ContextOptions::default(),
        )
    }

    #[test]
    fn prompt_embeds_every_section_heading_in_order() {
        let text = prompt();
        let mut cursor = 0usize;
        for heading in SECTION_ORDER {
            let needle = format!("## {heading}");
            let found = text[cursor..]
                .find(&needle)
                .unwrap_or_else(|| panic!("missing heading {heading}"));
            cursor += found + needle.len();
        }
    }

    #[test]
    fn prompt_carries_the_key_instructions() {
        let text = prompt();
        for phrase in [
            "continuation-oriented context, not a generic summary",
            "Do not summarize away critical facts",
            "Distinguish facts from speculation",
            "Preserve every decision already made",
            "terminology and named entities",
            "preferences and constraints",
            "unresolved disagreements and open questions",
            "rejected, or superseded",
            "Give extra weight to the most recent part",
            "Do not repeat the full transcript",
            EMPTY_SECTION_BODY,
            "# Conversation Handoff",
        ] {
            assert!(text.contains(phrase), "prompt is missing: {phrase}");
        }
    }

    #[test]
    fn prompt_calibrates_with_identity_and_size() {
        let text = prompt();
        assert!(text.contains("- Title: Migrating the exporter"));
        assert!(text.contains("- Original conversation ID: abc-123"));
        assert!(text.contains("- Created: 2025-09-05T11:32:04Z"));
        assert!(text.contains("- Last updated: 2025-09-06T15:18:44Z"));
        assert!(text.contains("214 messages, roughly 48300 words"));
    }

    #[test]
    fn prompt_repeats_the_mandated_continuation_block() {
        let text = prompt();
        for line in [
            "This document summarizes a previous ChatGPT conversation that reached its length limit.",
            "Treat the information above as prior conversation context.",
            "Do not restart the discussion from scratch.",
        ] {
            assert!(text.contains(line), "missing: {line}");
        }
        assert!(
            text.contains("The complete historical transcript is available in `transcript.md`")
        );
    }

    #[test]
    fn prompt_is_deterministic() {
        assert_eq!(prompt(), prompt());
    }

    #[test]
    fn prompt_is_compact() {
        // A prompt prepended to a near-limit transcript must not itself be
        // large. This is a regression guard, not a hard requirement.
        assert!(
            text::word_count(&prompt()) < 700,
            "prompt grew to {} words",
            text::word_count(&prompt())
        );
    }

    #[test]
    fn prompt_sanitizes_hostile_conversation_metadata() {
        let mut hostile = conversation();
        hostile.title = Some("evil\u{202e}title\u{7}".to_string());
        let text = build_prompt(
            &hostile,
            &PromptStats {
                message_count: 1,
                word_count: 1,
            },
            &ContextOptions::default(),
        );
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains('\u{7}'));
    }

    #[test]
    fn annotation_points_at_the_prompt_without_adding_a_section() {
        let mut document = ContextDocument::skeleton();
        document.set_section("Conversation", "- **Title:** x");
        annotate(&mut document);
        assert_eq!(document.sections.len(), SECTION_ORDER.len());
        let body = document
            .section("Conversation")
            .map(|s| s.body.clone())
            .unwrap_or_default();
        assert!(body.starts_with("- **Title:** x"));
        assert!(body.contains("local heuristic baseline"));
        assert!(body.contains("summarization-prompt.md"));
    }

    #[test]
    fn annotation_is_idempotent_in_shape() {
        let mut document = ContextDocument::skeleton();
        annotate(&mut document);
        let headings: Vec<&str> = document
            .sections
            .iter()
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(headings, SECTION_ORDER);
    }
}
