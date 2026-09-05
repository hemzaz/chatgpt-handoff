//! Deterministic conversation selection.
//!
//! Picking the wrong conversation is the worst failure this tool can have: the
//! user asks for one handoff and silently gets another. So resolution is a
//! strict ladder of increasingly fuzzy tiers, and any tier that produces more
//! than one plausible hit stops and reports [`Resolution::Ambiguous`] rather
//! than guessing.
//!
//! # The resolution ladder
//!
//! Tiers are tried in order and the first one that yields a hit decides the
//! outcome:
//!
//! 1. **Exact conversation id** — `id == needle`, case-sensitive.
//! 2. **Unique id prefix** — `id.starts_with(needle)`, needle at least
//!    [`ID_PREFIX_MIN_LEN`] characters. More than one hit is ambiguous.
//! 3. **Exact title** — case-sensitive, against both the raw `title` and
//!    [`Conversation::display_title`].
//! 4. **Case-insensitive exact title**.
//! 5. **Unique fuzzy title match** — see below.
//!
//! # The "unique fuzzy" rule
//!
//! Tier 5 collects every title scoring at least [`FUZZY_MIN_SCORE`]. Then:
//!
//! * exactly one candidate → [`Resolution::Unique`];
//! * several candidates → `Unique` **only if** the best scores at least
//!   [`FUZZY_CONFIDENT_SCORE`] *and* leads the runner-up by at least
//!   [`FUZZY_LEAD_MARGIN`] points;
//! * otherwise → [`Resolution::Ambiguous`] with up to
//!   [`MAX_AMBIGUOUS_CANDIDATES`] candidates.
//!
//! # Which tiers run
//!
//! [`Selector::id`] feeds the id tiers (1-2) and [`Selector::title`] the title
//! tiers (3-5). A bare [`Selector::query`] feeds every tier, but only for the
//! lanes that have no explicit field: `--conversation X` alone never falls back
//! to title matching, and `--title Y` alone never matches an id.

use crate::error::{AmbiguousCandidate, SelectError};
use crate::model::Conversation;
use crate::search::FuzzyScorer;

/// Minimum length of an id prefix before tier 2 will consider it.
pub const ID_PREFIX_MIN_LEN: usize = 8;

/// Titles below this fuzzy score are not candidates at all.
pub const FUZZY_MIN_SCORE: u32 = 50;

/// A fuzzy leader must reach this score to win outright.
pub const FUZZY_CONFIDENT_SCORE: u32 = 85;

/// ...and must lead the runner-up by at least this many points.
pub const FUZZY_LEAD_MARGIN: u32 = 20;

/// Cap on the candidate list reported for an ambiguous selector.
pub const MAX_AMBIGUOUS_CANDIDATES: usize = 10;

/// Score reported for candidates that matched an exact (non-fuzzy) tier.
const EXACT_SCORE: u32 = 100;

/// What the user asked for.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    pub id: Option<String>,
    pub title: Option<String>,
    pub query: Option<String>,
}

impl Selector {
    /// True when nothing at all was supplied.
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.title.is_none() && self.query.is_none()
    }

    /// Quoted human description, e.g. `conversation id "abc123"`, `title "foo"`
    /// or `query "iboga"`. An empty selector describes itself as
    /// `no selector`.
    pub fn describe(&self) -> String {
        if let Some(id) = &self.id {
            format!("conversation id {id:?}")
        } else if let Some(title) = &self.title {
            format!("title {title:?}")
        } else if let Some(query) = &self.query {
            format!("query {query:?}")
        } else {
            "no selector".to_string()
        }
    }
}

/// One conversation that a selector could plausibly have meant.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub conversation: &'a Conversation,
    pub score: u32,
}

/// Outcome of [`resolve`].
#[derive(Debug, Clone)]
pub enum Resolution<'a> {
    /// Exactly one conversation was meant.
    Unique(&'a Conversation),
    /// Several conversations matched and the tool refuses to guess.
    Ambiguous(Vec<Candidate<'a>>),
}

/// Update time used for ordering; conversations without one sort last.
fn order_time(conversation: &Conversation) -> f64 {
    conversation.update_time.unwrap_or(f64::NEG_INFINITY)
}

/// Sort candidates by score descending, `update_time` descending, id ascending.
///
/// The id tie-break is what makes the result independent of the input order
/// and of any `HashMap` iteration inside the model.
fn sort_candidates(candidates: &mut [Candidate<'_>]) {
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| order_time(b.conversation).total_cmp(&order_time(a.conversation)))
            .then_with(|| a.conversation.id.cmp(&b.conversation.id))
    });
}

