//! Fuzzy search over conversations.
//!
//! Scoring is delegated to [`nucleo_matcher`], whose raw scores are unbounded
//! and therefore meaningless to a user. [`FuzzyScorer`] normalizes them into a
//! stable `0..=100` band by dividing by the score the needle achieves against
//! *itself*, which is the maximum any haystack can reach for that needle.
//!
//! Every ordering decision in this module is total and independent of
//! `HashMap` iteration order, so repeated runs over the same export always
//! produce byte-identical output.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::model::Conversation;
use crate::text;

/// Which part of a conversation produced a [`Match`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchField {
    Id,
    Title,
    Content,
}

impl MatchField {
    /// Tie-break rank: lower wins. Title beats id, id beats content, so a
    /// conversation whose title and body both match is reported as a title hit.
    const fn rank(self) -> u8 {
        match self {
            MatchField::Title => 0,
            MatchField::Id => 1,
            MatchField::Content => 2,
        }
    }
}

/// One conversation that matched a query, with the field and score that won.
#[derive(Debug, Clone)]
pub struct Match<'a> {
    pub conversation: &'a Conversation,
    /// Normalized 0..=100.
    pub score: u32,
    pub field: MatchField,
    /// Short excerpt for content matches.
    pub excerpt: Option<String>,
}

/// Knobs for [`search`].
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    /// Maximum number of matches returned, applied after sorting.
    pub limit: usize,
    /// Scan message bodies too. Off by default: it is orders of magnitude more
    /// expensive than matching titles, because it walks every node of every
    /// conversation's mapping.
    pub search_content: bool,
    /// Matches scoring below this are dropped.
    pub min_score: u32,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            search_content: false,
            min_score: 30,
        }
    }
}

/// Longest haystack, in `char`s, handed to the matcher.
///
/// `nucleo` documents that it panics on haystacks longer than `u32::MAX`
/// codepoints, and its fuzzy match is `O(needle * haystack)`. A single ChatGPT
/// message can be megabytes of pasted logs, so haystacks are clamped to a
/// prefix. This bounds both the panic risk and the cost of content search.
const MAX_HAYSTACK_CHARS: usize = 4096;

/// Minimum query length before an id *prefix* is treated as a strong hit.
/// Shorter prefixes match far too many ids to be a useful signal.
const ID_PREFIX_MIN_LEN: usize = 4;

/// Score awarded to an exact (case-insensitive) id match.
const ID_EXACT_SCORE: u32 = 100;

/// Score awarded to a long-enough (case-insensitive) id prefix match.
const ID_PREFIX_SCORE: u32 = 95;

/// Grapheme budget for content excerpts.
const EXCERPT_GRAPHEMES: usize = 120;

/// A needle plus everything derived from it that is worth computing once.
struct CachedNeedle {
    text: String,
    atom: Atom,
    /// Score of the needle against itself: the maximum attainable raw score.
    perfect: u16,
}

/// Reusable fuzzy scorer. Scores are normalized to 0..=100 by comparing against
/// the score the needle achieves against itself.
///
/// Constructing a [`Matcher`] eagerly allocates a large matrix slab, so one
/// scorer must be reused across a whole conversation list rather than built per
/// candidate. The needle-derived state is cached too and only rebuilt when the
/// needle actually changes.
///
/// Matching is case-insensitive ([`CaseMatching::Ignore`]) and unicode-aware
/// ([`Normalization::Smart`]), so Hebrew, Arabic and accented needles behave
/// the same as ASCII ones.
pub struct FuzzyScorer {
    matcher: Matcher,
    /// Scratch buffer for the `&str -> Utf32Str` conversion.
    buf: Vec<char>,
    cached: Option<CachedNeedle>,
}

impl FuzzyScorer {
    /// Create a scorer. Prefer one per search pass over one per candidate.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
            cached: None,
        }
    }

    /// Score `needle` against `haystack`, normalized to `0..=100`.
    ///
    /// Returns `None` when the needle does not match at all, when either side
    /// is empty, or in the degenerate case where the needle cannot even score
    /// against itself (which would make normalization undefined).
    ///
    /// A self-match always scores exactly `100`.
    pub fn score(&mut self, haystack: &str, needle: &str) -> Option<u32> {
        if haystack.is_empty() || needle.is_empty() {
            return None;
        }

        let stale = match &self.cached {
            Some(cached) => cached.text != needle,
            None => true,
        };
        if stale {
            let atom = Atom::new(
                needle,
                CaseMatching::Ignore,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            );
            let perfect = {
                let self_hay = Utf32Str::new(clamp(needle), &mut self.buf);
                atom.score(self_hay, &mut self.matcher).unwrap_or(0)
            };
            self.cached = Some(CachedNeedle {
                text: needle.to_string(),
                atom,
                perfect,
            });
        }

        let cached = self.cached.as_ref()?;
        if cached.perfect == 0 {
            return None;
        }

        let raw = {
            let hay = Utf32Str::new(clamp(haystack), &mut self.buf);
            cached.atom.score(hay, &mut self.matcher)?
        };

        let normalized = u32::from(raw) * 100 / u32::from(cached.perfect);
        Some(normalized.min(100))
    }
}

