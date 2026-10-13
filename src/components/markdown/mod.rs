//! Markdown syntax models and parse/serialize helpers shared by editor blocks.
//!
//! These modules supply document import, inline projection, native block rendering,
//! and HTML export. Blocks and the editor supply document context and own live editing.

/// Fenced-code language lookup and theme-independent highlight spans.
pub(crate) mod code_highlight;
/// Footnote syntax and document-wide resolution metadata.
pub(crate) mod footnote;
/// Conservative HTML classification, visual styles, and export sanitization.
pub(crate) mod html;
/// Markdown image syntax, reference definitions, and source classification.
pub(crate) mod image;
/// Inline Markdown parsing, editing, rendering caches, and serialization.
pub mod inline;
/// Link reference definitions and supported autolink targets.
pub(crate) mod link;
/// Routing of external multiline paste to plain lines or structural import.
pub(crate) mod paste;
/// Native table contents, runtime cell addresses, widths, and Markdown syntax.
pub mod table;