/// Turn an exact-tier hit list into a resolution: one hit wins, several are
/// ambiguous, none defers to the next tier.
fn decide_exact<'a>(mut hits: Vec<&'a Conversation>) -> Option<Resolution<'a>> {
    match hits.len() {
        0 => None,
        1 => hits.pop().map(Resolution::Unique),
        _ => {
            let mut candidates: Vec<Candidate<'a>> = hits
                .into_iter()
                .map(|conversation| Candidate {
                    conversation,
                    score: EXACT_SCORE,
                })
                .collect();
            sort_candidates(&mut candidates);
            candidates.truncate(MAX_AMBIGUOUS_CANDIDATES);
            Some(Resolution::Ambiguous(candidates))
        }
    }
}

/// Tier 1: exact, case-sensitive conversation id.
fn tier_exact_id<'a>(conversations: &'a [Conversation], needle: &str) -> Option<Resolution<'a>> {
    decide_exact(conversations.iter().filter(|c| c.id == needle).collect())
}

/// Tier 2: unambiguous id prefix of at least [`ID_PREFIX_MIN_LEN`] characters.
fn tier_id_prefix<'a>(conversations: &'a [Conversation], needle: &str) -> Option<Resolution<'a>> {
    if needle.chars().count() < ID_PREFIX_MIN_LEN {
        return None;
    }
    decide_exact(
        conversations
            .iter()
            .filter(|c| c.id.starts_with(needle))
            .collect(),
    )
}

/// Tier 3: exact, case-sensitive title, matched against the raw title and the
/// sanitized display title (so a title carrying stripped control characters is
/// still reachable by what the user actually saw printed).
fn tier_exact_title<'a>(conversations: &'a [Conversation], needle: &str) -> Option<Resolution<'a>> {
    decide_exact(
        conversations
            .iter()
            .filter(|c| c.title.as_deref() == Some(needle) || c.display_title() == needle)
            .collect(),
    )
}

/// Tier 4: case-insensitive exact title.
fn tier_title_ignore_case<'a>(
    conversations: &'a [Conversation],
    needle: &str,
) -> Option<Resolution<'a>> {
    let needle_lower = needle.to_lowercase();
    decide_exact(
        conversations
            .iter()
            .filter(|c| {
                c.title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase() == needle_lower)
                    || c.display_title().to_lowercase() == needle_lower
            })
            .collect(),
    )
}

/// Tier 5: fuzzy title match, resolved by the "unique fuzzy" rule documented at
/// the module level.
fn tier_fuzzy_title<'a>(conversations: &'a [Conversation], needle: &str) -> Option<Resolution<'a>> {
    let mut scorer = FuzzyScorer::new();
    let mut candidates: Vec<Candidate<'a>> = conversations
        .iter()
        .filter_map(|conversation| {
            let score = scorer.score(&conversation.display_title(), needle)?;
            (score >= FUZZY_MIN_SCORE).then_some(Candidate {
                conversation,
                score,
            })
        })
        .collect();
    sort_candidates(&mut candidates);

    match candidates.split_first() {
        None => None,
        Some((best, [])) => Some(Resolution::Unique(best.conversation)),
        Some((best, rest)) => {
            let runner_up = rest.first().map_or(0, |c| c.score);
            let confident = best.score >= FUZZY_CONFIDENT_SCORE
                && best.score.saturating_sub(runner_up) >= FUZZY_LEAD_MARGIN;
            if confident {
                Some(Resolution::Unique(best.conversation))
            } else {
                candidates.truncate(MAX_AMBIGUOUS_CANDIDATES);
                Some(Resolution::Ambiguous(candidates))
            }
        }
    }
}

