//! Lightweight workspace panel state, file-tree scanning, and outline parsing.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::prelude::FluentBuilder;
use gpui::*;

use super::{BlockKind, Editor};
use crate::components::{InlineTextTree, parse_link_reference_definitions};
use crate::i18n::I18nStrings;
use crate::theme::Theme;

/// Asset path for directory nodes.
const FOLDER_ICON: &str = "icon/workspace/folder.svg";
/// Asset path for Markdown file nodes.
const MARKDOWN_ICON: &str = "icon/workspace/markdown.svg";
/// Fraction of viewport width requested by the workspace panel.
const WORKSPACE_PANEL_TARGET_RATIO: f32 = 0.15;
/// Lower width bound for the workspace panel, in pixels.
const WORKSPACE_PANEL_MIN_WIDTH: f32 = 270.0;
/// Upper width bound for the workspace panel, in pixels.
const WORKSPACE_PANEL_MAX_WIDTH: f32 = 360.0;
/// Height of each visible tree row, in pixels.
const WORKSPACE_NODE_HEIGHT: f32 = 28.0;
/// Additional left inset per tree depth, in pixels.
const WORKSPACE_NODE_INDENT: f32 = 18.0;

/// The active workspace panel view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WorkspaceTab {
    /// Flat document cards with short text previews.
    #[default]
    Documents,
    /// Directory and Markdown file tree.
    Files,
    /// Headings parsed from the current document.
    Outline,
}

/// Navigation target represented by a workspace tree node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceTreeKind {
    /// A directory that may contain further visible nodes.
    Directory(PathBuf),
    /// An openable Markdown file.
    MarkdownFile(PathBuf),
    /// A heading at a zero-based source line and ATX level.
    Heading { line: usize, level: u8 },
}

/// A file-tree or outline entry with stable identity and nested children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceTreeNode {
    /// Stable path- or source-line-based node identifier.
    id: String,
    /// Text shown for the node.
    label: String,
    /// Navigation action and underlying target.
    kind: WorkspaceTreeKind,
    /// Descendants displayed when the node is expanded.
    children: Vec<WorkspaceTreeNode>,
}

/// Selected file path or outline node identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkspaceSelection {
    /// Selected file in the Files tab.
    File(PathBuf),
    /// Selected node ID in the Outline tab.
    Outline(String),
}

/// Cached workspace trees and interaction state for one editor window.
#[derive(Default)]
pub(super) struct WorkspaceState {
    /// Whether the drawer is visible.
    pub(super) is_open: bool,
    /// Currently displayed workspace tab.
    pub(super) active_tab: WorkspaceTab,
    /// Folder explicitly opened by the user, retained while switching files.
    root_override: Option<PathBuf>,
    previews: HashMap<PathBuf, String>,
    panel_width: Option<f32>,
    pub(super) resizing: bool,
    /// Parent directory of the current document, when it has a path.
    root: Option<PathBuf>,
    /// Last scanned file tree.
    file_tree: Option<WorkspaceTreeNode>,
    /// File scan failure shown in the panel.
    file_error: Option<String>,
    /// Headings parsed from the current source.
    outline_tree: Vec<WorkspaceTreeNode>,
    /// Source text corresponding to `outline_tree`.
    outline_source: Option<String>,
    outline_initialized: bool,
    /// IDs of expanded directory and heading nodes.
    expanded: HashSet<String>,
    /// Current file or heading selection.
    selected: Option<WorkspaceSelection>,
}