impl Default for FuzzyScorer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FuzzyScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzyScorer")
            .field("needle", &self.cached.as_ref().map(|c| c.text.as_str()))
            .finish_non_exhaustive()
    }
}

/// Clamp a haystack to [`MAX_HAYSTACK_CHARS`] `char`s, on a `char` boundary.
fn clamp(input: &str) -> &str {
    match input.char_indices().nth(MAX_HAYSTACK_CHARS) {
        None => input,
        Some((offset, _)) => &input[..offset],
    }
}

/// Update time used for ordering; conversations without one sort last.
fn order_time(conversation: &Conversation) -> f64 {
    conversation.update_time.unwrap_or(f64::NEG_INFINITY)
}

/// Score a conversation id, favouring the paste-an-id workflow.
fn score_id(id: &str, query: &str, query_lower: &str, scorer: &mut FuzzyScorer) -> Option<u32> {
    let id_lower = id.to_lowercase();
    if id_lower == query_lower {
        return Some(ID_EXACT_SCORE);
    }
    if query.len() >= ID_PREFIX_MIN_LEN && id_lower.starts_with(query_lower) {
        return Some(ID_PREFIX_SCORE);
    }
    scorer.score(id, query)
}

/// Best-scoring message body in a conversation, with an excerpt.
///
/// Nodes are visited in sorted-key order so that a tie between two equally
/// scoring messages resolves the same way on every run.
fn score_content(
    conversation: &Conversation,
    query: &str,
    scorer: &mut FuzzyScorer,
) -> Option<(u32, String)> {
    let mut keys: Vec<&str> = conversation.mapping.keys().map(String::as_str).collect();
    keys.sort_unstable();

    let mut best: Option<(u32, String)> = None;
    for key in keys {
        let Some(node) = conversation.mapping.get(key) else {
            continue;
        };
        let Some(message) = node.message.as_ref() else {
            continue;
        };
        let text_body = message.content.plain_text();
        if text_body.trim().is_empty() {
            continue;
        }
        let Some(score) = scorer.score(&text_body, query) else {
            continue;
        };
        if best.as_ref().is_none_or(|(top, _)| score > *top) {
            let excerpt =
                text::truncate_graphemes(&text::collapse_whitespace(&text_body), EXCERPT_GRAPHEMES)
                    .into_owned();
            best = Some((score, excerpt));
        }
    }
    best
}

