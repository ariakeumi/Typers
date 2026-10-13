//! Document export helpers for HTML and PDF output.
//!
//! Export starts from the same Markdown text used by document saving. The
//! module owns format-specific rendering so editor code only chooses paths and
//! supplies the current theme.

use crate::fonts::FontSettings;
use crate::theme::Theme;

/// Markdown-to-HTML rendering shared by standalone and PDF export.
mod html;
/// Headless Chromium PDF generation.
mod pdf;

/// Export target selected from the app menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportFormat {
    /// Full HTML document with embedded theme CSS.
    Html,
    /// PDF bytes rendered from the themed HTML document.
    Pdf,
}

impl ExportFormat {
    /// File extension used for save-dialog defaults.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Pdf => "pdf",
        }
    }
}

pub(crate) use html::render_html_with_base_dir_and_fonts;

/// Renders Markdown as PDF bytes with the selected theme and font stacks.
pub(crate) fn render_pdf_with_fonts(
    markdown: &str,
    theme: &Theme,
    title: &str,
    base_path: Option<&std::path::Path>,
    fonts: &FontSettings,
) -> anyhow::Result<Vec<u8>> {
    pdf::render_pdf_with_fonts(markdown, theme, title, base_path, fonts)
}