/// Resolve a selector against a conversation list.
///
/// Walks the resolution ladder documented at the module level and stops at the
/// first tier that yields a hit. See that documentation for the exact tier
/// order and the "unique fuzzy" rule.
///
/// # Errors
///
/// * [`SelectError::Empty`] — the export contains no conversations.
/// * [`SelectError::NoSelector`] — nothing was asked for.
/// * [`SelectError::NotFound`] — no tier matched anything.
pub fn resolve<'a>(
    conversations: &'a [Conversation],
    selector: &Selector,
) -> Result<Resolution<'a>, SelectError> {
    if conversations.is_empty() {
        return Err(SelectError::Empty);
    }
    if selector.is_empty() {
        return Err(SelectError::NoSelector);
    }

    // An explicit field claims its lane; a bare query claims every lane that no
    // explicit field has claimed.
    let id_lane = selector.id.as_deref().or(if selector.title.is_none() {
        selector.query.as_deref()
    } else {
        None
    });
    let title_lane = selector.title.as_deref().or(if selector.id.is_none() {
        selector.query.as_deref()
    } else {
        None
    });

    if let Some(needle) = id_lane.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(resolution) = tier_exact_id(conversations, needle) {
            return Ok(resolution);
        }
        if let Some(resolution) = tier_id_prefix(conversations, needle) {
            return Ok(resolution);
        }
    }

    if let Some(needle) = title_lane.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(resolution) = tier_exact_title(conversations, needle) {
            return Ok(resolution);
        }
        if let Some(resolution) = tier_title_ignore_case(conversations, needle) {
            return Ok(resolution);
        }
        if let Some(resolution) = tier_fuzzy_title(conversations, needle) {
            return Ok(resolution);
        }
    }

    Err(SelectError::NotFound {
        query: selector.describe(),
    })
}

