//! Local, offline generation of the 14-section handoff document.
//!
//! # What this is, and what it is not
//!
//! Every section below `Recent Conversation` is produced by **heuristic
//! extraction**: cue-phrase matching, sentence segmentation, and token
//! frequency counting over the conversation text. There is no model in this
//! path — no network call, no semantic understanding, no paraphrase. A
//! sentence appears under "Decisions Already Made" because it contained the
//! literal substring "we decided", not because anything understood that a
//! decision was made.
//!
//! That limitation is disclosed inside the generated document itself (in the
//! `Conversation` and `Continuation Instructions` sections), because the
//! document's whole job is to be read by another model that must know how much
//! to trust each part of it.
//!
//! `Recent Conversation` is the exception and the reason the tool works at
//! all: it is the verbatim tail of the conversation, cut only at message
//! boundaries.
//!
//! # Language coverage
//!
//! Cue lists carry English **and** Hebrew phrasings, and no heuristic relies on
//! capitalization as its only signal — Hebrew has no case, so a
//! capitalization-only entity extractor would silently produce nothing for
//! half of the real exports this tool is aimed at. Capitalized runs are an
//! *additional* signal on top of frequency ranking, never the sole one.

use std::collections::{BTreeMap, BTreeSet};

use unicode_segmentation::UnicodeSegmentation;

use super::{
    ContextDocument, ContextGenerator, ContextOptions, RecentSelection, recent::select_recent,
};
use crate::error::Result;
use crate::graph::{BranchMessage, ConversationBranch};
use crate::model::{Conversation, MessageContent, Role};
use crate::text;
use crate::timefmt;
use crate::transcript;

// ---------------------------------------------------------------------------
// Caps. Every extracted list is bounded in both item count and item length, so
// a pathological conversation cannot blow up the handoff document.
// ---------------------------------------------------------------------------

/// Graphemes kept from the message chosen as the conversation's purpose.
const PURPOSE_CHARS: usize = 600;
/// Graphemes kept per early-background bullet.
const BACKGROUND_CHARS: usize = 200;
/// Early user messages (after the first) quoted as background.
const MAX_BACKGROUND_MESSAGES: usize = 4;
/// Graphemes kept per extracted sentence bullet.
const SENTENCE_CHARS: usize = 240;
/// Graphemes kept per one-line message digest entry.
const DIGEST_CHARS: usize = 120;
/// Graphemes kept per "Current State" line.
const CURRENT_STATE_CHARS: usize = 180;
/// Messages summarised under "Current State".
const CURRENT_STATE_MESSAGES: usize = 5;
/// Entries in the pre-tail digest before it is evenly sampled down.
const MAX_DIGEST_ENTRIES: usize = 60;
/// Bullets per extracted-sentence section.
const MAX_SENTENCE_BULLETS: usize = 10;
/// Bullets in the conclusion / rejection / open-question sections.
const MAX_SHORT_BULLETS: usize = 8;
/// Trailing messages scanned for unanswered questions.
const OPEN_QUESTION_WINDOW: usize = 12;
/// Frequency-ranked terminology entries.
const MAX_FREQUENT_TERMS: usize = 15;
/// Backtick-quoted terms and capitalized entity runs.
const MAX_MARKED_TERMS: usize = 10;
/// Distinct messages a token must appear in to count as terminology.
const MIN_TERM_MESSAGES: usize = 3;
/// Shortest token (in graphemes) that can be terminology.
const MIN_TERM_GRAPHEMES: usize = 3;
/// Total items across all "Important Technical Details" categories.
const MAX_TECHNICAL_ITEMS: usize = 15;
/// Items taken from any single technical-detail category.
const MAX_TECHNICAL_PER_CATEGORY: usize = 5;
/// Minimum size for a sentence to count as an established fact.
const FACT_MIN_GRAPHEMES: usize = 30;
const FACT_MIN_WORDS: usize = 5;
/// Words a user message needs before it can be the stated purpose.
const PURPOSE_MIN_WORDS: usize = 3;

// ---------------------------------------------------------------------------
// Cue lists.
//
// These are deliberately plain substring cues, lowercased, apostrophe-folded,
// and extensible: add a phrase to a list and the corresponding section picks it
// up. Substring matching means "want" also fires on "wanted" and "unwanted" —
// the recall is worth the occasional false positive in a document that is
// explicitly labelled as approximate.
// ---------------------------------------------------------------------------

/// Cues for "User Preferences and Constraints".
const PREFERENCE_CUES: &[&str] = &[
    // English
    "prefer",
    "want",
    "need",
    "must",
    "should",
    "don't",
    "do not",
    "never",
    "always",
    "avoid",
    "require",
    "make sure",
    "has to",
    "have to",
    "instead please",
    "keep it",
    // Hebrew
    "רוצה",
    "לא רוצה",
    "חייב",
    "צריך",
    "תמיד",
    "אף פעם",
    "עדיף",
    "אל תוכל",
    "בלי",
    "אסור",
    "תקפיד",
];

/// Cues for "Decisions Already Made".
const DECISION_CUES: &[&str] = &[
    // English
    "decided",
    "we'll use",
    "we will use",
    "let's use",
    "lets use",
    "going with",
    "chose",
    "settled on",
    "final answer",
    "agreed",
    "we're going to",
    "the plan is",
    // Hebrew
    "החלטנו",
    "החלטתי",
    "נלך על",
    "בחרתי",
    "בחרנו",
    "סוכם",
    "הוחלט",
];

/// Cues for "Key Conclusions".
const CONCLUSION_CUES: &[&str] = &[
    // English
    "in summary",
    "to summarize",
    "to summarise",
    "therefore",
    "in conclusion",
    "the key",
    "bottom line",
    "overall",
    "which means",
    "so the answer",
    // Hebrew
    "לסיכום",
    "לכן",
    "המסקנה",
    "בשורה התחתונה",
    "כלומר",
];

/// Cues strong enough, on their own, to mark a sentence as a rejected or
/// superseded approach.
///
/// "instead of" and "rather than" deliberately live in [`CONTRAST_CUES`]
/// instead: on their own they are far more often a *preference* ("tell me
/// instead of guessing") than a record of an approach that was actually
/// dropped.
const REJECTION_CUES: &[&str] = &[
    // English
    "won't",
    "will not",
    "don't use",
    "do not use",
    "no longer",
    "we dropped",
    "abandoned",
    "doesn't work",
    "does not work",
    "didn't work",
    "did not work",
    "deprecated",
    "ruled out",
    "rejected",
    "scrapped",
    "backed out",
    "gave up on",
    // Hebrew
    "לא נשתמש",
    "לא עובד",
    "ויתרנו",
    "נזנח",
    "נדחה",
    "בוטל",
];

/// Contrast cues that mark a rejection only when paired with a
/// [`REJECTION_SUPPORT_CUES`] signal in the same sentence.
const CONTRAST_CUES: &[&str] = &["instead of", "rather than", "במקום"];

/// Past-tense or decision signals. Paired with a contrast cue these turn
/// "instead of X, Y" from a stated preference into a record of a path taken
/// and a path abandoned.
const REJECTION_SUPPORT_CUES: &[&str] = &[
    // English
    "we decided",
    "decided",
    "we went with",
    "we're using",
    "we are using",
    "we'll use",
    "we will use",
    "let's use",
    "going with",
    "switched",
    "moved to",
    "replaced",
    "we chose",
    "chose",
    "ended up",
    "we used",
    "originally",
    "previously",
    "at first",
    "used to",
    // Hebrew
    "החלטנו",
    "עברנו",
    "בחרנו",
    "השתמשנו",
    "נעשה",
    "נשתמש",
    "נלך",
    "במקור",
    "בהתחלה",
    "הוחלט",
    "סוכם",
];

/// Sentence openers that make the rest of the sentence hypothetical. A
/// conditional describes a situation that may arise, not a path the
/// conversation actually abandoned.
///
/// The trade-off is deliberate: this also drops the occasional real record
/// ("When we tried the queue it didn't work"). A false positive in
/// "Rejected / Superseded Approaches" actively misleads the reading model into
/// avoiding something nobody rejected, so the heuristic errs toward silence.
const CONDITIONAL_OPENERS: &[&str] = &[
    "if ",
    "when ",
    "whenever ",
    "unless ",
    "in case ",
    "should you",
    "suppose ",
    "אם ",
    "כאשר ",
    "כש",
    "במידה ",
    "אילו ",
];

