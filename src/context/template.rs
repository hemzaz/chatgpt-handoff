//! The `context.md` document skeleton and its Markdown rendering.
//!
//! The handoff document is a *fixed* 14-section form. Generators never choose
//! which sections exist — they only fill bodies. A section with nothing in it
//! is still emitted, carrying [`EMPTY_SECTION_BODY`], so that a model reading
//! the document can tell "we looked and found nothing" apart from "this slot
//! was never considered". Silently omitting a section would let the reader
//! infer facts from an absence that means nothing.

/// Body substituted at render time for a section whose body is blank.
pub const EMPTY_SECTION_BODY: &str = "_None identified._";

/// Title line of every handoff document.
pub const DOCUMENT_TITLE: &str = "# Conversation Handoff";

/// One `## heading` + body pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub heading: String,
    pub body: String,
}

/// A complete handoff document: an ordered list of sections.
///
/// Construct with [`ContextDocument::skeleton`] to get all of
/// [`SECTION_ORDER`] pre-created with empty bodies, then fill them with
/// [`ContextDocument::set_section`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextDocument {
    pub sections: Vec<Section>,
}

impl ContextDocument {
    /// A document containing every heading in [`SECTION_ORDER`], in order,
    /// with empty bodies.
    pub fn skeleton() -> Self {
        ContextDocument {
            sections: SECTION_ORDER
                .iter()
                .map(|heading| Section {
                    heading: (*heading).to_string(),
                    body: String::new(),
                })
                .collect(),
        }
    }

    /// Replace the body of an existing section.
    ///
    /// Unknown headings are ignored rather than appended: the section list is
    /// a contract, and a typo must not silently grow a 15th section.
    pub fn set_section(&mut self, heading: &str, body: impl Into<String>) {
        if let Some(section) = self.sections.iter_mut().find(|s| s.heading == heading) {
            section.body = body.into();
        }
    }

    /// Case-sensitive exact lookup by heading.
    pub fn section(&self, heading: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.heading == heading)
    }

    /// Render the whole document.
    ///
    /// Layout is exactly: the title, then for each section a blank line, the
    /// `## heading`, a blank line, and the body. Blank bodies become
    /// [`EMPTY_SECTION_BODY`]. Every line is right-trimmed and the output ends
    /// with exactly one newline, so the result is byte-stable and diff-clean.
    pub fn render_markdown(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str(DOCUMENT_TITLE);
        out.push('\n');
        for section in &self.sections {
            out.push_str("\n## ");
            out.push_str(section.heading.trim());
            out.push_str("\n\n");
            let body = section.body.trim();
            if body.is_empty() {
                out.push_str(EMPTY_SECTION_BODY);
            } else {
                push_right_trimmed(&mut out, body);
            }
            out.push('\n');
        }
        out
    }
}

/// Append `body` with every line right-trimmed, without a trailing newline.
fn push_right_trimmed(out: &mut String, body: &str) {
    let mut first = true;
    for line in body.lines() {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(line.trim_end());
    }
}

/// The mandated section order of `context.md`.
///
/// This is a spec contract, not a preference: the summarization prompt in
/// [`crate::context::prompt`] is generated from this list so the two can never
/// drift apart.
pub const SECTION_ORDER: &[&str] = &[
    "Conversation",
    "Purpose",
    "Important Background",
    "Established Facts",
    "User Preferences and Constraints",
    "Decisions Already Made",
    "Terminology and Entities",
    "Important Technical Details",
    "Key Conclusions",
    "Rejected / Superseded Approaches",
    "Current State",
    "Open Questions",
    "Recent Conversation",
    "Continuation Instructions",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_has_every_section_in_order() {
        let doc = ContextDocument::skeleton();
        let headings: Vec<&str> = doc.sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, SECTION_ORDER);
        assert_eq!(doc.sections.len(), 14);
    }

    #[test]
    fn blank_bodies_render_as_an_explicit_none() {
        let doc = ContextDocument::skeleton();
        let rendered = doc.render_markdown();
        for heading in SECTION_ORDER {
            assert!(
                rendered.contains(&format!("## {heading}\n\n{EMPTY_SECTION_BODY}\n")),
                "missing empty body for {heading}"
            );
        }
    }

    #[test]
    fn layout_is_title_then_blank_separated_sections() {
        let mut doc = ContextDocument::skeleton();
        doc.set_section("Purpose", "Ship the thing.");
        let rendered = doc.render_markdown();
        assert!(rendered.starts_with("# Conversation Handoff\n\n## Conversation\n\n"));
        assert!(rendered.contains("\n## Purpose\n\nShip the thing.\n\n## Important Background\n"));
    }

    #[test]
    fn exactly_one_trailing_newline_and_no_trailing_whitespace() {
        let mut doc = ContextDocument::skeleton();
        doc.set_section("Purpose", "  padded   \n  lines  \n");
        let rendered = doc.render_markdown();
        assert!(rendered.ends_with('\n'));
        assert!(!rendered.ends_with("\n\n"));
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end(), "trailing whitespace in {line:?}");
        }
    }

    #[test]
    fn section_lookup_is_case_sensitive_and_exact() {
        let mut doc = ContextDocument::skeleton();
        doc.set_section("Open Questions", "why?");
        assert_eq!(
            doc.section("Open Questions").map(|s| s.body.as_str()),
            Some("why?")
        );
        assert!(doc.section("open questions").is_none());
        assert!(doc.section("Open").is_none());
    }

    #[test]
    fn setting_an_unknown_heading_does_not_grow_the_document() {
        let mut doc = ContextDocument::skeleton();
        doc.set_section("Invented Section", "nope");
        assert_eq!(doc.sections.len(), SECTION_ORDER.len());
        assert!(doc.section("Invented Section").is_none());
    }
}
