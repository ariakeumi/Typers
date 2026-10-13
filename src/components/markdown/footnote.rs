//! Inline footnote references and resolved footnote registry state.

use std::collections::HashMap;

use gpui::EntityId;
use uuid::Uuid;

/// Inline footnote reference parsed from `[^id]` syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineFootnoteReference {
    /// Footnote identifier without the `[^` and `]` markers.
    pub id: String,
    /// One-based display number, if the referenced definition has been resolved.
    pub ordinal: Option<usize>,
    /// Document-wide zero-based occurrence index after resolution; initially zero.
    pub occurrence_index: usize,
}

/// Hit-test payload for a rendered footnote reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineFootnoteHit {
    /// Definition identifier used for navigation.
    pub id: String,
    /// One-based display number assigned in first-reference order.
    pub ordinal: usize,
    /// Document-wide occurrence index used to identify the reference.
    pub occurrence_index: usize,
}

impl InlineFootnoteReference {
    /// Reconstructs the literal reference, independent of its resolved number.
    pub(crate) fn raw_markdown(&self) -> String {
        format!("[^{}]", self.id)
    }

    /// Builds a navigation payload only when the definition has been resolved.
    pub(crate) fn hit(&self) -> Option<InlineFootnoteHit> {
        Some(InlineFootnoteHit {
            id: self.id.clone(),
            ordinal: self.ordinal?,
            occurrence_index: self.occurrence_index,
        })
    }
}

/// Location of the first resolved inline reference for one footnote id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FootnoteReferenceLocation {
    /// Runtime entity containing the first reference.
    pub(crate) entity_id: EntityId,
    /// Document-wide index of that reference, not an index within the entity.
    pub(crate) occurrence_index: usize,
}

/// Definition block and first-reference metadata for one footnote id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FootnoteDefinitionBinding {
    /// One-based number, absent until an eligible reference uses this definition.
    pub(crate) ordinal: Option<usize>,
    /// Runtime entity of the first eligible definition with this identifier.
    pub(crate) definition_entity_id: EntityId,
    /// First visible reference, used for back-navigation from the definition.
    pub(crate) first_reference: Option<FootnoteReferenceLocation>,
}

/// Document-wide registry that binds references to definition blocks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FootnoteRegistry {
    /// Eligible definitions keyed by their literal, case-sensitive identifiers.
    pub(crate) bindings: HashMap<String, FootnoteDefinitionBinding>,
    /// References in each persistent block's fragment order, including unresolved ones.
    pub(crate) block_occurrences: HashMap<Uuid, Vec<FootnoteResolvedOccurrence>>,
}

impl FootnoteRegistry {
    /// Looks up a definition binding without normalizing its identifier.
    pub(crate) fn binding(&self, id: &str) -> Option<&FootnoteDefinitionBinding> {
        self.bindings.get(id)
    }

    /// Returns the display number, or `None` for missing or unreferenced definitions.
    pub(crate) fn ordinal(&self, id: &str) -> Option<usize> {
        self.binding(id).and_then(|binding| binding.ordinal)
    }

    /// Borrows a block's ordered occurrences, or `None` if none were recorded.
    pub(crate) fn occurrences_for_block(
        &self,
        block_id: Uuid,
    ) -> Option<&[FootnoteResolvedOccurrence]> {
        self.block_occurrences.get(&block_id).map(Vec::as_slice)
    }
}

/// Resolution metadata for a reference stored per block for inline rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FootnoteResolvedOccurrence {
    /// Literal identifier from the inline reference.
    pub(crate) id: String,
    /// One-based display number, absent when no eligible definition exists.
    pub(crate) ordinal: Option<usize>,
    /// Document-wide zero-based index, counting unresolved references too.
    pub(crate) occurrence_index: usize,
}

/// Rejects empty identifiers, spaces, tabs, CR/LF, and `^`, `[`, or `]` delimiters.
pub(crate) fn is_valid_footnote_id(id: &str) -> bool {
    !id.is_empty()
        && !id
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '^' | '[' | ']'))
}

/// Parses a definition's first line after at most three spaces of indentation.
/// Returns the literal identifier and trailing content with at most one leading space removed.
pub(crate) fn parse_footnote_definition_head(line: &str) -> Option<(String, String)> {
    let trimmed_end = line.trim_end();
    let leading_spaces = trimmed_end.bytes().take_while(|b| *b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }

    let rest = &trimmed_end[leading_spaces..];
    let after_open = rest.strip_prefix("[^")?;
    let label_end = after_open.find("]:")?;
    let id = after_open[..label_end].to_string();
    if !is_valid_footnote_id(&id) {
        return None;
    }

    let remainder = after_open[label_end + 2..]
        .strip_prefix(' ')
        .unwrap_or(&after_open[label_end + 2..])
        .to_string();
    Some((id, remainder))
}

/// Parses an entire literal `[^id]` reference without trimming or normalizing its identifier.
pub(crate) fn parse_inline_footnote_reference(markdown: &str) -> Option<String> {
    let rest = markdown.strip_prefix("[^")?;
    let id = rest.strip_suffix(']')?;
    is_valid_footnote_id(id).then(|| id.to_string())
}

/// Converts decimal digits to Unicode superscripts, including zero.
pub(crate) fn superscript_ordinal(ordinal: usize) -> String {
    /// Superscript glyphs indexed by their decimal digit value.
    const SUPERSCRIPT_DIGITS: [char; 10] = [
        '\u{2070}', '\u{00B9}', '\u{00B2}', '\u{00B3}', '\u{2074}', '\u{2075}', '\u{2076}',
        '\u{2077}', '\u{2078}', '\u{2079}',
    ];

    ordinal
        .to_string()
        .chars()
        .filter_map(|ch| {
            ch.to_digit(10)
                .map(|digit| SUPERSCRIPT_DIGITS[digit as usize])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        is_valid_footnote_id, parse_footnote_definition_head, parse_inline_footnote_reference,
        superscript_ordinal,
    };

    #[test]
    fn validates_and_parses_reference_footnote_syntax() {
        assert!(is_valid_footnote_id("long-note"));
        assert!(!is_valid_footnote_id("bad id"));
        assert_eq!(
            parse_inline_footnote_reference("[^ref-1]"),
            Some("ref-1".to_string())
        );
        assert_eq!(
            parse_footnote_definition_head("[^ref-1]: body"),
            Some(("ref-1".to_string(), "body".to_string()))
        );
    }

    #[test]
    fn formats_superscript_ordinals() {
        assert_eq!(superscript_ordinal(1), "\u{00B9}");
        assert_eq!(superscript_ordinal(12), "\u{00B9}\u{00B2}");
    }
}