/// Fuzzy-search `conversations` for `query`.
///
/// Titles and ids are always scored; message bodies only when
/// [`SearchOptions::search_content`] is set. Each conversation contributes at
/// most one [`Match`] — its best field, with ties broken title > id > content.
///
/// Results are filtered by [`SearchOptions::min_score`], then sorted by score
/// descending, `update_time` descending, and id ascending, then truncated to
/// [`SearchOptions::limit`]. An empty (or whitespace-only) query matches
/// nothing.
pub fn search<'a>(
    conversations: &'a [Conversation],
    query: &str,
    options: &SearchOptions,
) -> Vec<Match<'a>> {
    let query = query.trim();
    if query.is_empty() || options.limit == 0 {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();

    let mut scorer = FuzzyScorer::new();
    let mut matches: Vec<Match<'a>> = Vec::new();

    for conversation in conversations {
        let mut best: Option<(u32, MatchField, Option<String>)> = None;
        let mut consider = |score: u32, field: MatchField, excerpt: Option<String>| {
            let better = match &best {
                None => true,
                Some((top, top_field, _)) => {
                    score > *top || (score == *top && field.rank() < top_field.rank())
                }
            };
            if better {
                best = Some((score, field, excerpt));
            }
        };

        if let Some(score) = scorer.score(&conversation.display_title(), query) {
            consider(score, MatchField::Title, None);
        }
        if let Some(score) = score_id(&conversation.id, query, &query_lower, &mut scorer) {
            consider(score, MatchField::Id, None);
        }
        if options.search_content {
            if let Some((score, excerpt)) = score_content(conversation, query, &mut scorer) {
                consider(score, MatchField::Content, Some(excerpt));
            }
        }

        if let Some((score, field, excerpt)) = best {
            if score >= options.min_score {
                matches.push(Match {
                    conversation,
                    score,
                    field,
                    excerpt,
                });
            }
        }
    }

    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| order_time(b.conversation).total_cmp(&order_time(a.conversation)))
            .then_with(|| a.conversation.id.cmp(&b.conversation.id))
            .then_with(|| a.field.rank().cmp(&b.field.rank()))
    });
    matches.truncate(options.limit);
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Conversation, ConversationNode};
    use std::collections::HashMap;

    fn conversation(id: &str, title: Option<&str>, update_time: Option<f64>) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: title.map(str::to_string),
            create_time: Some(1.0),
            update_time,
            current_node: None,
            mapping: HashMap::new(),
        }
    }

    /// Attach message bodies under deterministic node keys.
    fn with_messages(mut conversation: Conversation, bodies: &[(&str, &str)]) -> Conversation {
        for (key, body) in bodies {
            let node: ConversationNode = serde_json::from_value(serde_json::json!({
                "id": key,
                "message": {
                    "id": key,
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": [body]}
                },
                "children": []
            }))
            .expect("fixture node parses");
            conversation.mapping.insert((*key).to_string(), node);
        }
        conversation
    }

    #[test]
    fn self_match_scores_one_hundred() {
        let mut scorer = FuzzyScorer::new();
        assert_eq!(scorer.score("iboga", "iboga"), Some(100));
        assert_eq!(scorer.score("איבוגה", "איבוגה"), Some(100));
    }

    #[test]
    fn unrelated_strings_do_not_match() {
        let mut scorer = FuzzyScorer::new();
        assert_eq!(scorer.score("completely other", "zzzqqq"), None);
    }

    #[test]
    fn empty_sides_never_match() {
        let mut scorer = FuzzyScorer::new();
        assert_eq!(scorer.score("", "abc"), None);
        assert_eq!(scorer.score("abc", ""), None);
    }

    #[test]
    fn hebrew_needle_matches_hebrew_title() {
        let mut scorer = FuzzyScorer::new();
        let score = scorer
            .score("איבוגה גמילה מאופיאטים", "איבוגה")
            .expect("hebrew prefix must match");
        assert!(score >= 80, "hebrew score too low: {score}");
    }

    #[test]
    fn latin_needle_against_hebrew_title_does_not_panic() {
        let mut scorer = FuzzyScorer::new();
        assert_eq!(scorer.score("איבוגה גמילה מאופיאטים", "iboga"), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let mut scorer = FuzzyScorer::new();
        assert_eq!(scorer.score("Iboga Detox", "IBOGA DETOX"), Some(100));
        assert_eq!(scorer.score("IBOGA DETOX", "iboga detox"), Some(100));
    }

    #[test]
    fn needle_cache_survives_alternating_haystacks() {
        let mut scorer = FuzzyScorer::new();
        let a = scorer.score("iboga detox", "iboga");
        let b = scorer.score("something else entirely", "iboga");
        let c = scorer.score("iboga detox", "iboga");
        assert_eq!(a, c);
        assert!(b.is_none() || b < a);
    }

    #[test]
    fn very_long_haystacks_are_clamped_not_panicking() {
        let mut scorer = FuzzyScorer::new();
        let huge = "x".repeat(MAX_HAYSTACK_CHARS * 3);
        assert!(scorer.score(&huge, "xxx").is_some());
    }

    #[test]
    fn empty_query_matches_nothing() {
        let set = vec![conversation("a", Some("iboga"), Some(1.0))];
        assert!(search(&set, "", &SearchOptions::default()).is_empty());
        assert!(search(&set, "   ", &SearchOptions::default()).is_empty());
    }

    #[test]
    fn exact_id_beats_everything() {
        let set = vec![
            conversation("aaaaaaaa-1111", Some("unrelated"), Some(5.0)),
            conversation("bbbbbbbb-2222", Some("iboga"), Some(9.0)),
        ];
        let found = search(&set, "aaaaaaaa-1111", &SearchOptions::default());
        assert_eq!(found[0].conversation.id, "aaaaaaaa-1111");
        assert_eq!(found[0].score, 100);
        assert_eq!(found[0].field, MatchField::Id);
    }

    #[test]
    fn id_prefix_scores_high() {
        let set = vec![conversation("abcd1234-ffff", Some("unrelated"), Some(1.0))];
        let found = search(&set, "abcd1234", &SearchOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].field, MatchField::Id);
        assert!(found[0].score >= ID_PREFIX_SCORE);
    }

    #[test]
    fn title_wins_over_content_on_a_tie() {
        let by_title = with_messages(
            conversation("aaa", Some("handoff"), Some(1.0)),
            &[("n1", "nothing relevant here")],
        );
        let by_content = with_messages(
            conversation("bbb", Some("zzz unrelated"), Some(1.0)),
            &[("n1", "handoff")],
        );
        let options = SearchOptions {
            search_content: true,
            ..SearchOptions::default()
        };
        let set = vec![by_title, by_content];
        let found = search(&set, "handoff", &options);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].conversation.id, "aaa");
        assert_eq!(found[0].field, MatchField::Title);
        assert_eq!(found[1].field, MatchField::Content);
        assert!(found[1].excerpt.is_some());
    }

    #[test]
    fn content_is_only_searched_when_requested() {
        let set = vec![with_messages(
            conversation("aaa", Some("zzz unrelated"), Some(1.0)),
            &[("n1", "the iboga protocol")],
        )];
        assert!(search(&set, "iboga", &SearchOptions::default()).is_empty());

        let options = SearchOptions {
            search_content: true,
            ..SearchOptions::default()
        };
        let found = search(&set, "iboga", &options);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].field, MatchField::Content);
        assert_eq!(found[0].excerpt.as_deref(), Some("the iboga protocol"));
    }

    #[test]
    fn excerpts_collapse_whitespace_and_truncate() {
        let long = format!("iboga {}", "word ".repeat(200));
        let set = vec![with_messages(
            conversation("aaa", Some("zzz"), Some(1.0)),
            &[("n1", &long)],
        )];
        let options = SearchOptions {
            search_content: true,
            ..SearchOptions::default()
        };
        let found = search(&set, "iboga", &options);
        let excerpt = found[0].excerpt.as_deref().expect("content match excerpt");
        assert!(!excerpt.contains('\n'));
        assert!(text::grapheme_count(excerpt) <= EXCERPT_GRAPHEMES + 1);
        assert!(excerpt.ends_with(text::ELLIPSIS));
    }

    #[test]
    fn ordering_is_deterministic_across_runs() {
        let set: Vec<Conversation> = (0..25)
            .map(|i| {
                conversation(
                    &format!("id-{i:02}"),
                    Some("iboga detox"),
                    // Deliberately identical timestamps: only the id tie-break
                    // can make this deterministic.
                    Some(100.0),
                )
            })
            .collect();
        let options = SearchOptions {
            limit: 50,
            ..SearchOptions::default()
        };
        let first: Vec<&str> = search(&set, "iboga", &options)
            .iter()
            .map(|m| m.conversation.id.as_str())
            .collect();
        for _ in 0..5 {
            let again: Vec<&str> = search(&set, "iboga", &options)
                .iter()
                .map(|m| m.conversation.id.as_str())
                .collect();
            assert_eq!(first, again);
        }
        assert_eq!(first.first().copied(), Some("id-00"));
    }

    #[test]
    fn newer_conversations_sort_first_on_equal_score() {
        let set = vec![
            conversation("aaa", Some("iboga"), Some(1.0)),
            conversation("bbb", Some("iboga"), Some(99.0)),
        ];
        let found = search(&set, "iboga", &SearchOptions::default());
        assert_eq!(found[0].conversation.id, "bbb");
    }

    #[test]
    fn missing_update_time_sorts_last() {
        let set = vec![
            conversation("aaa", Some("iboga"), None),
            conversation("bbb", Some("iboga"), Some(1.0)),
        ];
        let found = search(&set, "iboga", &SearchOptions::default());
        assert_eq!(found[0].conversation.id, "bbb");
    }

    #[test]
    fn min_score_and_limit_are_applied() {
        let set = vec![
            conversation("aaa", Some("iboga detox"), Some(2.0)),
            conversation("bbb", Some("iboga detox"), Some(1.0)),
        ];
        let strict = SearchOptions {
            min_score: 101,
            ..SearchOptions::default()
        };
        assert!(search(&set, "iboga", &strict).is_empty());

        let capped = SearchOptions {
            limit: 1,
            ..SearchOptions::default()
        };
        assert_eq!(search(&set, "iboga", &capped).len(), 1);
        assert!(
            search(
                &set,
                "iboga",
                &SearchOptions {
                    limit: 0,
                    ..SearchOptions::default()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn untitled_conversations_are_searchable_by_placeholder() {
        let set = vec![conversation("aaa", None, Some(1.0))];
        let found = search(&set, "untitled", &SearchOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].field, MatchField::Title);
    }

    #[test]
    fn hebrew_search_end_to_end() {
        let set = vec![
            conversation("aaa", Some("איבוגה גמילה מאופיאטים"), Some(2.0)),
            conversation("bbb", Some("unrelated english"), Some(1.0)),
        ];
        let found = search(&set, "איבוגה", &SearchOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].conversation.id, "aaa");
    }
}