/// [`resolve`], but an ambiguous outcome is an error rather than a value.
///
/// # Errors
///
/// Everything [`resolve`] can fail with, plus [`SelectError::Ambiguous`]
/// carrying the candidate list.
pub fn resolve_unique<'a>(
    conversations: &'a [Conversation],
    selector: &Selector,
) -> Result<&'a Conversation, SelectError> {
    match resolve(conversations, selector)? {
        Resolution::Unique(conversation) => Ok(conversation),
        Resolution::Ambiguous(candidates) => Err(SelectError::Ambiguous {
            query: selector.describe(),
            candidates: candidates
                .into_iter()
                .map(|candidate| AmbiguousCandidate {
                    id: candidate.conversation.id.clone(),
                    title: candidate.conversation.display_title(),
                    score: candidate.score,
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn conversation(id: &str, title: Option<&str>) -> Conversation {
        Conversation {
            id: id.to_string(),
            title: title.map(str::to_string),
            create_time: Some(1.0),
            update_time: Some(1.0),
            current_node: None,
            mapping: HashMap::new(),
        }
    }

    fn by_query(query: &str) -> Selector {
        Selector {
            query: Some(query.to_string()),
            ..Selector::default()
        }
    }

    fn unique_id<'a>(conversations: &'a [Conversation], selector: &Selector) -> &'a str {
        match resolve(conversations, selector) {
            Ok(Resolution::Unique(c)) => c.id.as_str(),
            other => panic!("expected a unique resolution, got {other:?}"),
        }
    }

    fn ambiguous_ids(conversations: &[Conversation], selector: &Selector) -> Vec<String> {
        match resolve(conversations, selector) {
            Ok(Resolution::Ambiguous(candidates)) => candidates
                .into_iter()
                .map(|c| c.conversation.id.clone())
                .collect(),
            other => panic!("expected an ambiguous resolution, got {other:?}"),
        }
    }

    #[test]
    fn describe_quotes_each_selector_kind() {
        assert_eq!(
            Selector {
                id: Some("abc123".into()),
                ..Selector::default()
            }
            .describe(),
            "conversation id \"abc123\""
        );
        assert_eq!(
            Selector {
                title: Some("foo".into()),
                ..Selector::default()
            }
            .describe(),
            "title \"foo\""
        );
        assert_eq!(by_query("iboga").describe(), "query \"iboga\"");
        assert_eq!(Selector::default().describe(), "no selector");
    }

    #[test]
    fn tier1_exact_id_wins_even_over_a_better_title() {
        let set = vec![
            conversation("iboga", Some("something else")),
            conversation("other-id", Some("iboga")),
        ];
        assert_eq!(unique_id(&set, &by_query("iboga")), "iboga");
    }

    #[test]
    fn tier1_accepts_an_id_pasted_positionally() {
        let set = vec![conversation("aaaaaaaa-bbbb-cccc", Some("x"))];
        assert_eq!(
            unique_id(&set, &by_query("aaaaaaaa-bbbb-cccc")),
            "aaaaaaaa-bbbb-cccc"
        );
    }

    #[test]
    fn tier2_unique_id_prefix() {
        let set = vec![
            conversation("aaaaaaaa-1111", Some("x")),
            conversation("bbbbbbbb-2222", Some("y")),
        ];
        assert_eq!(unique_id(&set, &by_query("aaaaaaaa")), "aaaaaaaa-1111");
    }

    #[test]
    fn tier2_rejects_short_prefixes() {
        let set = vec![conversation("aaaaaaaa-1111", Some("zzz"))];
        // "aaaaaaa" is 7 chars: below the 8-char floor, so tier 2 never fires
        // and no later tier matches this title either.
        assert!(matches!(
            resolve(&set, &by_query("aaaaaaa")),
            Err(SelectError::NotFound { .. })
        ));
    }

    #[test]
    fn tier2_ambiguous_prefix_is_reported_not_guessed() {
        let set = vec![
            conversation("aaaaaaaa-1111", Some("x")),
            conversation("aaaaaaaa-2222", Some("y")),
        ];
        assert_eq!(
            ambiguous_ids(&set, &by_query("aaaaaaaa")),
            vec!["aaaaaaaa-1111", "aaaaaaaa-2222"]
        );
    }

    #[test]
    fn tier3_exact_case_sensitive_title() {
        let set = vec![
            conversation("a", Some("Iboga Detox")),
            conversation("b", Some("iboga detox")),
        ];
        let selector = Selector {
            title: Some("Iboga Detox".into()),
            ..Selector::default()
        };
        assert_eq!(unique_id(&set, &selector), "a");
    }

    #[test]
    fn tier3_matches_the_sanitized_display_title() {
        let set = vec![conversation("a", Some("ok\u{202e}evil"))];
        let selector = Selector {
            title: Some("okevil".into()),
            ..Selector::default()
        };
        assert_eq!(unique_id(&set, &selector), "a");
    }

    #[test]
    fn tier3_matches_the_untitled_placeholder() {
        let set = vec![conversation("a", None), conversation("b", Some("titled"))];
        let selector = Selector {
            title: Some("(untitled)".into()),
            ..Selector::default()
        };
        assert_eq!(unique_id(&set, &selector), "a");
    }

    #[test]
    fn tier4_case_insensitive_title() {
        let set = vec![conversation("a", Some("Iboga Detox"))];
        let selector = Selector {
            title: Some("iboga detox".into()),
            ..Selector::default()
        };
        assert_eq!(unique_id(&set, &selector), "a");
    }

    #[test]
    fn tier4_ambiguous_case_insensitive_titles() {
        let set = vec![
            conversation("a", Some("Iboga Detox")),
            conversation("b", Some("IBOGA DETOX")),
            conversation("c", Some("iboga detox")),
        ];
        let selector = Selector {
            title: Some("Iboga DETOX".into()),
            ..Selector::default()
        };
        assert_eq!(ambiguous_ids(&set, &selector), vec!["a", "b", "c"]);
    }

    #[test]
    fn tier5_unique_fuzzy_when_only_one_candidate_clears_the_floor() {
        let set = vec![
            conversation("a", Some("איבוגה גמילה מאופיאטים")),
            conversation("b", Some("quarterly revenue planning")),
        ];
        assert_eq!(unique_id(&set, &by_query("איבוגה")), "a");
    }

    #[test]
    fn tier5_unique_fuzzy_when_the_leader_is_far_enough_ahead() {
        // Neither title matches "iboga" exactly, so tiers 3 and 4 miss and this
        // really is tier 5: 100 vs 74 clears both the floor and the margin.
        let set = vec![
            conversation("a", Some("iboga detox")),
            conversation("b", Some("invoicing backlog agenda")),
        ];
        assert_eq!(unique_id(&set, &by_query("iboga")), "a");
    }

    #[test]
    fn tier5_similar_titles_are_ambiguous_not_a_silent_wrong_pick() {
        // A needle that is a prefix of both titles scores identically against
        // both. The leader has no margin, so the tool must refuse to choose.
        let set = vec![
            conversation("a", Some("iboga detox protocol")),
            conversation("b", Some("iboga detox protocols")),
        ];
        let ids = ambiguous_ids(&set, &by_query("iboga detox protoco"));
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn tier5_confident_leader_without_enough_margin_is_still_ambiguous() {
        // 100 vs 87: above the confidence floor, but a 13-point lead is under
        // the 20-point margin, so this is ambiguous rather than a guess.
        let set = vec![
            conversation("a", Some("iboga detox")),
            conversation("b", Some("in bogota")),
        ];
        let ids = ambiguous_ids(&set, &by_query("iboga"));
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn ambiguous_candidate_lists_are_capped() {
        let set: Vec<Conversation> = (0..30)
            .map(|i| conversation(&format!("id-{i:02}"), Some("iboga detox notes")))
            .collect();
        assert_eq!(
            ambiguous_ids(&set, &by_query("iboga detox notes")).len(),
            MAX_AMBIGUOUS_CANDIDATES
        );
    }

    #[test]
    fn title_selector_never_matches_an_id() {
        let set = vec![conversation("aaaaaaaa-1111", Some("zzz"))];
        let selector = Selector {
            title: Some("aaaaaaaa-1111".into()),
            ..Selector::default()
        };
        assert!(matches!(
            resolve(&set, &selector),
            Err(SelectError::NotFound { .. })
        ));
    }

    #[test]
    fn id_selector_never_matches_a_title() {
        let set = vec![conversation("xyz", Some("iboga detox"))];
        let selector = Selector {
            id: Some("iboga detox".into()),
            ..Selector::default()
        };
        assert!(matches!(
            resolve(&set, &selector),
            Err(SelectError::NotFound { .. })
        ));
    }

    #[test]
    fn empty_list_is_reported_before_anything_else() {
        assert!(matches!(
            resolve(&[], &by_query("x")),
            Err(SelectError::Empty)
        ));
        assert!(matches!(
            resolve(&[], &Selector::default()),
            Err(SelectError::Empty)
        ));
        assert!(matches!(
            resolve_unique(&[], &by_query("x")),
            Err(SelectError::Empty)
        ));
    }

    #[test]
    fn no_selector() {
        let set = vec![conversation("a", Some("x"))];
        assert!(matches!(
            resolve(&set, &Selector::default()),
            Err(SelectError::NoSelector)
        ));
    }

    #[test]
    fn not_found_carries_the_selector_description() {
        let set = vec![conversation("a", Some("completely different"))];
        match resolve(&set, &by_query("zzzqqqxxx")) {
            Err(SelectError::NotFound { query }) => assert_eq!(query, "query \"zzzqqqxxx\""),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn resolve_unique_converts_ambiguity_into_an_error() {
        let set = vec![
            conversation("a", Some("Iboga Detox")),
            conversation("b", Some("IBOGA DETOX")),
        ];
        let selector = Selector {
            title: Some("iboga detox".into()),
            ..Selector::default()
        };
        match resolve_unique(&set, &selector) {
            Err(SelectError::Ambiguous { query, candidates }) => {
                assert_eq!(query, "title \"iboga detox\"");
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].id, "a");
                assert_eq!(candidates[0].title, "Iboga Detox");
                assert_eq!(candidates[0].score, EXACT_SCORE);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_unique_returns_the_conversation() {
        let set = vec![conversation("a", Some("iboga"))];
        let found = resolve_unique(&set, &by_query("a")).expect("exact id resolves");
        assert_eq!(found.id, "a");
    }

    #[test]
    fn duplicate_ids_are_ambiguous_rather_than_first_wins() {
        let set = vec![
            conversation("dup", Some("one")),
            conversation("dup", Some("two")),
        ];
        assert_eq!(ambiguous_ids(&set, &by_query("dup")), vec!["dup", "dup"]);
    }

    #[test]
    fn resolution_is_deterministic_across_repeated_runs() {
        let set: Vec<Conversation> = (0..12)
            .map(|i| conversation(&format!("id-{i:02}"), Some("iboga detox notes")))
            .collect();
        let selector = by_query("iboga detox notes");
        let first = ambiguous_ids(&set, &selector);
        for _ in 0..5 {
            assert_eq!(ambiguous_ids(&set, &selector), first);
        }
    }

    #[test]
    fn whitespace_only_selectors_do_not_match_everything() {
        let set = vec![conversation("a", Some("iboga"))];
        let selector = by_query("   ");
        assert!(matches!(
            resolve(&set, &selector),
            Err(SelectError::NotFound { .. })
        ));
    }
}