/// Tokens too common to be terminology. Short tokens (under
/// [`MIN_TERM_GRAPHEMES`]) are dropped before this list is consulted, so it
/// only needs the longer function words.
const STOPWORDS: &[&str] = &[
    // English
    "the",
    "and",
    "but",
    "for",
    "are",
    "was",
    "were",
    "this",
    "that",
    "these",
    "those",
    "with",
    "from",
    "have",
    "has",
    "had",
    "you",
    "your",
    "our",
    "not",
    "can",
    "will",
    "would",
    "should",
    "could",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "how",
    "all",
    "any",
    "one",
    "two",
    "some",
    "more",
    "most",
    "than",
    "then",
    "they",
    "them",
    "their",
    "there",
    "here",
    "its",
    "into",
    "about",
    "just",
    "like",
    "make",
    "made",
    "use",
    "used",
    "using",
    "get",
    "got",
    "need",
    "want",
    "also",
    "only",
    "out",
    "own",
    "way",
    "well",
    "very",
    "much",
    "does",
    "did",
    "done",
    "been",
    "being",
    "over",
    "under",
    "after",
    "before",
    "both",
    "each",
    "other",
    "same",
    "such",
    "too",
    "now",
    "new",
    "see",
    "say",
    "said",
    "come",
    "take",
    "know",
    "think",
    "look",
    "first",
    "last",
    "good",
    "back",
    "work",
    "yes",
    "sure",
    "okay",
    "let",
    "may",
    "might",
    "still",
    "even",
    "because",
    "while",
    "every",
    "many",
    "less",
    "few",
    "one's",
    "thing",
    "things",
    "something",
    "anything",
    "nothing",
    "really",
    "maybe",
    "here's",
    "that's",
    "it's",
    "don't",
    "doesn't",
    "isn't",
    "aren't",
    "won't",
    "can't", // Hebrew
    "של",
    "את",
    "על",
    "לא",
    "כן",
    "זה",
    "זאת",
    "הוא",
    "היא",
    "הם",
    "הן",
    "אני",
    "אתה",
    "אנחנו",
    "יש",
    "אין",
    "אבל",
    "גם",
    "כמו",
    "אם",
    "כי",
    "מה",
    "מי",
    "איך",
    "למה",
    "מתי",
    "איפה",
    "כל",
    "רק",
    "עוד",
    "אז",
    "כך",
    "כדי",
    "אחרי",
    "לפני",
    "בין",
    "יותר",
    "פחות",
    "מאוד",
    "הזה",
    "הזאת",
    "האלה",
    "שלי",
    "שלך",
    "שלנו",
    "להיות",
    "יכול",
    "אפשר",
    "צריך",
    "בגלל",
    "למרות",
    "אולי",
    "ואז",
    "כלומר",
];

/// First words that make a line look like a shell command.
const COMMAND_HEADS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "docker",
    "kubectl",
    "make",
    "python",
    "python3",
    "pip",
    "pip3",
    "curl",
    "wget",
    "sudo",
    "brew",
    "apt",
    "go",
    "rustc",
    "rustup",
    "sed",
    "awk",
    "grep",
    "rg",
    "ls",
    "cd",
    "mkdir",
    "rm",
    "cp",
    "mv",
    "chmod",
    "ssh",
    "scp",
    "tar",
    "psql",
    "terraform",
    "ansible",
    "systemctl",
    "journalctl",
];

/// Extensions that make a bare token look like a file rather than prose.
const FILE_EXTENSIONS: &[&str] = &[
    ".rs", ".toml", ".json", ".md", ".py", ".ts", ".tsx", ".js", ".jsx", ".yaml", ".yml", ".go",
    ".sh", ".txt", ".html", ".css", ".sql", ".lock", ".c", ".h", ".cpp", ".java", ".rb", ".php",
    ".xml", ".ini", ".cfg", ".env",
];

/// The four-line block the spec mandates at the end of every handoff document.
const CONTINUATION_BLOCK: &str = "\
This document summarizes a previous ChatGPT conversation that reached its length limit.
Treat the information above as prior conversation context.
Do not restart the discussion from scratch.
The complete historical transcript is available in `transcript.md` and should only be consulted when historical detail is required.";

/// Disclosure appended to `Continuation Instructions`.
const HEURISTIC_DISCLOSURE: &str = "\
Every section above `Recent Conversation` was produced by local heuristic extraction — cue-phrase \
matching and token-frequency counting over the conversation text — not by semantic summarization. \
Treat those sections as approximate pointers, and prefer `Recent Conversation` (verbatim) and \
`transcript.md` wherever they disagree.";

/// Shorter form of the same disclosure, placed near the top of the document.
const HEURISTIC_NOTE: &str = "\
_Sections below were produced by local heuristic extraction, not semantic summarization. \
`Recent Conversation` is verbatim; everything else is approximate._";

/// Offline, model-free handoff-context generator.
///
/// See the [module documentation](self) for exactly which parts of the output
/// are heuristic and which are verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicContextGenerator;

impl ContextGenerator for DeterministicContextGenerator {
    fn name(&self) -> &'static str {
        "deterministic"
    }

    fn generate(
        &self,
        conversation: &Conversation,
        branch: &ConversationBranch,
        options: &ContextOptions,
    ) -> Result<ContextDocument> {
        // Heuristics run over the same filtered message list the transcript
        // renderer uses, so the indices in `RecentSelection` address the same
        // messages `render_messages` will emit.
        let visible: Vec<BranchMessage<'_>> = branch
            .messages(conversation)
            .into_iter()
            .filter(|entry| options.transcript.includes(entry.message))
            .collect();

        let selection = select_recent(&visible, options.recent_messages, options.recent_chars);
        let range = selection.start_index..selection.start_index + selection.message_count;
        let recent_markdown =
            transcript::render_messages(conversation, branch, &options.transcript, range);

        Ok(build_document(
            conversation,
            &visible,
            &selection,
            &recent_markdown,
            options,
        ))
    }
}

