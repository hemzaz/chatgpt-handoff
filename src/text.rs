//! Unicode-safe text helpers.
//!
//! Every truncation in this crate goes through this module so that no output
//! can ever be cut in the middle of a grapheme cluster (which matters a lot for
//! the Hebrew, Arabic and emoji content that shows up in real exports).

use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

/// Ellipsis appended to truncated text.
pub const ELLIPSIS: &str = "…";

/// Truncate to at most `max` grapheme clusters, appending an ellipsis when the
/// input was actually shortened. Never splits a grapheme cluster.
pub fn truncate_graphemes(input: &str, max: usize) -> Cow<'_, str> {
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut boundary = None;
    for (count, (offset, _)) in input.grapheme_indices(true).enumerate() {
        if count == max {
            boundary = Some(offset);
            break;
        }
    }
    match boundary {
        None => Cow::Borrowed(input),
        Some(offset) => {
            let mut out = String::with_capacity(offset + ELLIPSIS.len());
            out.push_str(input[..offset].trim_end());
            out.push_str(ELLIPSIS);
            Cow::Owned(out)
        }
    }
}

/// Number of grapheme clusters. Used everywhere a "character count" is shown,
/// because `str::len` (bytes) and `chars().count()` (code points) both lie for
/// non-ASCII text.
pub fn grapheme_count(input: &str) -> usize {
    input.graphemes(true).count()
}

/// Unicode-aware word count.
pub fn word_count(input: &str) -> usize {
    input.unicode_words().count()
}

/// Split into sentences using the UAX #29 sentence-boundary algorithm.
///
/// Unlike `unicode_sentences`, this keeps punctuation attached so downstream
/// heuristics can detect questions, but drops fragments with no words at all.
pub fn sentences(input: &str) -> impl Iterator<Item = &str> {
    input
        .split_sentence_bounds()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.unicode_words().next().is_some())
}

/// Collapse all runs of whitespace (including newlines) into single spaces.
pub fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Make a string safe to print to a terminal.
///
/// Export files are untrusted input: a conversation title is attacker-supplied
/// text that lands directly in our stdout. We strip C0/C1 control characters
/// (which can move the cursor, clear the screen, or set the window title) and
/// Unicode bidirectional *overrides* — the classic display-spoofing vector.
///
/// Legitimate directional *marks* (U+200E/U+200F) are preserved, because real
/// Hebrew and Arabic titles depend on them for correct rendering.
pub fn sanitize_display(input: &str) -> Cow<'_, str> {
    fn is_dangerous(ch: char) -> bool {
        matches!(ch,
            '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
        )
    }

    if !input.chars().any(is_dangerous) {
        return Cow::Borrowed(input);
    }
    Cow::Owned(input.chars().filter(|c| !is_dangerous(*c)).collect())
}

/// Make a string usable as a single path component.
///
/// Strips directory separators, control characters, and platform-reserved
/// characters, and never yields `.`, `..` or the empty string.
pub fn sanitize_filename(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    // Collapse the runs of separators that path-like input produces, then trim
    // the leading/trailing punctuation that would otherwise leave artefacts
    // such as `-..-etc-passwd` behind.
    let mut collapsed = String::with_capacity(cleaned.len());
    for ch in cleaned.chars() {
        if ch == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(ch);
    }
    let cleaned = collapsed
        .trim_matches(|c: char| c == '.' || c == '-' || c.is_whitespace())
        .to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "untitled".to_string()
    } else {
        truncate_graphemes(&cleaned, 120).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_grapheme_boundaries() {
        let s = "👨‍👩‍👧‍👦abc";
        assert_eq!(truncate_graphemes(s, 1), "👨‍👩‍👧‍👦…");
        assert_eq!(truncate_graphemes(s, 4), s);
        assert_eq!(truncate_graphemes(s, 0), "");
    }

    #[test]
    fn truncation_handles_hebrew() {
        let s = "איבוגה גמילה מאופיאטים";
        let cut = truncate_graphemes(s, 6);
        assert!(cut.ends_with(ELLIPSIS));
        assert!(s.starts_with(cut.trim_end_matches(ELLIPSIS).trim_end()));
    }

    #[test]
    fn no_truncation_when_short_enough() {
        assert!(matches!(truncate_graphemes("hi", 10), Cow::Borrowed("hi")));
    }

    #[test]
    fn sanitize_strips_control_and_bidi_overrides() {
        let hostile = "safe\u{202e}gnp.exe\u{7}\u{1b}[2J";
        let out = sanitize_display(hostile);
        assert!(!out.contains('\u{202e}'));
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("safe"));
    }

    #[test]
    fn sanitize_preserves_legitimate_rtl_marks_and_newlines() {
        let s = "שלום\u{200f} world\nsecond";
        assert!(matches!(sanitize_display(s), Cow::Borrowed(_)));
    }

    #[test]
    fn filenames_are_single_components() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_filename("/etc/shadow"), "etc-shadow");
        assert_eq!(sanitize_filename("  ..  "), "untitled");
        assert_eq!(sanitize_filename(""), "untitled");
        assert_eq!(sanitize_filename("."), "untitled");
        assert_eq!(sanitize_filename("context.md"), "context.md");
        assert_eq!(sanitize_filename("a/b\\c:d"), "a-b-c-d");
        assert!(!sanitize_filename("a/b\\c:d").contains(['/', '\\', ':']));
        // A sanitized name is always exactly one path component.
        for hostile in ["../../etc/passwd", "a/b", "\u{0}x", "..", "  "] {
            let safe = sanitize_filename(hostile);
            assert_eq!(
                std::path::Path::new(&safe).components().count(),
                1,
                "{hostile:?}"
            );
        }
    }

    #[test]
    fn sentence_splitting_keeps_questions() {
        let found: Vec<_> = sentences("We shipped it. Should we ship again? yes").collect();
        assert_eq!(found.len(), 3);
        assert!(found[1].ends_with('?'));
    }

    #[test]
    fn whitespace_collapse() {
        assert_eq!(collapse_whitespace("  a \n\n b  "), "a b");
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn counts_are_unicode_aware() {
        assert_eq!(grapheme_count("👨‍👩‍👧‍👦"), 1);
        assert_eq!(word_count("שלום עולם and hello"), 4);
    }
}