impl Editor {
    /// Toggles the workspace drawer and refreshes its models when opening.
    pub(crate) fn toggle_workspace_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.is_open {
            self.workspace.is_open = false;
        } else {
            self.close_menu_bar(cx);
            self.dismiss_contextual_overlays(cx);
            self.workspace.is_open = true;
            self.sync_workspace_models(cx);
            window.activate_window();
        }
        cx.notify();
    }

    pub(super) fn show_workspace_tab(
        &mut self,
        tab: WorkspaceTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.is_open = true;
        self.set_workspace_tab(tab, cx);
        self.sync_workspace_models(cx);
        window.activate_window();
        cx.notify();
    }

    pub(super) fn prompt_workspace_folder(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(super::writing::localized(cx, "Open Folder", "打开文件夹").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |editor, cx| {
                    editor.workspace.root_override = Some(path);
                    editor.workspace.root = None;
                    editor.workspace.file_tree = None;
                    editor.workspace.is_open = true;
                    editor.workspace.active_tab = WorkspaceTab::Files;
                    editor.sync_workspace_models(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn workspace_panel_width(&self, viewport: f32) -> f32 {
        self.workspace
            .panel_width
            .unwrap_or_else(|| workspace_panel_width_for_viewport(viewport))
            .clamp(180.0, (viewport - 240.0).max(180.0))
    }

    pub(super) fn on_workspace_resize_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workspace.resizing {
            return;
        }
        self.workspace.panel_width = Some(f32::from(event.position.x).clamp(
            180.0,
            (f32::from(window.viewport_size().width) - 240.0).max(180.0),
        ));
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn on_workspace_resize_end(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.resizing {
            self.workspace.resizing = false;
            cx.notify();
        }
    }

    /// Handles the workspace toggle application action.
    pub(crate) fn on_toggle_workspace_action(
        &mut self,
        _: &crate::components::ToggleWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_workspace_drawer(window, cx);
    }

    /// Invalidates workspace caches after the current document path changes.
    pub(super) fn sync_workspace_after_document_path_change(&mut self, cx: &mut Context<Self>) {
        self.workspace.root = None;
        self.workspace.file_tree = None;
        self.workspace.file_error = None;
        self.workspace.outline_source = None;
        self.workspace.outline_initialized = false;
        if self.workspace.is_open {
            self.sync_workspace_models(cx);
        }
    }

    /// Refreshes both file and outline views from current editor state.
    fn sync_workspace_models(&mut self, cx: &mut Context<Self>) {
        self.sync_workspace_file_tree();
        self.sync_workspace_outline(cx);
        if self.workspace.active_tab == WorkspaceTab::Outline {
            let offset = self.capture_source_selection_snapshot(cx).range.start;
            let line = self.workspace.outline_source.as_ref().map_or(0, |source| {
                source.as_bytes()[..offset.min(source.len())]
                    .iter()
                    .filter(|&&ch| ch == b'\n')
                    .count()
            });
            self.workspace.selected = outline_node_at_line(&self.workspace.outline_tree, line)
                .map(WorkspaceSelection::Outline);
        }
    }

    /// Uses the current file's parent directory as the workspace root.
    fn workspace_root_for_current_file(&self) -> Option<PathBuf> {
        self.workspace
            .root_override
            .clone()
            .or_else(|| self.file_path.as_ref()?.parent().map(Path::to_path_buf))
    }

    /// Scans a changed root and updates the selected file path.
    fn sync_workspace_file_tree(&mut self) {
        let next_root = self.workspace_root_for_current_file();
        if self.workspace.root == next_root && self.workspace.file_tree.is_some() {
            if self.workspace.active_tab != WorkspaceTab::Outline {
                self.workspace.selected = self
                    .file_path
                    .as_ref()
                    .map(|path| WorkspaceSelection::File(path.clone()));
            }
            return;
        }

        self.workspace.root = next_root.clone();
        self.workspace.file_tree = None;
        self.workspace.previews.clear();
        self.workspace.file_error = None;

        let Some(root) = next_root else {
            self.workspace.selected = None;
            return;
        };

        // Validate the root path
        if root.as_os_str().is_empty() {
            self.workspace.file_error = Some("Invalid workspace path: empty path".to_string());
            self.workspace.selected = None;
            return;
        }

        match scan_workspace_dir(&root) {
            Ok(tree) => {
                self.workspace.expanded.insert(tree.id.clone());
                self.workspace.file_tree = Some(tree);
                self.workspace.selected = self
                    .file_path
                    .as_ref()
                    .map(|path| WorkspaceSelection::File(path.clone()));
            }
            Err(err) => {
                self.workspace.file_error = Some(err.to_string());
            }
        }
    }

    /// Rebuilds the heading outline only when serialized source changed.
    fn sync_workspace_outline(&mut self, cx: &mut Context<Self>) {
        let source = self.serialized_document_text(cx);
        if self.workspace.outline_source.as_deref() == Some(source.as_str()) {
            return;
        }

        let outline = build_outline_tree(&source);
        prune_outline_state(&mut self.workspace, &outline);
        if !self.workspace.outline_initialized {
            collect_node_ids(&outline, &mut self.workspace.expanded);
            self.workspace.outline_initialized = true;
        }
        self.workspace.outline_tree = outline;
        self.workspace.outline_source = Some(source);
    }

    /// Selects a panel tab and refreshes its models.
    fn set_workspace_tab(&mut self, tab: WorkspaceTab, cx: &mut Context<Self>) {
        if self.workspace.active_tab != tab {
            self.workspace.active_tab = tab;
            self.sync_workspace_models(cx);
            cx.notify();
        }
    }

    /// Toggles expansion of a directory or heading node by ID.
    fn toggle_workspace_node(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.workspace.expanded.remove(id) {
            self.workspace.expanded.insert(id.to_string());
        }
        cx.notify();
    }

    /// Selects an outline heading and moves editor focus to its source line.
    fn select_outline_node(&mut self, id: String, line: usize, cx: &mut Context<Self>) {
        self.workspace.selected = Some(WorkspaceSelection::Outline(id));
        self.reveal_outline_heading(line, cx);
        cx.notify();
    }

    /// Maps an outline heading's source line to its owning block, then focuses
    /// and scrolls the editor so the body jumps to that heading.
    ///
    /// The outline is parsed from `serialized_document_text`, whose line offsets
    /// align with the per-block source spans produced by
    /// `build_source_target_mappings`, so the byte offset of the heading line
    /// lands exactly on the heading block's source range.
    fn reveal_outline_heading(&mut self, line: usize, cx: &mut Context<Self>) {
        let source = self.serialized_document_text(cx);
        let target = source
            .lines()
            .take(line)
            .map(|line| line.len() + 1)
            .sum::<usize>();

        let mappings = self.build_source_target_mappings(cx);
        let entity_id = mappings
            .iter()
            .find(|mapping| mapping.full_source_range.contains(&target))
            .or_else(|| {
                mappings.iter().min_by_key(|mapping| {
                    Self::source_offset_distance(&mapping.full_source_range, target)
                })
            })
            .map(|mapping| mapping.entity.entity_id());

        let Some(entity_id) = entity_id else {
            return;
        };
        let Some(block) = self.focusable_entity_by_id(entity_id) else {
            return;
        };
        self.focus_block_range(&block, 0..0, cx);
    }

    /// Selects a file and routes it through the guarded document replacement flow.
    fn open_workspace_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.selected = Some(WorkspaceSelection::File(path.clone()));
        self.request_dropped_markdown_replace(path, window, cx);
    }

    /// Renders the active workspace tab when the drawer is open.
    pub(super) fn render_workspace_panel(
        &mut self,
        theme: &Theme,
        strings: &I18nStrings,
        panel_width: f32,
        titlebar_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.workspace.is_open {
            return None;
        }

        self.sync_workspace_models(cx);
        let editor = cx.entity().downgrade();
        let c = &theme.colors;
        let d = &theme.dimensions;
        let header = match self.workspace.active_tab {
            WorkspaceTab::Documents => {
                super::writing::localized(cx, "Documents", "文档").to_string()
            }
            WorkspaceTab::Files => self
                .workspace
                .root
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| strings.workspace_tab_files.clone()),
            WorkspaceTab::Outline => strings.workspace_tab_outline.clone(),
        };
        let documents = self.workspace.active_tab == WorkspaceTab::Documents;
        let body = match self.workspace.active_tab {
            WorkspaceTab::Documents => {
                self.render_workspace_document_list(theme, strings, &editor, cx)
            }
            WorkspaceTab::Files => self.render_workspace_files_tree(theme, strings, &editor),
            WorkspaceTab::Outline => self.render_workspace_outline_tree(theme, strings, &editor),
        };

        Some(
            div()
                .id("workspace-panel")
                .h_full()
                .w(px(panel_width))
                .flex()
                .flex_col()
                .flex_shrink_0()
                .bg(if c.editor_background.l > 0.5 {
                    rgba(0xfafafaff).into()
                } else {
                    c.dialog_surface
                })
                .border_r(px(d.dialog_border_width))
                .border_color(c.dialog_border)
                .relative()
                .children((titlebar_height > 0.0).then(|| {
                    div()
                        .id("workspace-titlebar-drag")
                        .h(px(titlebar_height))
                        .w_full()
                        .flex_shrink_0()
                        .window_control_area(WindowControlArea::Drag)
                }))
                .child(
                    div()
                        .h(px(32.0))
                        .flex_shrink_0()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(14.0))
                        .text_color(c.dialog_body)
                        .child(header),
                )
                .child(
                    div()
                        .id("workspace-panel-scroll")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .px(px(if documents { 0.0 } else { 8.0 }))
                        .py(px(if documents { 0.0 } else { 10.0 }))
                        .child(body),
                )
                .child(
                    div()
                        .id("workspace-resize-handle")
                        .absolute()
                        .right(px(-3.0))
                        .top_0()
                        .bottom_0()
                        .w(px(6.0))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|editor, _, _, cx| {
                                editor.workspace.resizing = true;
                                cx.stop_propagation();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_workspace_document_list(
        &mut self,
        theme: &Theme,
        strings: &I18nStrings,
        editor: &WeakEntity<Editor>,
        cx: &App,
    ) -> AnyElement {
        let Some(tree) = self.workspace.file_tree.as_ref() else {
            return self.render_workspace_files_tree(theme, strings, editor);
        };
        let files = tree
            .children
            .iter()
            .filter_map(|node| match &node.kind {
                WorkspaceTreeKind::MarkdownFile(path) => Some((path.clone(), node.label.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut cards = Vec::new();
        for (path, label) in files {
            let preview = self
                .workspace
                .previews
                .entry(path.clone())
                .or_insert_with(|| {
                    let mut bytes = Vec::new();
                    if let Ok(file) = fs::File::open(&path) {
                        let _ = file.take(8192).read_to_end(&mut bytes);
                    }
                    document_preview(&String::from_utf8_lossy(&bytes))
                })
                .clone();
            let selected = self.file_path.as_ref() == Some(&path);
            let preview = if selected {
                document_preview(&self.serialized_document_text(cx))
            } else {
                preview
            };
            let open_editor = editor.clone();
            let c = &theme.colors;
            cards.push(
                div()
                    .id((
                        "workspace-document",
                        stable_node_hash(&path.to_string_lossy()),
                    ))
                    .w_full()
                    .px(px(24.0))
                    .py(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .border_b(px(1.0))
                    .border_color(c.dialog_border)
                    .bg(if selected {
                        c.status_bar_button_hover
                    } else {
                        hsla(0.0, 0.0, 0.0, 0.0)
                    })
                    .hover(|style| style.bg(c.status_bar_button_hover))
                    .cursor_pointer()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .text_color(c.dialog_body)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .line_height(px(20.0))
                            .text_color(c.dialog_muted)
                            .h(px(40.0))
                            .overflow_hidden()
                            .child(preview),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = open_editor.update(cx, |editor, cx| {
                            editor.open_workspace_file(path.clone(), window, cx)
                        });
                    })
                    .into_any_element(),
            );
        }
        if cards.is_empty() {
            return self.render_workspace_empty_state("", &strings.workspace_empty_files, theme);
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(cards)
            .into_any_element()
    }

    /// Renders the file tree or its empty/error state.
    fn render_workspace_files_tree(
        &self,
        theme: &Theme,
        strings: &I18nStrings,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        if self.workspace.root.is_none() {
            return self.render_workspace_empty_state(
                &strings.workspace_no_file_title,
                &strings.workspace_no_file_message,
                theme,
            );
        }

        if let Some(error) = self.workspace.file_error.as_ref() {
            return self.render_workspace_empty_state(
                &strings.workspace_scan_failed_title,
                error,
                theme,
            );
        }

        let Some(root) = self.workspace.file_tree.as_ref() else {
            return self.render_workspace_empty_state("", &strings.workspace_empty_files, theme);
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .children(self.render_workspace_nodes(std::slice::from_ref(root), 0, theme, editor))
            .into_any_element()
    }

    /// Renders the heading outline or its empty state.
    fn render_workspace_outline_tree(
        &self,
        theme: &Theme,
        strings: &I18nStrings,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        if self.workspace.outline_tree.is_empty() {
            return self.render_workspace_empty_state("", &strings.workspace_empty_outline, theme);
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .children(self.render_workspace_nodes(&self.workspace.outline_tree, 0, theme, editor))
            .into_any_element()
    }

    /// Renders a centered workspace empty-state message.
    fn render_workspace_empty_state(
        &self,
        title: &str,
        message: &str,
        theme: &Theme,
    ) -> AnyElement {
        let c = &theme.colors;
        let t = &theme.typography;
        let title = (!title.is_empty()).then(|| {
            div()
                .text_size(px(t.text_size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(c.text_default)
                .child(title.to_string())
        });

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .px(px(22.0))
            .text_align(TextAlign::Center)
            .children(title)
            .child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(t.text_size * t.text_line_height))
                    .text_color(c.dialog_muted)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    /// Flattens expanded nodes into visible rows at their nesting depths.
    fn render_workspace_nodes(
        &self,
        nodes: &[WorkspaceTreeNode],
        depth: usize,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for node in nodes {
            elements.push(self.render_workspace_node(node, depth, theme, editor));
            if !node.children.is_empty() && self.workspace.expanded.contains(&node.id) {
                elements.extend(self.render_workspace_nodes(
                    &node.children,
                    depth + 1,
                    theme,
                    editor,
                ));
            }
        }
        elements
    }

    /// Renders a file, directory, or heading row with its navigation handler.
    fn render_workspace_node(
        &self,
        node: &WorkspaceTreeNode,
        depth: usize,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        let c = &theme.colors;
        let is_heading = matches!(node.kind, WorkspaceTreeKind::Heading { .. });
        let is_expanded = self.workspace.expanded.contains(&node.id);
        let has_children = !node.children.is_empty();
        let selected = match (&self.workspace.selected, &node.kind) {
            (Some(WorkspaceSelection::File(selected)), WorkspaceTreeKind::MarkdownFile(path)) => {
                selected == path
            }
            (Some(WorkspaceSelection::Outline(selected)), _) => selected == &node.id,
            _ => false,
        };
        let node_id = node.id.clone();
        let click_editor = editor.clone();
        let click_kind = node.kind.clone();
        let arrow_node_id = node.id.clone();
        let arrow_editor = editor.clone();
        let arrow = if has_children {
            if is_expanded { "⌄" } else { "›" }
        } else {
            ""
        };

        let icon = match &node.kind {
            WorkspaceTreeKind::Directory(_) => Some((FOLDER_ICON, c.dialog_muted)),
            WorkspaceTreeKind::MarkdownFile(_) => Some((MARKDOWN_ICON, c.dialog_muted)),
            WorkspaceTreeKind::Heading { .. } => None,
        };

        let label_color = if selected {
            c.text_default
        } else if is_heading {
            c.dialog_body
        } else {
            c.dialog_muted
        };

        let mut arrow_el = div()
            .w(px(14.0))
            .h(px(22.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(c.dialog_muted)
            .child(arrow);
        if has_children {
            arrow_el = arrow_el.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                move |_event, _window, cx| {
                    let _ = arrow_editor.update(cx, |editor, cx| {
                        editor.toggle_workspace_node(&arrow_node_id, cx);
                    });
                    cx.stop_propagation();
                },
            );
        }

        div()
            .id(("workspace-node", stable_node_hash(&node.id)))
            .min_h(px(if is_heading {
                32.0
            } else {
                WORKSPACE_NODE_HEIGHT
            }))
            .flex_shrink_0()
            .py(px(if is_heading { 5.0 } else { 3.0 }))
            .w_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .when(is_heading, |row| row.items_start())
            .gap(px(6.0))
            .pl(px(8.0 + depth as f32 * WORKSPACE_NODE_INDENT))
            .pr(px(8.0))
            .rounded(px(3.0))
            .bg(if selected {
                c.selection
            } else {
                hsla(0.0, 0.0, 0.0, 0.0)
            })
            .hover(|this| this.bg(c.dialog_secondary_button_hover))
            .cursor_pointer()
            .child(arrow_el)
            .children(icon.map(|(path, color)| {
                svg()
                    .path(path)
                    .size(px(16.0))
                    .flex_shrink_0()
                    .text_color(color)
                    .into_any_element()
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .when(!is_heading, |label| label.truncate())
                    .text_size(px(14.0))
                    .line_height(px(22.0))
                    .text_color(label_color)
                    .child(node.label.clone()),
            )
            .on_click(move |_event, window, cx| {
                let node_id = node_id.clone();
                let click_kind = click_kind.clone();
                let _ = click_editor.update(cx, |editor, cx| match click_kind {
                    WorkspaceTreeKind::Directory(_) => editor.toggle_workspace_node(&node_id, cx),
                    WorkspaceTreeKind::MarkdownFile(path) => {
                        editor.open_workspace_file(path, window, cx);
                    }
                    WorkspaceTreeKind::Heading { line, .. } => {
                        editor.select_outline_node(node_id, line, cx);
                    }
                });
            })
            .into_any_element()
    }
}

fn outline_node_at_line(nodes: &[WorkspaceTreeNode], line: usize) -> Option<String> {
    let mut selected = None;
    for node in nodes {
        if let WorkspaceTreeKind::Heading { line: heading, .. } = node.kind {
            if heading > line {
                break;
            }
            selected = Some(node.id.clone());
            if let Some(child) = outline_node_at_line(&node.children, line) {
                selected = Some(child);
            }
        }
    }
    selected
}

fn document_preview(source: &str) -> String {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(2)
        .map(|line| line.trim_start_matches(['#', '>', ' ']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Recognizes `.md` files for the workspace tree, ignoring extension case.
fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("md"))
}

/// Recursively scans directories, retaining directories and `.md` files.
///
/// Directories sort before files; entries within each group sort by lowercase label.
fn scan_workspace_dir(path: &Path) -> Result<WorkspaceTreeNode> {
    let mut children = Vec::new();
    for entry in
        fs::read_dir(path).with_context(|| format!("failed to read '{}'", path.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            children.push(scan_workspace_dir(&entry_path)?);
        } else if file_type.is_file() && is_markdown_file(&entry_path) {
            children.push(WorkspaceTreeNode {
                id: file_node_id(&entry_path),
                label: file_label(&entry_path),
                kind: WorkspaceTreeKind::MarkdownFile(entry_path),
                children: Vec::new(),
            });
        }
    }

    children.sort_by(|left, right| {
        let left_dir = matches!(left.kind, WorkspaceTreeKind::Directory(_));
        let right_dir = matches!(right.kind, WorkspaceTreeKind::Directory(_));
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
    });

    Ok(WorkspaceTreeNode {
        id: file_node_id(path),
        label: file_label(path),
        kind: WorkspaceTreeKind::Directory(path.to_path_buf()),
        children,
    })
}

/// Uses the final path component as the displayed file or directory name.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Builds a path-based ID shared by file and directory nodes.
fn file_node_id(path: &Path) -> String {
    format!("file:{}", path.to_string_lossy())
}

/// Hashes a node ID for GPUI element identity.
fn stable_node_hash(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

/// Clamps the panel's target viewport fraction to its pixel width bounds.
pub(super) fn workspace_panel_width_for_viewport(viewport_width: f32) -> f32 {
    let target = viewport_width * WORKSPACE_PANEL_TARGET_RATIO;
    target.clamp(WORKSPACE_PANEL_MIN_WIDTH, WORKSPACE_PANEL_MAX_WIDTH)
}

/// Removes expansion and selection IDs for headings absent from the new outline.
fn prune_outline_state(workspace: &mut WorkspaceState, outline: &[WorkspaceTreeNode]) {
    let mut current_ids = HashSet::new();
    collect_node_ids(outline, &mut current_ids);
    workspace
        .expanded
        .retain(|id| !is_outline_node_id(id) || current_ids.contains(id));

    if matches!(
        &workspace.selected,
        Some(WorkspaceSelection::Outline(id)) if !current_ids.contains(id)
    ) {
        workspace.selected = None;
    }
}

/// Collects every node ID recursively.
fn collect_node_ids(nodes: &[WorkspaceTreeNode], ids: &mut HashSet<String>) {
    for node in nodes {
        ids.insert(node.id.clone());
        collect_node_ids(&node.children, ids);
    }
}

/// Distinguishes heading IDs from file-tree IDs.
fn is_outline_node_id(id: &str) -> bool {
    id.starts_with("outline:")
}

/// Builds a nested ATX-heading outline while ignoring fenced code lines.
fn build_outline_tree(markdown: &str) -> Vec<WorkspaceTreeNode> {
    let mut roots = Vec::new();
    let mut stack: Vec<(u8, Vec<usize>)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let references = parse_link_reference_definitions(markdown);

    for (line_index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some((marker, len)) = fence {
            if is_closing_fence(trimmed, marker, len) {
                fence = None;
            }
            continue;
        }

        if let Some(next_fence) = opening_fence(trimmed) {
            fence = Some(next_fence);
            continue;
        }

        let Some((level, title)) = BlockKind::parse_atx_heading_line(line) else {
            continue;
        };

        while stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= level)
        {
            stack.pop();
        }

        let node = WorkspaceTreeNode {
            id: format!("outline:{line_index}"),
            label: InlineTextTree::from_markdown_with_link_references(&title, &references)
                .visible_text(),
            kind: WorkspaceTreeKind::Heading {
                line: line_index,
                level,
            },
            children: Vec::new(),
        };

        let siblings = if let Some((_, parent_path)) = stack.last() {
            children_at_path_mut(&mut roots, parent_path)
        } else {
            &mut roots
        };
        siblings.push(node);

        let mut node_path = stack
            .last()
            .map(|(_, path)| path.clone())
            .unwrap_or_default();
        node_path.push(siblings.len() - 1);
        stack.push((level, node_path));
    }

    roots
}

/// Returns the child vector at an index path through the outline tree.
fn children_at_path_mut<'a>(
    nodes: &'a mut Vec<WorkspaceTreeNode>,
    path: &[usize],
) -> &'a mut Vec<WorkspaceTreeNode> {
    let mut current = nodes;
    for &index in path {
        current = &mut current[index].children;
    }
    current
}

/// Recognizes a backtick or tilde fence of at least three markers.
fn opening_fence(trimmed: &str) -> Option<(char, usize)> {
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    (len >= 3).then_some((marker, len))
}

/// Accepts a matching fence run at least as long as the opener, with no trailing content.
fn is_closing_fence(trimmed: &str, marker: char, len: usize) -> bool {
    let count = trimmed.chars().take_while(|ch| *ch == marker).count();
    count >= len && trimmed[count..].trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::{
        WorkspaceSelection, WorkspaceState, WorkspaceTreeKind, build_outline_tree,
        prune_outline_state, scan_workspace_dir, workspace_panel_width_for_viewport,
    };
    use std::fs;

    use crate::components::{BlockKind, Editor};
    use gpui::{AppContext, TestAppContext};

    #[gpui::test]
    async fn opened_folder_survives_document_switches_and_sidebar_width_is_bounded(
        cx: &mut TestAppContext,
    ) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("a.md"), "# A").unwrap();
        let nested = root.path().join("nested/b.md");
        fs::write(&nested, "# B").unwrap();
        let editor = cx.new(|cx| Editor::from_markdown(cx, "# B".into(), Some(nested)));
        editor.update(cx, |editor, cx| {
            editor.workspace.root_override = Some(root.path().to_path_buf());
            editor.sync_workspace_models(cx);
            assert_eq!(editor.workspace.root.as_deref(), Some(root.path()));
            editor.file_path = Some(root.path().join("a.md"));
            editor.sync_workspace_after_document_path_change(cx);
            editor.sync_workspace_models(cx);
            assert_eq!(editor.workspace.root.as_deref(), Some(root.path()));
            editor.workspace.panel_width = Some(700.0);
            assert_eq!(editor.workspace_panel_width(800.0), 560.0);
            assert_eq!(editor.workspace_panel_width(400.0), 180.0);
        });
    }

    #[test]
    fn workspace_scan_keeps_dirs_and_md_files_only() {
        let root =
            std::env::temp_dir().join(format!("velotype-workspace-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).expect("create dirs");
        fs::write(root.join("a.md"), "a").expect("write md");
        fs::write(root.join("a.txt"), "ignored").expect("write txt");
        fs::write(root.join("nested").join("b.md"), "b").expect("write nested md");

        let tree = scan_workspace_dir(&root).expect("scan tree");
        let labels = tree
            .children
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["nested", "a.md"]);
        assert!(matches!(
            tree.children[0].kind,
            WorkspaceTreeKind::Directory(_)
        ));
        assert!(matches!(
            tree.children[1].kind,
            WorkspaceTreeKind::MarkdownFile(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn outline_tree_skips_headings_inside_fenced_code() {
        let outline = build_outline_tree(
            "# Root\n\n```md\n# ignored\n```\n\n## Child\n\n### Grandchild\n\n# Next",
        );

        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].label, "Root");
        assert_eq!(outline[0].children[0].label, "Child");
        assert_eq!(outline[0].children[0].children[0].label, "Grandchild");
        assert_eq!(outline[1].label, "Next");
    }

    #[test]
    fn outline_labels_show_formatted_text_and_keep_source_locations() {
        let outline = build_outline_tree(
            "# **使用** pmset 禁用 USB 唤醒\n\n\
             ## *嵌套 **加粗***、~~删除~~和 `code`\n\n\
             ### [**链接**](https://example.com) 与 [参考][ref]\n\n\
             [ref]: https://example.com/reference",
        );

        let root = &outline[0];
        assert_eq!(root.label, "使用 pmset 禁用 USB 唤醒");
        assert_eq!(root.id, "outline:0");
        assert_eq!(root.kind, WorkspaceTreeKind::Heading { line: 0, level: 1 });
        let child = &root.children[0];
        assert_eq!(child.label, "嵌套 加粗、删除和 code");
        assert_eq!(child.kind, WorkspaceTreeKind::Heading { line: 2, level: 2 });
        assert_eq!(child.children[0].label, "链接 与 参考");
        assert_eq!(child.children[0].id, "outline:4");
    }

    #[test]
    fn outline_labels_preserve_literal_markers() {
        let outline = build_outline_tree(
            "# `**literal**` 与 \\*星号\\*\n\n## **尚未闭合\n\n### file_name 和 2 * 3",
        );

        assert_eq!(outline[0].label, "**literal** 与 *星号*");
        assert_eq!(outline[0].children[0].label, "**尚未闭合");
        assert_eq!(
            outline[0].children[0].children[0].label,
            "file_name 和 2 * 3"
        );
    }

    #[test]
    fn outline_expansion_state_is_not_auto_populated_and_prunes_stale_ids() {
        let outline = build_outline_tree("# Root\n\n## Child\n\n# Next");
        let mut fresh = WorkspaceState::default();
        prune_outline_state(&mut fresh, &outline);
        assert!(fresh.expanded.is_empty());

        let mut existing = WorkspaceState::default();
        existing.expanded.insert("outline:0".to_string());
        existing.expanded.insert("outline:999".to_string());
        existing
            .expanded
            .insert("workspace-dir:C:/docs".to_string());
        existing.selected = Some(WorkspaceSelection::Outline("outline:999".to_string()));

        prune_outline_state(&mut existing, &outline);

        assert!(existing.expanded.contains("outline:0"));
        assert!(existing.expanded.contains("workspace-dir:C:/docs"));
        assert!(!existing.expanded.contains("outline:999"));
        assert_eq!(existing.selected, None);
    }

    #[test]
    fn workspace_panel_width_uses_ratio_with_bounds() {
        assert_eq!(workspace_panel_width_for_viewport(1000.0), 270.0);
        assert_eq!(workspace_panel_width_for_viewport(2000.0), 300.0);
        assert_eq!(workspace_panel_width_for_viewport(4000.0), 360.0);
    }

    #[gpui::test]
    async fn outline_node_click_focuses_and_queues_scroll_for_target_heading(
        cx: &mut TestAppContext,
    ) {
        let markdown =
            "# Alpha\n\nPara under alpha.\n\n## Beta\n\nPara under beta.\n\n# Gamma".to_string();
        let editor = cx.new(|cx| Editor::from_markdown(cx, markdown, None));

        editor.update(cx, |editor, cx| {
            let beta = editor
                .document
                .visible_blocks()
                .iter()
                .find(|visible| {
                    matches!(
                        visible.entity.read(cx).kind(),
                        BlockKind::Heading { level: 2 }
                    )
                })
                .expect("## Beta heading")
                .entity
                .clone();

            // The outline is parsed from the same serialized text, where
            // "## Beta" lands on line index 4.
            editor.select_outline_node("outline:4".into(), 4, cx);

            assert_eq!(editor.pending_focus, Some(beta.entity_id()));
            assert!(editor.pending_scroll_active_block_into_view);
        });
    }
}