/// Assemble the document from an already-filtered message slice.
///
/// Split out from [`ContextGenerator::generate`] so the heuristics can be
/// exercised against a synthetic message slice without a real conversation
/// graph or transcript renderer. `recent_markdown` is the verbatim tail as
/// rendered by [`crate::transcript::render_messages`].
pub(crate) fn build_document(
    conversation: &Conversation,
    messages: &[BranchMessage<'_>],
    selection: &RecentSelection,
    recent_markdown: &str,
    options: &ContextOptions,
) -> ContextDocument {
    let mut document = ContextDocument::skeleton();

    document.set_section(
        "Conversation",
        conversation_section(conversation, messages, options),
    );
    document.set_section("Purpose", purpose_section(messages));
    document.set_section(
        "Important Background",
        background_section(messages, selection),
    );
    document.set_section("Established Facts", facts_section(messages));
    // Preferences are computed once: the rejection heuristic needs them so the
    // same sentence cannot be filed under two contradictory headings.
    let preferences = preference_items(messages);
    document.set_section("User Preferences and Constraints", bullets(&preferences));
    document.set_section("Decisions Already Made", decisions_section(messages));
    document.set_section("Terminology and Entities", terminology_section(messages));
    document.set_section("Important Technical Details", technical_section(messages));
    document.set_section("Key Conclusions", conclusions_section(messages));
    document.set_section(
        "Rejected / Superseded Approaches",
        rejections_section(messages, &preferences),
    );
    document.set_section("Current State", current_state_section(messages));
    document.set_section("Open Questions", open_questions_section(messages));
    document.set_section(
        "Recent Conversation",
        recent_section(selection, messages.len(), recent_markdown),
    );
    document.set_section(
        "Continuation Instructions",
        format!("{CONTINUATION_BLOCK}\n\n{HEURISTIC_DISCLOSURE}"),
    );

    document
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// Conversation identity, taken straight from the export metadata (not a
/// heuristic), plus the up-front honesty note about everything that follows.
fn conversation_section(
    conversation: &Conversation,
    messages: &[BranchMessage<'_>],
    options: &ContextOptions,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "- **Title:** {}\n",
        clean(&conversation.display_title())
    ));
    out.push_str(&format!(
        "- **Original conversation ID:** {}\n",
        clean(&conversation.id)
    ));
    out.push_str(&format!(
        "- **Created:** {}\n",
        timefmt::format(conversation.create_time, options.timezone)
    ));
    out.push_str(&format!(
        "- **Last updated:** {}\n",
        timefmt::format(conversation.update_time, options.timezone)
    ));
    out.push_str(&format!(
        "- **Messages on the active branch:** {}\n",
        messages.len()
    ));
    out.push('\n');
    out.push_str(HEURISTIC_NOTE);
    out
}

/// The first substantive user message, on the assumption that people open a
/// conversation by stating what they want. Falls back to the first user
/// message of any length before giving up.
fn purpose_section(messages: &[BranchMessage<'_>]) -> String {
    let user_texts: Vec<String> = messages
        .iter()
        .filter(|entry| *entry.message.role() == Role::User)
        .map(|entry| clean(&entry.message.content.plain_text()))
        .filter(|body| !body.is_empty())
        .collect();

    let chosen = user_texts
        .iter()
        .find(|body| text::word_count(body) >= PURPOSE_MIN_WORDS)
        .or_else(|| user_texts.first());

    match chosen {
        None => String::new(),
        Some(body) => clipped(body, PURPOSE_CHARS),
    }
}

/// Early user turns, plus a one-line-per-message digest of everything that
/// happens before the verbatim tail so that no part of the conversation is
/// dropped without trace.
fn background_section(messages: &[BranchMessage<'_>], selection: &RecentSelection) -> String {
    let mut blocks: Vec<String> = Vec::new();

    // Only look before the verbatim tail: repeating a message that appears in
    // full under `Recent Conversation` wastes the reader's budget. When the
    // tail covers everything, fall back to the whole branch so the section is
    // never misleadingly empty.
    let head = match messages.get(..selection.start_index) {
        Some(head) if !head.is_empty() => head,
        _ => messages,
    };
    let early: Vec<String> = head
        .iter()
        .filter(|entry| *entry.message.role() == Role::User)
        .map(|entry| clean(&entry.message.content.plain_text()))
        .filter(|body| !body.is_empty())
        .skip(1)
        .take(MAX_BACKGROUND_MESSAGES)
        .map(|body| clipped(&body, BACKGROUND_CHARS))
        .collect();
    if !early.is_empty() {
        blocks.push(bullets(&dedup_capped(early, MAX_BACKGROUND_MESSAGES)));
    }

    if let Some(digest) = pre_tail_digest(messages, selection) {
        blocks.push(digest);
    }

    blocks.join("\n\n")
}

/// One line per pre-tail message, evenly sampled down when there are too many.
///
/// Returns `None` when the verbatim tail already covers the whole branch.
fn pre_tail_digest(messages: &[BranchMessage<'_>], selection: &RecentSelection) -> Option<String> {
    let head = messages.get(..selection.start_index)?;
    if head.is_empty() {
        return None;
    }

    let picked = sample_indices(head.len(), MAX_DIGEST_ENTRIES);
    let lines: Vec<String> = picked
        .iter()
        .filter_map(|index| head.get(*index))
        .map(|entry| digest_line(entry, DIGEST_CHARS))
        .collect();

    let mut out = format!(
        "**Earlier conversation digest** (the {} message(s) before the verbatim tail):\n\n",
        head.len()
    );
    out.push_str(&bullets(&lines));
    if picked.len() < head.len() {
        out.push_str(&format!(
            "\n\n_Digest sampled down to {} of {} pre-tail messages; the full history is in `transcript.md`._",
            picked.len(),
            head.len()
        ));
    }
    Some(out)
}

/// Declarative user statements: no question mark, long enough to carry a fact,
/// and not already claimed by the preference or decision sections.
fn facts_section(messages: &[BranchMessage<'_>]) -> String {
    let candidates = role_sentences(messages, |role| *role == Role::User)
        .into_iter()
        .filter(|(_, sentence)| !sentence.contains('?'))
        .filter(|(_, sentence)| {
            text::grapheme_count(sentence) >= FACT_MIN_GRAPHEMES
                && text::word_count(sentence) >= FACT_MIN_WORDS
        })
        .filter(|(_, sentence)| {
            let folded = fold(sentence);
            !contains_any(&folded, PREFERENCE_CUES) && !contains_any(&folded, DECISION_CUES)
        })
        .map(|(_, sentence)| clipped(&sentence, SENTENCE_CHARS));

    bullets(&dedup_capped(candidates, MAX_SENTENCE_BULLETS))
}

/// User sentences carrying an explicit preference or constraint cue.
fn preference_items(messages: &[BranchMessage<'_>]) -> Vec<String> {
    cue_items(
        messages,
        |role| *role == Role::User,
        PREFERENCE_CUES,
        MAX_SENTENCE_BULLETS,
    )
}

/// Decision cues from either side — the assistant proposing "let's use X" and
/// the user replying "we decided on X" are equally load-bearing.
fn decisions_section(messages: &[BranchMessage<'_>]) -> String {
    cue_bullets(messages, |_| true, DECISION_CUES, MAX_SENTENCE_BULLETS)
}

/// Frequency-ranked vocabulary, plus backtick-quoted terms and capitalized
/// multi-word runs.
///
/// Ranking is frequency-first *by design*: capitalization is not portable
/// across the languages this tool handles, so it is only ever an additional
/// signal layered on top.
fn terminology_section(messages: &[BranchMessage<'_>]) -> String {
    let mut lines: Vec<String> = Vec::new();

    let frequent = frequent_terms(messages);
    if !frequent.is_empty() {
        lines.push(format!("- **Frequent terms:** {}", frequent.join(", ")));
    }

    let quoted = quoted_terms(messages);
    if !quoted.is_empty() {
        let rendered: Vec<String> = quoted.iter().map(|term| format!("`{term}`")).collect();
        lines.push(format!("- **Quoted terms:** {}", rendered.join(", ")));
    }

    let named = capitalized_runs(messages);
    if !named.is_empty() {
        lines.push(format!("- **Named entities:** {}", named.join(", ")));
    }

    lines.join("\n")
}

/// Code languages, paths, commands, links and versions seen anywhere on the
/// branch. Purely lexical: nothing here is checked for existence or validity.
fn technical_section(messages: &[BranchMessage<'_>]) -> String {
    let mut languages: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let mut commands: Vec<String> = Vec::new();
    let mut urls: Vec<String> = Vec::new();
    let mut versions: Vec<String> = Vec::new();

    for entry in messages {
        if let MessageContent::Code {
            language: Some(language),
            ..
        } = &entry.message.content
        {
            languages.push(clean(language));
        }

        let body = entry.message.content.plain_text();
        for line in body.lines() {
            let trimmed = line.trim();
            if let Some(info) = trimmed.strip_prefix("```") {
                let language = info.trim();
                if !language.is_empty() && text::grapheme_count(language) <= 20 {
                    languages.push(clean(language));
                }
                continue;
            }
            if let Some(command) = shell_command(trimmed) {
                commands.push(clipped(&clean(&command), DIGEST_CHARS));
            }
            for token in trimmed.split_whitespace() {
                let token = trim_token(token);
                if token.is_empty() {
                    continue;
                }
                if token.starts_with("http://") || token.starts_with("https://") {
                    urls.push(clipped(&clean(token), DIGEST_CHARS));
                } else if looks_like_path(token) {
                    paths.push(clean(token));
                } else if looks_like_version(token) {
                    versions.push(clean(token));
                }
            }
        }
    }

    let categories: [(&str, Vec<String>); 5] = [
        ("Code languages", languages),
        ("Files and paths", paths),
        ("Commands", commands),
        ("Links", urls),
        ("Versions", versions),
    ];

    let mut budget = MAX_TECHNICAL_ITEMS;
    let mut lines: Vec<String> = Vec::new();
    for (label, items) in categories {
        if budget == 0 {
            break;
        }
        let take = MAX_TECHNICAL_PER_CATEGORY.min(budget);
        let kept = dedup_capped(items, take);
        if kept.is_empty() {
            continue;
        }
        budget -= kept.len();
        lines.push(format!("- **{label}:** {}", kept.join(", ")));
    }

    lines.join("\n")
}

/// Assistant sentences carrying a conclusion cue, weighted toward the end of
/// the conversation where the settled answers live.
fn conclusions_section(messages: &[BranchMessage<'_>]) -> String {
    let midpoint = messages.len() / 2;
    let candidates = role_sentences(messages, |role| *role == Role::Assistant)
        .into_iter()
        .filter(|(_, sentence)| !sentence.contains('?'))
        .filter(|(_, sentence)| contains_any(&fold(sentence), CONCLUSION_CUES));

    // Stable partition: later-half hits first, each half in chronological
    // order. `sort_by_key` is stable, so equal keys keep their input order.
    let mut ordered: Vec<(usize, String)> = candidates.collect();
    ordered.sort_by_key(|(index, _)| usize::from(*index < midpoint));

    bullets(&dedup_capped(
        ordered
            .into_iter()
            .map(|(_, sentence)| clipped(&sentence, SENTENCE_CHARS)),
        MAX_SHORT_BULLETS,
    ))
}

/// Sentences recording an approach that was actually dropped, from either side.
///
/// `already_claimed` holds the bullets already filed under "User Preferences
/// and Constraints"; a sentence never appears in both, because a stated
/// preference and an abandoned approach are contradictory readings of the same
/// line and the reading model would have no way to tell which was meant.
fn rejections_section(messages: &[BranchMessage<'_>], already_claimed: &[String]) -> String {
    let claimed: BTreeSet<String> = already_claimed
        .iter()
        .map(|item| fold(item.as_str()))
        .collect();
    let candidates = role_sentences(messages, |_| true)
        .into_iter()
        .filter(|(_, sentence)| !sentence.contains('?'))
        .filter(|(_, sentence)| is_rejection(sentence))
        .map(|(_, sentence)| clipped(&sentence, SENTENCE_CHARS))
        .filter(|sentence| !claimed.contains(&fold(sentence)));
    bullets(&dedup_capped(candidates, MAX_SHORT_BULLETS))
}

/// Whether a sentence records a rejected or superseded approach.
///
/// A bare contrast ("instead of") is not enough: it needs a past-tense or
/// decision signal alongside it. Hypothetical sentences are excluded outright.
fn is_rejection(sentence: &str) -> bool {
    let folded = fold(sentence);
    if CONDITIONAL_OPENERS
        .iter()
        .any(|opener| folded.starts_with(opener))
    {
        return false;
    }
    if contains_any(&folded, REJECTION_CUES) {
        return true;
    }
    contains_any(&folded, CONTRAST_CUES) && contains_any(&folded, REJECTION_SUPPORT_CUES)
}

/// One line per message for the last handful of turns — where the
/// conversation actually stands right now.
fn current_state_section(messages: &[BranchMessage<'_>]) -> String {
    let start = messages.len().saturating_sub(CURRENT_STATE_MESSAGES);
    let lines: Vec<String> = messages
        .get(start..)
        .unwrap_or_default()
        .iter()
        .map(|entry| digest_line(entry, CURRENT_STATE_CHARS))
        .collect();
    bullets(&lines)
}

/// Question sentences from the tail of the conversation.
///
/// A question in the final user message of a conversation that ends on that
/// message is the single most likely thing to still be unanswered, so it is
/// promoted to the top of the list.
fn open_questions_section(messages: &[BranchMessage<'_>]) -> String {
    let start = messages.len().saturating_sub(OPEN_QUESTION_WINDOW);
    let window = messages.get(start..).unwrap_or_default();

    let mut promoted: Vec<String> = Vec::new();
    if let Some(last) = messages.last() {
        if *last.message.role() == Role::User {
            promoted.extend(question_sentences(&last.message.content.plain_text()));
        }
    }

    let mut trailing: Vec<String> = Vec::new();
    for entry in window {
        trailing.extend(question_sentences(&entry.message.content.plain_text()));
    }

    let candidates = promoted
        .into_iter()
        .chain(trailing)
        .map(|sentence| clipped(&sentence, SENTENCE_CHARS));
    bullets(&dedup_capped(candidates, MAX_SHORT_BULLETS))
}

/// The verbatim tail, introduced by a line saying exactly how much of the
/// conversation it covers.
fn recent_section(selection: &RecentSelection, total: usize, recent_markdown: &str) -> String {
    if selection.message_count == 0 {
        return "_No messages were preserved verbatim._".to_string();
    }
    let header = format!(
        "_Preserved verbatim: the last {} of {} message(s) on the active branch ({} characters). \
         Unlike every section above, this is the conversation itself, not an extraction._",
        selection.message_count, total, selection.characters
    );
    let demoted = demote_headings(recent_markdown);
    let body = demoted.trim();
    if body.is_empty() {
        header
    } else {
        format!("{header}\n\n{body}")
    }
}

/// Push every ATX heading in the rendered tail down to level 3 so the tail
/// nests inside `## Recent Conversation`.
///
/// [`crate::transcript::render_messages`] emits `## User` / `## Assistant`,
/// which is right for a standalone `transcript.md` but wrong once embedded: an
/// `##` heading inside an `##` section *terminates* that section for anything
/// reading the document as a tree, and this document's entire audience reads it
/// as a tree. Levels 1 and 2 both become level 3; level 3 and deeper are
/// already nested and are left alone. The transcript renderer is not changed —
/// its own output is correct for its own file.
///
/// Headings inside fenced code blocks are message *content*, not structure, and
/// are never rewritten: a conversation about Markdown routinely quotes a
/// literal `## User` line inside a fence.
fn demote_headings(markdown: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut open_fence: Option<(char, usize)> = None;

    for line in markdown.lines() {
        let marker = fence_marker(line);
        match open_fence {
            Some((open_char, open_len)) => {
                // Only a bare, matching, long-enough fence closes the block.
                if let Some((fence_char, fence_len, bare)) = marker {
                    if bare && fence_char == open_char && fence_len >= open_len {
                        open_fence = None;
                    }
                }
                lines.push(line.to_string());
            }
            None => match marker {
                Some((fence_char, fence_len, _)) => {
                    open_fence = Some((fence_char, fence_len));
                    lines.push(line.to_string());
                }
                None => lines.push(demote_heading_line(line)),
            },
        }
    }
    lines.join("\n")
}

/// `(fence character, run length, nothing follows the run)` for a fence line.
fn fence_marker(line: &str) -> Option<(char, usize, bool)> {
    let trimmed = line.trim_start();
    let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = trimmed.chars().take_while(|c| *c == fence_char).count();
    if run < 3 {
        return None;
    }
    let rest = trimmed.get(run..).unwrap_or("");
    Some((fence_char, run, rest.trim().is_empty()))
}

/// `# h` and `## h` become `### h`; anything else is returned unchanged.
fn demote_heading_line(line: &str) -> String {
    for prefix in ["## ", "# "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return format!("### {rest}");
        }
    }
    line.to_string()
}

// ---------------------------------------------------------------------------
// Shared heuristic plumbing
// ---------------------------------------------------------------------------

/// Collect `(message index, sentence)` pairs for messages matching `role_pred`.
///
/// Sentences come from [`crate::text::sentences`] (UAX #29), which segments
/// Hebrew as correctly as English, then are sanitized and whitespace-collapsed
/// so a bullet can never break the document layout.
fn role_sentences(
    messages: &[BranchMessage<'_>],
    role_pred: impl Fn(&Role) -> bool,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (index, entry) in messages.iter().enumerate() {
        if !role_pred(entry.message.role()) {
            continue;
        }
        let body = entry.message.content.plain_text();
        for sentence in text::sentences(&body) {
            let cleaned = clean(sentence);
            if !cleaned.is_empty() {
                out.push((index, cleaned));
            }
        }
    }
    out
}

/// Bullets for every sentence from a matching role that carries one of `cues`.
fn cue_bullets(
    messages: &[BranchMessage<'_>],
    role_pred: impl Fn(&Role) -> bool,
    cues: &[&str],
    cap: usize,
) -> String {
    bullets(&cue_items(messages, role_pred, cues, cap))
}

/// The deduplicated sentence list behind [`cue_bullets`].
fn cue_items(
    messages: &[BranchMessage<'_>],
    role_pred: impl Fn(&Role) -> bool,
    cues: &[&str],
    cap: usize,
) -> Vec<String> {
    let candidates = role_sentences(messages, role_pred)
        .into_iter()
        // A question is never a stated preference, decision or rejection, even
        // when it happens to contain the cue word ("what should we do?").
        .filter(|(_, sentence)| !sentence.contains('?'))
        .filter(|(_, sentence)| contains_any(&fold(sentence), cues))
        .map(|(_, sentence)| clipped(&sentence, SENTENCE_CHARS));
    dedup_capped(candidates, cap)
}

/// Question sentences of a message body, sanitized and collapsed.
fn question_sentences(body: &str) -> Vec<String> {
    text::sentences(body)
        .filter(|sentence| sentence.contains('?'))
        .map(clean)
        .filter(|sentence| !sentence.is_empty())
        .collect()
}

/// `- **Role:** text` for a single message.
fn digest_line(entry: &BranchMessage<'_>, max: usize) -> String {
    let body = clean(&entry.message.content.plain_text());
    let body = if body.is_empty() {
        "(no text content)".to_string()
    } else {
        clipped(&body, max)
    };
    format!("**{}:** {}", entry.message.role().heading(), body)
}

/// Tokens that recur across at least [`MIN_TERM_MESSAGES`] distinct messages.
///
/// Ranked by distinct-message count, then total occurrences, then the token
/// itself — a total order, so the output is byte-identical across runs.
fn frequent_terms(messages: &[BranchMessage<'_>]) -> Vec<String> {
    let mut stats: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for entry in messages {
        let body = fold(&entry.message.content.plain_text());
        let mut seen_here: BTreeSet<&str> = BTreeSet::new();
        for token in body.unicode_words() {
            if !is_term_candidate(token) {
                continue;
            }
            let counters = stats.entry(token.to_string()).or_insert((0, 0));
            counters.1 += 1;
            if seen_here.insert(token) {
                counters.0 += 1;
            }
        }
    }

    let mut ranked: Vec<(String, usize, usize)> = stats
        .into_iter()
        .filter(|(_, (messages_seen, _))| *messages_seen >= MIN_TERM_MESSAGES)
        .map(|(token, (messages_seen, total))| (token, messages_seen, total))
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(right.2.cmp(&left.2))
            .then(left.0.cmp(&right.0))
    });
    // Dedupe after ranking so a plural variant cannot occupy a slot its
    // higher-ranked singular already holds.
    dedup_capped_terms(
        ranked.into_iter().map(|(token, _, _)| token),
        MAX_FREQUENT_TERMS,
    )
}

/// Whether a folded token can be terminology: long enough, not a stopword, not
/// a bare number.
fn is_term_candidate(token: &str) -> bool {
    text::grapheme_count(token) >= MIN_TERM_GRAPHEMES
        && !STOPWORDS.contains(&token)
        && token.chars().any(|c| !c.is_numeric())
}

/// Backtick-quoted spans. Odd-indexed pieces of a backtick split are the
/// insides of single-backtick spans; multi-backtick fences degrade into empty
/// pieces, which the length filter drops.
fn quoted_terms(messages: &[BranchMessage<'_>]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for entry in messages {
        let body = entry.message.content.plain_text();
        let pieces: Vec<&str> = body.split('`').collect();
        let mut index = 1;
        while index < pieces.len() {
            let candidate = pieces[index].trim();
            let length = text::grapheme_count(candidate);
            if !candidate.contains('\n') && (1..=60).contains(&length) {
                found.push(clean(candidate));
            }
            index += 2;
        }
    }
    dedup_capped_exact(found, MAX_MARKED_TERMS)
}

/// Runs of two or more consecutive capitalized words, which in English usually
/// mark a proper noun.
///
/// Runs starting at the first word of a sentence are skipped: sentence-initial
/// capitalization carries no information. This heuristic contributes nothing
/// for Hebrew, which is exactly why it is layered on top of frequency ranking
/// rather than used alone.
fn capitalized_runs(messages: &[BranchMessage<'_>]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for entry in messages {
        let body = entry.message.content.plain_text();
        for sentence in text::sentences(&body) {
            let words: Vec<&str> = sentence.split_whitespace().map(trim_token).collect();
            let mut position = 0usize;
            while position < words.len() {
                if !is_capitalized(words[position]) {
                    position += 1;
                    continue;
                }
                let start = position;
                while position < words.len() && is_capitalized(words[position]) {
                    position += 1;
                }
                if position - start >= 2 && start > 0 {
                    let run = words.get(start..position).unwrap_or_default().join(" ");
                    found.push(clipped(&clean(&run), 60));
                }
            }
        }
    }
    dedup_capped_terms(found, MAX_MARKED_TERMS)
}

fn is_capitalized(word: &str) -> bool {
    word.chars().next().is_some_and(char::is_uppercase)
}

/// The line, if its first word looks like a shell command (or it is prefixed
/// with a `$` prompt).
fn shell_command(line: &str) -> Option<String> {
    let body = line.strip_prefix("$ ").unwrap_or(line).trim();
    if body.is_empty() {
        return None;
    }
    let head = body.split_whitespace().next()?;
    COMMAND_HEADS
        .contains(&head)
        .then(|| body.to_string())
        .filter(|command| body.split_whitespace().count() >= 2 && !command.ends_with('.'))
}

/// A token that reads as a filesystem path rather than prose.
fn looks_like_path(token: &str) -> bool {
    let length = text::grapheme_count(token);
    if !(3..=120).contains(&length) {
        return false;
    }
    if token.contains('/') && !token.ends_with('/') && token.chars().any(char::is_alphanumeric) {
        return true;
    }
    let lowered = token.to_lowercase();
    FILE_EXTENSIONS.iter().any(|ext| lowered.ends_with(ext))
}

/// A dotted numeric token such as `1.85` or `v0.4.2`.
fn looks_like_version(token: &str) -> bool {
    let body = token.strip_prefix(['v', 'V']).unwrap_or(token);
    if !body.starts_with(|c: char| c.is_ascii_digit()) || body.ends_with('.') {
        return false;
    }
    if !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    body.split('.').filter(|part| !part.is_empty()).count() >= 2
}

/// Strip surrounding punctuation and markup from a token picked out of prose.
///
/// A trailing `.` is stripped too: at the end of a token it is sentence
/// punctuation far more often than part of the token. *Leading* dots are
/// preserved, so `./src` and `.env` survive, and a real extension survives too
/// — `src/export/json.rs.` trims to `src/export/json.rs`, not `src/export/json`.
/// Stripping happens before deduplication so `Rust CLIs.` and `Rust CLI` do not
/// both reach the output.
fn trim_token(token: &str) -> &str {
    let mut current = token;
    loop {
        let trimmed = current
            .trim_matches(is_boundary_punctuation)
            .trim_end_matches(['.', '…']);
        if trimmed.len() == current.len() {
            return trimmed;
        }
        current = trimmed;
    }
}

fn is_boundary_punctuation(c: char) -> bool {
    matches!(
        c,
        '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '!' | '?' | '*'
    )
}

/// Evenly spaced indices into `len` items, at most `cap` of them.
///
/// Even sampling (rather than head or tail truncation) keeps the digest
/// representative of the whole pre-tail span instead of just its beginning.
fn sample_indices(len: usize, cap: usize) -> Vec<usize> {
    if len == 0 || cap == 0 {
        return Vec::new();
    }
    if len <= cap {
        return (0..len).collect();
    }
    if cap == 1 {
        return vec![0];
    }
    (0..cap).map(|step| step * (len - 1) / (cap - 1)).collect()
}

/// Sanitize for display and collapse all whitespace to single spaces.
fn clean(input: &str) -> String {
    text::collapse_whitespace(&text::sanitize_display(input))
}

/// Truncate by grapheme clusters. The only truncation primitive used here —
/// byte slicing would corrupt Hebrew and emoji alike.
fn clipped(input: &str, max: usize) -> String {
    text::truncate_graphemes(input, max).into_owned()
}

/// Lowercase and fold typographic apostrophes, so `don't` and `don’t` both
/// match the `don't` cue.
fn fold(input: &str) -> String {
    input.to_lowercase().replace('\u{2019}', "'")
}

fn contains_any(folded: &str, cues: &[&str]) -> bool {
    cues.iter().any(|cue| folded.contains(cue))
}

/// Case-insensitively deduplicate, dropping blanks, keeping the first spelling
/// of each entry and stopping at `cap`.
fn dedup_capped(candidates: impl IntoIterator<Item = String>, cap: usize) -> Vec<String> {
    dedup_with(candidates, cap, fold)
}

/// As [`dedup_capped`] but folding a single English plural on the final word,
/// so `Rust CLI` and `Rust CLIs` are one entity rather than two.
fn dedup_capped_terms(candidates: impl IntoIterator<Item = String>, cap: usize) -> Vec<String> {
    dedup_with(candidates, cap, term_key)
}

/// Dedupe key for terminology and entity names: case-folded, with one trailing
/// `s` removed from the last word when at least three graphemes remain (so
/// `bus` stays `bus`). Hebrew is unaffected — it has no `s` plural.
fn term_key(value: &str) -> String {
    let folded = fold(value);
    match folded.rsplit_once(' ') {
        Some((head, last)) => format!("{head} {}", singular(last)),
        None => singular(&folded),
    }
}

fn singular(word: &str) -> String {
    match word.strip_suffix('s') {
        Some(stem) if text::grapheme_count(stem) >= 3 => stem.to_string(),
        _ => word.to_string(),
    }
}

/// As [`dedup_capped`] but case-*sensitive*, for identifiers where `Foo` and
/// `foo` are genuinely different things.
fn dedup_capped_exact(candidates: impl IntoIterator<Item = String>, cap: usize) -> Vec<String> {
    dedup_with(candidates, cap, |value| value.to_string())
}

fn dedup_with(
    candidates: impl IntoIterator<Item = String>,
    cap: usize,
    key: impl Fn(&str) -> String,
) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for candidate in candidates {
        if out.len() >= cap {
            break;
        }
        if candidate.trim().is_empty() {
            continue;
        }
        if seen.insert(key(&candidate)) {
            out.push(candidate);
        }
    }
    out
}

/// Render items as a Markdown bullet list; an empty list renders as an empty
/// body, which [`ContextDocument::render_markdown`] turns into an explicit
/// "none identified" marker.
fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::recent::fixtures::{branch, script};
    use super::*;
    use crate::context::template::{EMPTY_SECTION_BODY, SECTION_ORDER};
    use crate::model::{ConversationNode, Message};
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

    fn english() -> Vec<Message> {
        script(&[
            (
                Role::User,
                "I need to migrate our exporter from the legacy pipeline to the new one. \
                 The exporter currently writes CSV files every night.",
            ),
            (
                Role::Assistant,
                "The exporter can move to Parquet. Overall the migration is straightforward.",
            ),
            (
                Role::User,
                "I prefer Parquet over CSV. Please avoid adding new dependencies to the exporter.",
            ),
            (
                Role::Assistant,
                "We decided to go with Parquet. Instead of the nightly cron we will use a queue.\n\
                 Run this from `exporter/`:\n\
                 cargo build --release\n\
                 Then edit src/exporter/main.rs first.",
            ),
            (
                Role::User,
                "The exporter runs on version 1.85 of the toolchain in production. \
                 See https://example.com/docs for the Acme Exporter spec.",
            ),
            (
                Role::Assistant,
                "In summary the exporter migration needs a queue and Parquet output.",
            ),
            (
                Role::User,
                "What should we do about the nightly cron job that still runs?",
            ),
        ])
    }

    fn hebrew() -> Vec<Message> {
        script(&[
            (
                Role::User,
                "אני צריך להעביר את המערכת שלנו לשרת חדש. המערכת רצה כרגע על שרת ישן מאוד.",
            ),
            (
                Role::Assistant,
                "אפשר להעביר את המערכת בשלבים. המערכת תמשיך לעבוד בזמן ההעברה.",
            ),
            (
                Role::User,
                "אני רוצה שההעברה תהיה בלי השבתה. חייב להיות גיבוי מלא לפני ההעברה.",
            ),
            (
                Role::Assistant,
                "החלטנו לעשות את ההעברה בסופשבוע. במקום העברה מלאה נעשה העברה הדרגתית.",
            ),
            (
                Role::Assistant,
                "לסיכום ההעברה תתבצע בשלבים עם גיבוי מלא של המערכת.",
            ),
            (Role::User, "מתי בדיוק נתחיל את ההעברה של המערכת?"),
        ])
    }

    fn document_for(messages: &[Message]) -> ContextDocument {
        let entries = branch(messages);
        let options = ContextOptions::default();
        let selection = select_recent(&entries, 3, None);
        build_document(
            &conversation(),
            &entries,
            &selection,
            "## User\n\nverbatim tail\n",
            &options,
        )
    }

    fn body_of(document: &ContextDocument, heading: &str) -> String {
        document
            .section(heading)
            .map(|section| section.body.clone())
            .unwrap_or_default()
    }

    // -- document shape ----------------------------------------------------

    #[test]
    fn all_fourteen_sections_are_present_in_order() {
        let messages = english();
        let document = document_for(&messages);
        let headings: Vec<&str> = document
            .sections
            .iter()
            .map(|section| section.heading.as_str())
            .collect();
        assert_eq!(headings, SECTION_ORDER);
    }

    #[test]
    fn sections_without_findings_say_so_explicitly() {
        // A conversation with nothing to extract still yields every section.
        let messages = script(&[(Role::User, "hi"), (Role::Assistant, "hello")]);
        let document = document_for(&messages);
        let rendered = document.render_markdown();
        assert!(rendered.contains(&format!("## Established Facts\n\n{EMPTY_SECTION_BODY}")));
        assert!(rendered.contains(&format!("## Key Conclusions\n\n{EMPTY_SECTION_BODY}")));
        for heading in SECTION_ORDER {
            assert!(rendered.contains(&format!("## {heading}\n")), "{heading}");
        }
    }

    #[test]
    fn generation_is_byte_identical_across_runs() {
        let messages = english();
        let first = document_for(&messages).render_markdown();
        let second = document_for(&messages).render_markdown();
        assert_eq!(first, second);

        let hebrew_messages = hebrew();
        assert_eq!(
            document_for(&hebrew_messages).render_markdown(),
            document_for(&hebrew_messages).render_markdown()
        );
    }

    #[test]
    fn output_leaks_no_node_ids_or_content_type_internals() {
        let messages = english();
        let rendered = document_for(&messages).render_markdown();
        assert!(!rendered.contains("node-"));
        assert!(!rendered.contains("content_type"));
        assert!(!rendered.contains("multimodal_text"));
        assert!(!rendered.contains("execution_output"));
    }

    #[test]
    fn the_document_discloses_that_it_is_heuristic() {
        let messages = english();
        let rendered = document_for(&messages).render_markdown();
        assert!(rendered.contains("heuristic extraction"));
        assert!(rendered.contains("not by semantic summarization"));
        assert!(rendered.contains("not semantic summarization"));
    }

    // -- Conversation / Purpose -------------------------------------------

    #[test]
    fn conversation_section_carries_identity_and_timestamps() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Conversation");
        assert!(body.contains("- **Title:** Migrating the exporter"));
        assert!(body.contains("- **Original conversation ID:** abc-123"));
        assert!(body.contains("- **Created:** 2025-09-05T11:32:04Z"));
        assert!(body.contains("- **Last updated:** 2025-09-06T15:18:44Z"));
    }

    #[test]
    fn purpose_is_the_first_substantive_user_message() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Purpose");
        assert!(body.starts_with("I need to migrate our exporter"));
        assert!(!body.contains('\n'), "purpose must be whitespace-collapsed");
    }

    #[test]
    fn purpose_skips_a_trivial_opener() {
        let messages = script(&[
            (Role::User, "hi"),
            (Role::Assistant, "hello"),
            (Role::User, "I want to rewrite the billing service in Rust."),
        ]);
        let body = body_of(&document_for(&messages), "Purpose");
        assert!(body.starts_with("I want to rewrite the billing service"));
    }

    #[test]
    fn hebrew_purpose_is_extracted() {
        let messages = hebrew();
        let body = body_of(&document_for(&messages), "Purpose");
        assert!(body.contains("להעביר את המערכת"));
    }

    // -- Background --------------------------------------------------------

    #[test]
    fn background_digests_every_pre_tail_message() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 2, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "tail",
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Important Background");
        assert!(body.contains("Earlier conversation digest"));
        // 7 messages, tail of 2 => 5 pre-tail entries, each on its own line.
        let digest_lines = body
            .lines()
            .filter(|line| line.starts_with("- **User:**") || line.starts_with("- **Assistant:**"))
            .count();
        assert_eq!(digest_lines, 5);
    }

    #[test]
    fn background_digest_is_sampled_and_says_so_when_capped() {
        let turns: Vec<(Role, String)> = (0..400)
            .map(|index| {
                let role = if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                (role, format!("message number {index} about the exporter"))
            })
            .collect();
        let refs: Vec<(Role, &str)> = turns
            .iter()
            .map(|(role, text)| (role.clone(), text.as_str()))
            .collect();
        let messages = script(&refs);
        let entries = branch(&messages);
        let selection = select_recent(&entries, 5, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "tail",
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Important Background");
        assert!(body.contains("Digest sampled down to 60 of 395 pre-tail messages"));
        let digest_lines = body
            .lines()
            .filter(|line| line.starts_with("- **User:**") || line.starts_with("- **Assistant:**"))
            .count();
        assert_eq!(digest_lines, MAX_DIGEST_ENTRIES);
    }

    #[test]
    fn background_quotes_only_messages_outside_the_verbatim_tail() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 3, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "tail",
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Important Background");
        // The trailing question lives in the verbatim tail; repeating it here
        // would spend the reader's budget on a duplicate.
        assert!(!body.contains("What should we do about the nightly cron"));
        assert!(body.contains("I prefer Parquet over CSV."));
    }

    #[test]
    fn background_falls_back_to_the_whole_branch_when_the_tail_covers_it() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 99, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "tail",
            &ContextOptions::default(),
        );
        // Never a misleading "none identified" just because the tail is long.
        assert!(!body_of(&document, "Important Background").is_empty());
    }

    #[test]
    fn a_question_is_never_a_preference_decision_or_rejection() {
        let messages = script(&[
            (Role::User, "Set it up."),
            (
                Role::Assistant,
                "What should we do next? Should we instead of that use a queue? \
                 Have we decided on Parquet?",
            ),
            (Role::User, "Not sure."),
        ]);
        let document = document_for(&messages);
        for heading in [
            "User Preferences and Constraints",
            "Decisions Already Made",
            "Rejected / Superseded Approaches",
            "Key Conclusions",
        ] {
            assert!(
                !body_of(&document, heading).contains('?'),
                "a question leaked into {heading}"
            );
        }
    }

    #[test]
    fn background_has_no_digest_when_the_tail_covers_everything() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 99, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "tail",
            &ContextOptions::default(),
        );
        assert!(
            !body_of(&document, "Important Background").contains("Earlier conversation digest")
        );
    }

    // -- extraction heuristics --------------------------------------------

    #[test]
    fn facts_are_declarative_user_sentences_only() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Established Facts");
        assert!(body.contains("The exporter currently writes CSV files every night."));
        // Questions and preference sentences are claimed by other sections.
        assert!(!body.contains('?'));
        assert!(!body.contains("I prefer Parquet"));
    }

    #[test]
    fn preferences_pick_up_english_cues() {
        let messages = english();
        let body = body_of(&document_for(&messages), "User Preferences and Constraints");
        assert!(body.contains("I prefer Parquet over CSV."));
        assert!(body.contains("Please avoid adding new dependencies"));
    }

    #[test]
    fn preferences_pick_up_hebrew_cues() {
        let messages = hebrew();
        let body = body_of(&document_for(&messages), "User Preferences and Constraints");
        assert!(body.contains("אני רוצה שההעברה תהיה בלי השבתה."));
        assert!(body.contains("חייב להיות גיבוי מלא"));
    }

    #[test]
    fn decisions_pick_up_both_languages() {
        let english_body = body_of(&document_for(&english()), "Decisions Already Made");
        assert!(english_body.contains("We decided to go with Parquet."));

        let hebrew_body = body_of(&document_for(&hebrew()), "Decisions Already Made");
        assert!(hebrew_body.contains("החלטנו לעשות את ההעברה בסופשבוע."));
    }

    #[test]
    fn conclusions_pick_up_both_languages() {
        let english_body = body_of(&document_for(&english()), "Key Conclusions");
        assert!(english_body.contains("In summary the exporter migration"));

        let hebrew_body = body_of(&document_for(&hebrew()), "Key Conclusions");
        assert!(hebrew_body.contains("לסיכום ההעברה תתבצע בשלבים"));
    }

    #[test]
    fn rejections_pick_up_both_languages() {
        let english_body = body_of(
            &document_for(&english()),
            "Rejected / Superseded Approaches",
        );
        assert!(english_body.contains("Instead of the nightly cron"));

        let hebrew_body = body_of(&document_for(&hebrew()), "Rejected / Superseded Approaches");
        assert!(hebrew_body.contains("במקום העברה מלאה"));
    }

    #[test]
    fn terminology_ranks_by_frequency_not_capitalization() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Terminology and Entities");
        assert!(body.contains("**Frequent terms:**"));
        assert!(body.contains("exporter"), "{body}");
        assert!(body.contains("`cargo build --release`") || body.contains("**Quoted terms:**"));
    }

    #[test]
    fn terminology_works_without_capital_letters() {
        let messages = hebrew();
        let body = body_of(&document_for(&messages), "Terminology and Entities");
        assert!(body.contains("**Frequent terms:**"), "{body}");
        assert!(body.contains("המערכת") || body.contains("ההעברה"), "{body}");
    }

    #[test]
    fn terminology_includes_capitalized_runs_when_present() {
        let messages = script(&[
            (Role::User, "We are shipping the Acme Exporter next week."),
            (Role::Assistant, "The Acme Exporter is ready."),
            (Role::User, "Ship the Acme Exporter."),
        ]);
        let body = body_of(&document_for(&messages), "Terminology and Entities");
        assert!(body.contains("Acme Exporter"), "{body}");
    }

    #[test]
    fn technical_details_collect_paths_commands_links_and_versions() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Important Technical Details");
        assert!(body.contains("src/exporter/main.rs"), "{body}");
        assert!(body.contains("https://example.com/docs"), "{body}");
        assert!(body.contains("1.85"), "{body}");
        assert!(body.contains("cargo build --release"), "{body}");
    }

    #[test]
    fn trailing_sentence_punctuation_is_stripped_from_extracted_paths() {
        let messages = script(&[
            (
                Role::User,
                "The loader lives in src/export/json.rs. The branch code is src/graph/branch.rs.",
            ),
            (Role::Assistant, "Understood."),
        ]);
        let body = body_of(&document_for(&messages), "Important Technical Details");
        assert!(body.contains("src/export/json.rs"), "{body}");
        assert!(body.contains("src/graph/branch.rs"), "{body}");
        // The real extension survives; the full stop does not.
        assert!(!body.contains("json.rs."), "{body}");
        assert!(!body.contains("branch.rs."), "{body}");
        assert!(!body.contains("src/export/json,"), "{body}");
    }

    #[test]
    fn punctuated_and_plural_entity_variants_collapse_to_one_entry() {
        let messages = script(&[
            (Role::User, "I am building a Rust CLI."),
            (Role::Assistant, "Most Rust CLIs. use clap for arguments."),
            (Role::User, "Right, a Rust CLI is what I want."),
            (Role::Assistant, "The Rust CLI will ship next week."),
        ]);
        let body = body_of(&document_for(&messages), "Terminology and Entities");
        assert!(body.contains("Rust CLI"), "{body}");
        assert!(!body.contains("CLIs."), "{body}");
        // "Rust CLI" and "Rust CLIs" are one entity, listed once.
        let entities = body
            .lines()
            .find(|line| line.starts_with("- **Named entities:**"))
            .unwrap_or_default();
        assert_eq!(entities.matches("Rust CLI").count(), 1, "{entities}");
    }

    #[test]
    fn a_conditional_instead_of_sentence_is_not_a_rejected_approach() {
        let messages = script(&[
            (
                Role::User,
                "If an export is malformed, tell me about it instead of quietly guessing.",
            ),
            (Role::Assistant, "Understood."),
            (Role::User, "Thanks."),
        ]);
        let document = document_for(&messages);
        let rejected = body_of(&document, "Rejected / Superseded Approaches");
        assert!(
            !rejected.contains("instead of quietly guessing"),
            "a hypothetical preference was filed as a rejected approach: {rejected}"
        );
    }

    #[test]
    fn a_bare_contrast_needs_a_decision_signal_to_count_as_a_rejection() {
        assert!(!is_rejection("Tell me instead of guessing."));
        assert!(is_rejection("Instead of the cron we decided on a queue."));
        assert!(is_rejection("That approach no longer works for us."));
        assert!(!is_rejection("If it fails, retry instead of aborting."));
        assert!(!is_rejection("When it fails, log instead of aborting."));
    }

    #[test]
    fn a_sentence_is_never_both_a_preference_and_a_rejected_approach() {
        let messages = script(&[
            (
                Role::User,
                "We must use Parquet instead of CSV, we decided that already.",
            ),
            (Role::Assistant, "Understood."),
            (Role::User, "Thanks."),
        ]);
        let document = document_for(&messages);
        let preferences = body_of(&document, "User Preferences and Constraints");
        let rejected = body_of(&document, "Rejected / Superseded Approaches");
        assert!(
            preferences.contains("Parquet instead of CSV"),
            "{preferences}"
        );
        assert!(
            !rejected.contains("Parquet instead of CSV"),
            "the same line was filed under two contradictory headings: {rejected}"
        );
    }

    #[test]
    fn current_state_summarises_the_last_turns() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Current State");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), CURRENT_STATE_MESSAGES);
        assert!(lines.iter().all(|line| line.starts_with("- **")));
        assert!(body.contains("What should we do about the nightly cron"));
    }

    #[test]
    fn open_questions_promote_a_trailing_user_question() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Open Questions");
        assert!(body.starts_with("- What should we do about the nightly cron job"));
    }

    #[test]
    fn open_questions_work_in_hebrew() {
        let messages = hebrew();
        let body = body_of(&document_for(&messages), "Open Questions");
        assert!(body.contains("מתי בדיוק נתחיל"), "{body}");
    }

    #[test]
    fn recent_section_states_how_much_is_verbatim() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 3, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "## User\n\nverbatim tail\n",
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Recent Conversation");
        assert!(body.contains("the last 3 of 7 message(s)"));
        assert!(body.contains("verbatim tail"));
    }

    #[test]
    fn nested_transcript_headings_are_demoted_to_keep_the_section_intact() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 2, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "## User\n\nfirst\n\n## Assistant\n\nsecond\n",
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Recent Conversation");
        assert!(body.contains("### User"), "{body}");
        assert!(body.contains("### Assistant"), "{body}");
        // No level-1 or level-2 heading may survive inside the section, or the
        // section ends early for anything reading the document as a tree.
        for line in body.lines() {
            assert!(
                !line.starts_with("# ") && !line.starts_with("## "),
                "heading escapes Recent Conversation: {line:?}"
            );
        }
    }

    #[test]
    fn headings_inside_fenced_message_content_are_not_demoted() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 2, None);
        let tail = "## User\n\nHere is the markdown I mean:\n\n                    ```markdown\n## User\n# Heading\n```\n\n## Assistant\n\nGot it\n";
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            tail,
            &ContextOptions::default(),
        );
        let body = body_of(&document, "Recent Conversation");
        // The two role headings were demoted...
        assert_eq!(body.matches("### User").count(), 1, "{body}");
        assert!(body.contains("### Assistant"), "{body}");
        // ...but the fenced content is message text and stays byte-identical.
        assert!(
            body.contains("```markdown\n## User\n# Heading\n```"),
            "{body}"
        );
    }

    #[test]
    fn heading_demotion_survives_long_and_tilde_fences() {
        let input =
            "## User\n\n````\n## Not a heading\n````\n\n~~~\n# Also not\n~~~\n\n## Assistant";
        let out = demote_headings(input);
        assert!(out.starts_with("### User"));
        assert!(out.contains("\n## Not a heading\n"));
        assert!(out.contains("\n# Also not\n"));
        assert!(out.ends_with("### Assistant"));
    }

    #[test]
    fn recent_section_is_explicit_when_nothing_was_preserved() {
        let messages = english();
        let entries = branch(&messages);
        let selection = select_recent(&entries, 0, None);
        let document = build_document(
            &conversation(),
            &entries,
            &selection,
            "",
            &ContextOptions::default(),
        );
        assert_eq!(
            body_of(&document, "Recent Conversation"),
            "_No messages were preserved verbatim._"
        );
    }

    #[test]
    fn continuation_instructions_are_the_mandated_block() {
        let messages = english();
        let body = body_of(&document_for(&messages), "Continuation Instructions");
        for line in CONTINUATION_BLOCK.lines() {
            assert!(body.contains(line), "missing: {line}");
        }
        assert!(body.contains("heuristic extraction"));
    }

    #[test]
    fn an_empty_branch_still_produces_a_complete_document() {
        let document = build_document(
            &conversation(),
            &[],
            &RecentSelection {
                start_index: 0,
                message_count: 0,
                characters: 0,
            },
            "",
            &ContextOptions::default(),
        );
        let rendered = document.render_markdown();
        assert_eq!(document.sections.len(), SECTION_ORDER.len());
        assert!(rendered.contains(&format!("## Purpose\n\n{EMPTY_SECTION_BODY}")));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn hostile_content_is_sanitized_and_never_breaks_the_layout() {
        let messages = script(&[
            (
                Role::User,
                "control\u{7}chars and \u{202e}bidi overrides must not survive.\nSecond line.",
            ),
            (Role::Assistant, "ok"),
        ]);
        let rendered = document_for(&messages).render_markdown();
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{202e}'));
    }

    // -- helper unit tests -------------------------------------------------

    #[test]
    fn sampling_is_even_and_bounded() {
        assert_eq!(sample_indices(0, 5), Vec::<usize>::new());
        assert_eq!(sample_indices(3, 5), vec![0, 1, 2]);
        assert_eq!(sample_indices(5, 5), vec![0, 1, 2, 3, 4]);
        let picked = sample_indices(100, 5);
        assert_eq!(picked, vec![0, 24, 49, 74, 99]);
        assert!(picked.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn version_and_path_detection() {
        assert!(looks_like_version("1.85"));
        assert!(looks_like_version("v0.4.2"));
        assert!(!looks_like_version("1."));
        assert!(!looks_like_version("word"));
        assert!(!looks_like_version("1.2.x"));
        assert!(looks_like_path("src/main.rs"));
        assert!(looks_like_path("Cargo.toml"));
        assert!(!looks_like_path("e.g"));
        assert!(!looks_like_path("ok"));
    }

    #[test]
    fn cue_matching_folds_case_and_typographic_apostrophes() {
        assert!(contains_any(&fold("We DON’T want that"), PREFERENCE_CUES));
        assert!(contains_any(&fold("we Decided already"), DECISION_CUES));
        assert!(!contains_any(&fold("nothing here"), DECISION_CUES));
    }
}
