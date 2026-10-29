//! Native writing tools. Search and formatting share the editor's canonical
//! Markdown offsets, so changing modes never creates a second document buffer.

use gpui::*;
use pulldown_cmark::{Event, Options, Parser, TagEnd};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

use super::workspace::WorkspaceTab;
use super::{Editor, HistoryEntry, UndoSelectionSnapshot};
use crate::components::single_line_input::{InputChanged, SingleLineInput};
use crate::components::{UndoCaptureKind, WritingAction, WritingCommand};
use crate::i18n::I18nManager;
use crate::theme::Theme;

#[derive(Default)]
pub(super) struct WritingState {
    pub search_open: bool,
    replace_open: bool,
    query: Option<Entity<SingleLineInput>>,
    replacement: Option<Entity<SingleLineInput>>,
    matches: Vec<Range<usize>>,
    current: Option<usize>,
    searched_source: String,
    searched_query: String,
    searched_entities: Vec<EntityId>,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    search_error: bool,
    cached_statistics: Option<(String, DocumentStatistics)>,
    pub focus_mode: bool,
    pub typewriter_mode: bool,
    pub read_only: bool,
    pub statistics_open: bool,
    /// Zero means actual size; each step changes document text by ten percent.
    pub zoom_steps: i8,
}

/// Native menu checkmarks follow the active window, including when several
/// documents have different writing modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WritingMenuState {
    focus: bool,
    typewriter: bool,
    read_only: bool,
    source: bool,
    sidebar: bool,
    outline: bool,
    documents: bool,
    files: bool,
    fullscreen: bool,
}
impl Global for WritingMenuState {}
impl WritingMenuState {
    pub(crate) fn checked(self, command: WritingCommand) -> bool {
        use WritingCommand::*;
        match command {
            FocusMode => self.focus,
            TypewriterMode => self.typewriter,
            ReadOnly => self.read_only,
            SourceMode => self.source,
            Sidebar => self.sidebar,
            Outline => self.outline,
            Documents => self.documents,
            Files => self.files,
            FullScreen => self.fullscreen,
            _ => false,
        }
    }
}

pub(crate) fn localized<'a>(cx: &App, en: &'a str, zh: &'a str) -> &'a str {
    if cx
        .try_global::<I18nManager>()
        .is_some_and(|m| m.current_language_id().starts_with("zh"))
    {
        zh
    } else {
        en
    }
}

fn action(command: WritingCommand) -> WritingAction {
    WritingAction { command }
}

pub(crate) fn install_writing_keybindings(cx: &mut App) {
    use WritingCommand::*;
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let mut bindings = Vec::new();
    for (suffix, command) in [
        ("f", Find),
        ("h", Replace),
        ("g", FindNext),
        ("shift-g", FindPrevious),
        ("shift-l", Sidebar),
        ("shift-o", OpenFolder),
        ("shift-1", FocusMode),
        ("shift-2", TypewriterMode),
        ("shift-r", ReadOnly),
        ("=", ZoomIn),
        ("-", ZoomOut),
        ("0", ActualSize),
        ("1", Heading1),
        ("2", Heading2),
        ("3", Heading3),
        ("4", Heading4),
        ("5", Heading5),
        ("6", Heading6),
        ("alt-0", Paragraph),
        ("shift-k", CodeBlock),
        ("shift-q", Quote),
        ("shift-x", TaskList),
        ("k", Link),
    ] {
        // macOS reserves Cmd-H for Hide. Typora uses Cmd-Option-F for replace.
        if cfg!(target_os = "macos") && suffix == "h" {
            continue;
        }
        bindings.push(KeyBinding::new(
            &format!("{modifier}-{suffix}"),
            action(command),
            None,
        ));
    }
    if cfg!(target_os = "macos") {
        bindings.push(KeyBinding::new("cmd-alt-f", action(Replace), None));
        bindings.push(KeyBinding::new("cmd-alt-c", action(CodeBlock), None));
    }
    bindings.push(KeyBinding::new("f3", action(FindNext), None));
    bindings.push(KeyBinding::new("shift-f3", action(FindPrevious), None));
    bindings.push(KeyBinding::new("f11", action(FullScreen), None));
    if cfg!(target_os = "macos") {
        bindings.push(KeyBinding::new("cmd-m", action(MinimizeWindow), None));
        bindings.push(KeyBinding::new("cmd-ctrl-f", action(FullScreen), None));
    }
    cx.bind_keys(bindings);
}

/// App menus follow the writing workflow: Edit, Paragraph, Format, View.
pub(crate) fn writing_menus(manager: &I18nManager) -> Vec<Menu> {
    use WritingCommand::*;
    let zh = manager.current_language_id().starts_with("zh");
    let label = |en: &str, cn: &str| if zh { cn.to_owned() } else { en.to_owned() };
    let item = |en: &str, cn: &str, command| MenuItem::action(label(en, cn), action(command));
    vec![
        Menu {
            name: label("Edit", "编辑").into(),
            disabled: false,
            items: vec![
                item("Undo", "撤销", Undo),
                item("Redo", "重做", Redo),
                MenuItem::separator(),
                item("Cut", "剪切", Cut),
                item("Copy", "复制", Copy),
                item("Paste", "粘贴", Paste),
                item("Select All", "全选", SelectAll),
                MenuItem::separator(),
                item("Find…", "查找…", Find),
                item("Find Next", "查找下一个", FindNext),
                item("Find Previous", "查找上一个", FindPrevious),
                item("Replace…", "替换…", Replace),
            ],
        },
        Menu {
            name: label("Paragraph", "段落").into(),
            disabled: false,
            items: vec![
                item("Heading 1", "一级标题", Heading1),
                item("Heading 2", "二级标题", Heading2),
                item("Heading 3", "三级标题", Heading3),
                item("Heading 4", "四级标题", Heading4),
                item("Heading 5", "五级标题", Heading5),
                item("Heading 6", "六级标题", Heading6),
                item("Paragraph", "正文", Paragraph),
                MenuItem::separator(),
                item("Blockquote", "引用", Quote),
                item("Unordered List", "无序列表", BulletList),
                item("Ordered List", "有序列表", OrderedList),
                item("Task List", "任务列表", TaskList),
                MenuItem::separator(),
                item("Code Fences", "代码块", CodeBlock),
                item("Table…", "表格…", Table),
                item("Math Block", "公式块", MathBlock),
                item("Mermaid Diagram", "Mermaid 图表", MermaidBlock),
                item("Horizontal Line", "水平分割线", HorizontalRule),
            ],
        },
        Menu {
            name: label("Format", "格式").into(),
            disabled: false,
            items: vec![
                item("Bold", "加粗", Bold),
                item("Italic", "斜体", Italic),
                item("Underline", "下划线", Underline),
                item("Strikethrough", "删除线", Strikethrough),
                item("Code", "行内代码", InlineCode),
                MenuItem::separator(),
                item("Hyperlink…", "超链接…", Link),
            ],
        },
        Menu {
            name: label("View", "显示").into(),
            disabled: false,
            items: vec![
                item("Source Code Mode", "源代码模式", SourceMode),
                item("Read-only Mode", "只读模式", ReadOnly),
                MenuItem::separator(),
                item("Focus Mode", "专注模式", FocusMode),
                item("Typewriter Mode", "打字机模式", TypewriterMode),
                MenuItem::separator(),
                item("Show / Hide Sidebar", "显示／隐藏侧边栏", Sidebar),
                item("Outline", "大纲", Outline),
                item("Document List", "文档列表", Documents),
                item("File Tree", "文件树", Files),
                item("Word Count", "字数统计", Statistics),
                MenuItem::separator(),
                item("Actual Size", "实际大小", ActualSize),
                item("Zoom In", "放大", ZoomIn),
                item("Zoom Out", "缩小", ZoomOut),
            ],
        },
    ]
}

impl Editor {
    pub(super) fn sync_writing_menus(&self, window: &Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        let state = WritingMenuState {
            focus: self.writing.focus_mode,
            typewriter: self.writing.typewriter_mode,
            read_only: self.writing.read_only,
            source: self.view_mode == crate::editor::ViewMode::Source,
            sidebar: self.workspace.is_open,
            outline: self.workspace.is_open && self.workspace.active_tab == WorkspaceTab::Outline,
            documents: self.workspace.is_open
                && self.workspace.active_tab == WorkspaceTab::Documents,
            files: self.workspace.is_open && self.workspace.active_tab == WorkspaceTab::Files,
            fullscreen: window.is_fullscreen(),
        };
        if cx.try_global::<WritingMenuState>() != Some(&state) {
            cx.set_global(state);
            crate::app_menu::install_menus(cx);
        }
    }

    pub(crate) fn on_writing_action(
        &mut self,
        event: &WritingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use WritingCommand::*;
        if self.handle_search_edit_command(event.command, window, cx) {
            return;
        }
        if self.writing.read_only
            && !matches!(
                event.command,
                Find | Replace
                    | FindNext
                    | FindPrevious
                    | ReadOnly
                    | FocusMode
                    | TypewriterMode
                    | Outline
                    | Files
                    | Documents
                    | Sidebar
                    | OpenFolder
                    | SourceMode
                    | ZoomIn
                    | ZoomOut
                    | ActualSize
                    | Statistics
                    | Copy
                    | SelectAll
                    | MinimizeWindow
                    | ZoomWindow
                    | FullScreen
            )
        {
            return;
        }
        let old_zoom = self.writing.zoom_steps;
        match event.command {
            Find | Replace => self.open_search(event.command == Replace, window, cx),
            FindNext => self.navigate_search(false, cx),
            FindPrevious => self.navigate_search(true, cx),
            FocusMode => self.writing.focus_mode = !self.writing.focus_mode,
            ReadOnly => {
                self.writing.read_only = !self.writing.read_only;
                self.sync_block_read_only(cx);
                self.dismiss_contextual_overlays(cx);
            }
            TypewriterMode => {
                self.writing.typewriter_mode = !self.writing.typewriter_mode;
                self.request_active_block_scroll_into_view(cx);
            }
            Outline | Files | Documents => self.show_workspace_tab(
                if event.command == Outline {
                    WorkspaceTab::Outline
                } else if event.command == Documents {
                    WorkspaceTab::Documents
                } else {
                    WorkspaceTab::Files
                },
                window,
                cx,
            ),
            Sidebar => self.toggle_workspace_drawer(window, cx),
            OpenFolder => self.prompt_workspace_folder(cx),
            SourceMode => self.toggle_view_mode_from_ui(cx),
            Statistics => self.writing.statistics_open = !self.writing.statistics_open,
            MinimizeWindow => window.minimize_window(),
            ZoomWindow => window.zoom_window(),
            FullScreen => window.toggle_fullscreen(),
            Table if self.view_mode == crate::editor::ViewMode::Rendered => {
                use super::context_menu::{TableInsertDialogState, TableInsertTarget};
                let target = self
                    .active_entity_id
                    .map(|id| {
                        self.table_cell_binding(id)
                            .map_or(id, |binding| binding.table_block.entity_id())
                    })
                    .map(|id| TableInsertTarget::After(self.root_ancestor_entity_id(id)))
                    .unwrap_or(TableInsertTarget::Append);
                self.table_insert_dialog = Some(TableInsertDialogState {
                    target,
                    body_rows: 2,
                    columns: 2,
                });
            }
            ZoomIn => self.writing.zoom_steps = (self.writing.zoom_steps + 1).min(10),
            ZoomOut => self.writing.zoom_steps = (self.writing.zoom_steps - 1).max(-5),
            ActualSize => self.writing.zoom_steps = 0,
            Undo => self.undo_document(cx),
            Redo => self.redo_document(cx),
            Copy => {
                if let Some(text) = self.selected_markdown_text(cx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                } else if let Some(target) = self.current_edit_target_from_state(cx) {
                    target.update(cx, |block, cx| {
                        block.on_copy(&crate::components::Copy, window, cx)
                    });
                }
            }
            Cut => {
                if self.cross_block_selection.is_some() {
                    self.on_cut_capture(&crate::components::Cut, window, cx);
                } else if let Some(target) = self.current_edit_target_from_state(cx) {
                    target.update(cx, |block, cx| {
                        block.on_cut(&crate::components::Cut, window, cx)
                    });
                }
            }
            Paste => {
                if let Some(target) = self.current_edit_target_from_state(cx) {
                    target.update(cx, |block, cx| {
                        block.on_paste(&crate::components::Paste, window, cx)
                    });
                }
            }
            SelectAll => {
                let len = self.serialized_document_text(cx).len();
                self.apply_selection_snapshot_in_current_mode(
                    &UndoSelectionSnapshot {
                        range: 0..len,
                        reversed: false,
                    },
                    cx,
                );
            }
            Bold | Italic | Underline | InlineCode if self.cross_block_selection.is_none() => {
                if let Some(target) = self.current_edit_target_from_state(cx) {
                    target.update(cx, |block, cx| match event.command {
                        Bold => {
                            block.on_bold_selection(&crate::components::BoldSelection, window, cx)
                        }
                        Italic => block.on_italic_selection(
                            &crate::components::ItalicSelection,
                            window,
                            cx,
                        ),
                        Underline => block.on_underline_selection(
                            &crate::components::UnderlineSelection,
                            window,
                            cx,
                        ),
                        _ => block.on_code_selection(&crate::components::CodeSelection, window, cx),
                    });
                }
            }
            command => self.apply_writing_format(command, cx),
        }
        if old_zoom != self.writing.zoom_steps {
            self.row_stride_cache.clear();
            self.row_stride_width = None;
            self.last_scroll_viewport_size = None;
            self.request_active_block_scroll_into_view(cx);
        }
        cx.notify();
    }

    fn ensure_search_fields(&mut self, cx: &mut Context<Self>) {
        if self.writing.query.is_some() {
            return;
        }
        let find_placeholder = localized(cx, "Find", "查找");
        let replace_placeholder = localized(cx, "Replace with", "替换为");
        let query = cx.new(|cx| SingleLineInput::new("", find_placeholder, cx).compact());
        let replacement = cx.new(|cx| SingleLineInput::new("", replace_placeholder, cx).compact());
        cx.subscribe(&query, |editor, _, _: &InputChanged, cx| {
            editor.refresh_search(true, cx);
        })
        .detach();
        self.writing.query = Some(query);
        self.writing.replacement = Some(replacement);
    }

    fn open_search(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .selected_markdown_text(cx)
            .filter(|text| !text.is_empty() && !text.contains('\n'));
        self.ensure_search_fields(cx);
        self.close_menu_bar(cx);
        self.writing.search_open = true;
        self.writing.replace_open = replace;
        self.writing.statistics_open = false;
        if let Some(selected) = selected {
            self.writing
                .query
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value(selected, cx));
        }
        self.refresh_search(true, cx);
        self.pending_focus = None;
        self.writing
            .query
            .as_ref()
            .unwrap()
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    pub(super) fn close_search(&mut self, cx: &mut Context<Self>) {
        self.writing.search_open = false;
        for entity in self
            .document
            .visible_blocks()
            .iter()
            .map(|block| &block.entity)
            .chain(self.table_cells.values().map(|binding| &binding.cell))
        {
            if !entity.read(cx).search_highlights.is_empty() {
                entity.update(cx, |block, cx| {
                    block.search_highlights.clear();
                    cx.notify();
                });
            }
        }
        self.pending_focus = self.active_entity_id;
        cx.notify();
    }

    /// Invalidates only when text or query changes. Large documents do not get
    /// lowercased again for caret blink frames.
    pub(super) fn refresh_search(&mut self, reset: bool, cx: &mut Context<Self>) {
        let Some(query) = self
            .writing
            .query
            .as_ref()
            .map(|q| q.read(cx).value().to_owned())
        else {
            return;
        };
        let source = self.serialized_document_text(cx);
        let entities = self
            .document
            .visible_blocks()
            .iter()
            .map(|block| block.entity.entity_id())
            .collect::<Vec<_>>();
        if !reset
            && entities == self.writing.searched_entities
            && source == self.writing.searched_source
            && query == self.writing.searched_query
        {
            return;
        }
        let caret = self.capture_source_selection_snapshot(cx).range.start;
        self.writing.search_error = false;
        self.writing.matches = if self.writing.regex {
            match search_regex(&query, self.writing.case_sensitive, self.writing.whole_word) {
                Ok(regex) if !query.is_empty() => {
                    regex.find_iter(&source).map(|m| m.range()).collect()
                }
                Ok(_) => Vec::new(),
                Err(_) => {
                    self.writing.search_error = true;
                    Vec::new()
                }
            }
        } else {
            find_matches(
                &source,
                &query,
                self.writing.case_sensitive,
                self.writing.whole_word,
            )
        };
        self.writing.current = self
            .writing
            .matches
            .iter()
            .position(|range| range.start >= caret)
            .or_else(|| (!self.writing.matches.is_empty()).then_some(0));
        self.writing.searched_entities = entities;
        self.writing.searched_source = source;
        self.writing.searched_query = query;
        if reset {
            self.reveal_search_match(cx);
        }
        self.sync_search_highlights(cx);
        cx.notify();
    }

    fn sync_search_highlights(&self, cx: &mut Context<Self>) {
        if !self.writing.search_open {
            return;
        }
        for mapping in self.build_source_target_mappings(cx) {
            let mut local = Vec::new();
            let first = self
                .writing
                .matches
                .partition_point(|range| range.end <= mapping.full_source_range.start);
            for (index, range) in self.writing.matches.iter().enumerate().skip(first) {
                if range.start >= mapping.full_source_range.end {
                    break;
                }
                let max = mapping.source_to_content.len().saturating_sub(1);
                let start = range
                    .start
                    .saturating_sub(mapping.full_source_range.start)
                    .min(max);
                let end = range
                    .end
                    .saturating_sub(mapping.full_source_range.start)
                    .min(max);
                let range = mapping.source_to_content[start]..mapping.source_to_content[end];
                if !range.is_empty() {
                    local.push((range, self.writing.current == Some(index)));
                }
            }
            if mapping.entity.read(cx).search_highlights != local {
                mapping.entity.update(cx, |block, cx| {
                    block.search_highlights = local;
                    cx.notify();
                });
            }
        }
    }

    fn reveal_search_match(&mut self, cx: &mut Context<Self>) {
        self.sync_search_highlights(cx);
        let Some(range) = self
            .writing
            .current
            .and_then(|i| self.writing.matches.get(i))
            .cloned()
        else {
            return;
        };
        self.clear_cross_block_selection(cx);
        self.apply_selection_snapshot_in_current_mode(
            &UndoSelectionSnapshot {
                range,
                reversed: false,
            },
            cx,
        );
        // Keep keyboard input in the find field while showing the result in the document.
        if self.writing.search_open {
            self.pending_focus = None;
        }
        self.request_active_block_scroll_into_view(cx);
    }

    fn navigate_search(&mut self, previous: bool, cx: &mut Context<Self>) {
        self.refresh_search(false, cx);
        let len = self.writing.matches.len();
        if len == 0 {
            return;
        }
        self.writing.current = Some(match self.writing.current {
            Some(i) if previous => (i + len - 1) % len,
            Some(i) => (i + 1) % len,
            None if previous => len - 1,
            None => 0,
        });
        self.reveal_search_match(cx);
    }

    fn replace_search_match(&mut self, all: bool, cx: &mut Context<Self>) {
        self.refresh_search(false, cx);
        let Some(replacement) = self
            .writing
            .replacement
            .as_ref()
            .map(|r| r.read(cx).value().to_owned())
        else {
            return;
        };
        let ranges: Vec<_> = if all {
            self.writing.matches.clone()
        } else {
            self.writing
                .current
                .and_then(|i| self.writing.matches.get(i))
                .cloned()
                .into_iter()
                .collect()
        };
        let Some(first) = ranges.first() else {
            return;
        };
        let mut source = self.serialized_document_text(cx);
        let regex = self
            .writing
            .regex
            .then(|| {
                search_regex(
                    &self.writing.searched_query,
                    self.writing.case_sensitive,
                    self.writing.whole_word,
                )
                .ok()
            })
            .flatten();
        let edits = ranges
            .iter()
            .map(|range| {
                let mut expanded = replacement.clone();
                if let Some(regex) = &regex
                    && let Some(captures) = regex.captures_at(&source, range.start)
                    && captures.get(0).is_some_and(|m| m.range() == *range)
                {
                    expanded.clear();
                    captures.expand(&replacement, &mut expanded);
                }
                (range.clone(), expanded)
            })
            .collect::<Vec<_>>();
        let caret = first.start + edits[0].1.len();
        for (range, replacement) in edits.into_iter().rev() {
            source.replace_range(range, &replacement);
        }
        self.commit_source_edit(source, caret..caret, cx);
        self.pending_focus = None;
        self.refresh_search(true, cx);
    }

    /// A replacement/formatting command is one undo step, including Replace All.
    fn commit_source_edit(&mut self, source: String, range: Range<usize>, cx: &mut Context<Self>) {
        if self.writing.read_only {
            return;
        }
        if source == self.serialized_document_text(cx) {
            return;
        }
        self.finalize_pending_undo_capture(cx);
        self.prepare_undo_capture(UndoCaptureKind::NonCoalescible, cx);
        self.clear_cross_block_selection(cx);
        self.restore_history_entry(
            &HistoryEntry {
                source_text: source,
                selection: UndoSelectionSnapshot {
                    range,
                    reversed: false,
                },
                timestamp: std::time::Instant::now(),
                kind: UndoCaptureKind::NonCoalescible,
            },
            cx,
        );
        self.mark_dirty(cx);
        self.finalize_pending_undo_capture(cx);
    }

    pub(super) fn sync_block_read_only(&self, cx: &mut Context<Self>) {
        for entity in self
            .document
            .visible_blocks()
            .iter()
            .map(|block| &block.entity)
            .chain(self.table_cells.values().map(|binding| &binding.cell))
        {
            if entity.read(cx).read_only != self.writing.read_only {
                entity.update(cx, |block, cx| {
                    block.read_only = self.writing.read_only;
                    block.marked_range = None;
                    block.code_language_marked_range = None;
                    cx.notify();
                });
            }
        }
    }

    fn apply_writing_format(&mut self, command: WritingCommand, cx: &mut Context<Self>) {
        // Empty rendered paragraphs are source separators, with no text
        // selection mapping. Convert the live block instead of formatting the
        // previous source selection returned as a fallback for those gaps.
        if command == WritingCommand::CodeBlock
            && self.view_mode == crate::editor::ViewMode::Rendered
            && self.cross_block_selection.is_none()
            && let Some(target) = self.current_edit_target_from_state(cx)
            && !target.read(cx).is_table_cell()
            && Self::is_empty_paragraph_separator(target.read(cx))
        {
            self.finalize_pending_undo_capture(cx);
            self.prepare_undo_capture(UndoCaptureKind::NonCoalescible, cx);
            target.update(cx, |block, cx| block.enter_code_block(None, cx));
            self.focus_block(target.entity_id());
            return;
        }
        let source = self.serialized_document_text(cx);
        let selection = self.capture_source_selection_snapshot(cx).range;
        if let Some((source, selection)) = format_source(&source, selection, command) {
            self.commit_source_edit(source, selection, cx);
        }
    }

    pub(super) fn search_owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.writing.search_open
            && [
                self.writing.query.as_ref(),
                self.writing.replacement.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    pub(super) fn handle_search_edit_command(
        &mut self,
        command: WritingCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use WritingCommand::*;
        if !matches!(command, Copy | Cut | Paste | SelectAll | Undo | Redo)
            || !self.writing.search_open
        {
            return false;
        }
        let input = [
            self.writing.query.as_ref(),
            self.writing.replacement.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find(|input| input.read(cx).focus_handle(cx).is_focused(window))
        .cloned();
        let Some(input) = input else {
            return false;
        };
        input.update(cx, |input, cx| {
            input.dispatch_edit_command(command, window, cx)
        });
        true
    }

    pub(super) fn render_search_bar(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.writing.search_open {
            return None;
        }
        self.refresh_search(false, cx);
        let c = &theme.colors;
        let count = if self.writing.search_error {
            localized(cx, "Invalid regex", "正则错误").to_string()
        } else {
            self.writing
                .current
                .map(|i| format!("{} / {}", i + 1, self.writing.matches.len()))
                .unwrap_or_else(|| {
                    if self.writing.searched_query.is_empty() {
                        String::new()
                    } else {
                        localized(cx, "No results", "无结果").to_string()
                    }
                })
        };
        let query = self.writing.query.clone()?;
        let replacement = self.writing.replacement.clone()?;
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .h(px(28.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .text_size(px(12.0))
                .text_color(c.dialog_body)
                .cursor_pointer()
                .hover(|style| style.bg(c.dialog_secondary_button_hover))
                .child(label)
        };
        let bar = div()
            .id("find-replace-bar")
            .w_full()
            .flex_shrink_0()
            .px(px(16.0))
            .py(px(3.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(c.status_bar_background)
            .border_b(px(1.0))
            .border_color(c.dialog_border)
            .shadow_sm()
            .occlude()
            .capture_key_down(cx.listener(|editor, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "enter" {
                    editor.navigate_search(event.keystroke.modifiers.shift, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" {
                    editor.close_search(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        button(
                            "find-toggle-replace",
                            localized(cx, "Find ▾", "查找 ▾").into(),
                        )
                        .on_click(cx.listener(|editor, _, _, cx| {
                            editor.writing.replace_open = !editor.writing.replace_open;
                            cx.notify();
                        })),
                    )
                    .child(div().flex_1().min_w(px(80.0)).child(query))
                    .child(
                        div()
                            .w(px(78.0))
                            .text_size(px(11.0))
                            .text_color(c.dialog_muted)
                            .text_align(TextAlign::Center)
                            .child(count),
                    )
                    .child(
                        button(
                            "find-case",
                            if self.writing.case_sensitive {
                                "[Aa]"
                            } else {
                                "Aa"
                            }
                            .into(),
                        )
                        .on_click(cx.listener(|editor, _, _, cx| {
                            editor.writing.case_sensitive = !editor.writing.case_sensitive;
                            editor.refresh_search(true, cx);
                        })),
                    )
                    .child(
                        button(
                            "find-whole-word",
                            if self.writing.whole_word { "[W]" } else { "W" }.into(),
                        )
                        .on_click(cx.listener(|editor, _, _, cx| {
                            editor.writing.whole_word = !editor.writing.whole_word;
                            editor.refresh_search(true, cx);
                        })),
                    )
                    .child(
                        button(
                            "find-regex",
                            if self.writing.regex { "[(.*)]" } else { "(.*)" }.into(),
                        )
                        .on_click(cx.listener(|editor, _, _, cx| {
                            editor.writing.regex = !editor.writing.regex;
                            editor.refresh_search(true, cx);
                        })),
                    )
                    .child(
                        button("find-previous", "↑".into()).on_click(
                            cx.listener(|editor, _, _, cx| editor.navigate_search(true, cx)),
                        ),
                    )
                    .child(button("find-next", "↓".into()).on_click(
                        cx.listener(|editor, _, _, cx| editor.navigate_search(false, cx)),
                    ))
                    .child(
                        button("find-close", "×".into())
                            .on_click(cx.listener(|editor, _, _, cx| editor.close_search(cx))),
                    ),
            );
        let bar = if self.writing.replace_open {
            bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(30.0))
                    .child(div().flex_1().min_w(px(80.0)).child(replacement))
                    .child(
                        button("replace-current", localized(cx, "Replace", "替换").into())
                            .on_click(cx.listener(|editor, _, _, cx| {
                                editor.replace_search_match(false, cx)
                            })),
                    )
                    .child(
                        button(
                            "replace-all",
                            localized(cx, "Replace All", "全部替换").into(),
                        )
                        .on_click(
                            cx.listener(|editor, _, _, cx| editor.replace_search_match(true, cx)),
                        ),
                    ),
            )
        } else {
            bar
        };
        Some(bar.into_any_element())
    }

    pub(super) fn writing_statistics(&mut self, cx: &App) -> DocumentStatistics {
        let source = self.serialized_document_text(cx);
        if let Some((cached_source, stats)) = &self.writing.cached_statistics
            && cached_source == &source
        {
            return stats.clone();
        }
        let stats = document_statistics(&source);
        self.writing.cached_statistics = Some((source, stats.clone()));
        stats
    }

    pub(super) fn render_statistics(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.writing.statistics_open {
            return None;
        }
        let stats = self.writing_statistics(cx);
        let c = &theme.colors;
        let labels = [
            (localized(cx, "Words", "字数"), stats.words.to_string()),
            (
                localized(cx, "Characters", "字符数"),
                stats.characters.to_string(),
            ),
            (
                localized(cx, "Characters (no spaces)", "字符数（不含空格）"),
                stats.non_whitespace.to_string(),
            ),
            (
                localized(cx, "Paragraphs", "段落"),
                stats.paragraphs.to_string(),
            ),
            (localized(cx, "Lines", "行数"), stats.lines.to_string()),
            (
                localized(cx, "Reading time", "阅读时长"),
                format!("{} min", stats.words.div_ceil(240).max(1)),
            ),
        ];
        Some(
            div()
                .id("document-statistics")
                .absolute()
                .bottom(px(theme.dimensions.status_bar_height + 8.0))
                .right(px(16.0))
                .w(px(260.0))
                .p(px(18.0))
                .rounded(px(8.0))
                .bg(c.dialog_surface)
                .border(px(1.0))
                .border_color(c.dialog_border)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(localized(cx, "Word Count", "字数统计")),
                        )
                        .child(
                            div()
                                .id("statistics-close")
                                .cursor_pointer()
                                .child("×")
                                .on_click(cx.listener(|editor, _, _, cx| {
                                    editor.writing.statistics_open = false;
                                    cx.notify();
                                })),
                        ),
                )
                .children(labels.into_iter().map(|(label, value)| {
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(12.0))
                        .text_color(c.dialog_muted)
                        .child(label)
                        .child(div().text_color(c.dialog_body).child(value))
                }))
                .into_any_element(),
        )
    }
}

fn search_regex(
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Result<regex::Regex, regex::Error> {
    let pattern = if whole_word {
        format!(r"\b(?:{query})\b")
    } else {
        query.to_owned()
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .build()
}

/// Match ranges always refer to original UTF-8 bytes, even when lowercasing
/// changes the length (İ → i + combining dot). No range splits a source scalar.
fn find_matches(
    source: &str,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let (haystack, boundaries) = if case_sensitive {
        (
            source.to_owned(),
            (0..=source.len())
                .map(|i| source.is_char_boundary(i).then_some(i))
                .collect::<Vec<_>>(),
        )
    } else {
        let mut folded = String::new();
        let mut boundaries = vec![Some(0)];
        for (start, ch) in source.char_indices() {
            let before = folded.len();
            folded.extend(ch.to_lowercase());
            boundaries.resize(folded.len() + 1, None);
            boundaries[before] = Some(start);
            boundaries[folded.len()] = Some(start + ch.len_utf8());
        }
        (folded, boundaries)
    };
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    haystack
        .match_indices(&needle)
        .filter_map(|(start, text)| {
            let start = boundaries[start]?;
            let end = boundaries[start + text.len()]?;
            let word = |ch: char| ch.is_alphanumeric() || ch == '_';
            if whole_word
                && (source[..start].chars().next_back().is_some_and(word)
                    || source[end..].chars().next().is_some_and(word))
            {
                return None;
            }
            Some(start..end)
        })
        .collect()
}

fn format_source(
    source: &str,
    selection: Range<usize>,
    command: WritingCommand,
) -> Option<(String, Range<usize>)> {
    use WritingCommand::*;
    let selection = crate::text_offsets::normalize_utf8_range(source, selection);
    let inline = match command {
        Bold => Some(("**", "**")),
        Italic => Some(("*", "*")),
        Underline => Some(("<u>", "</u>")),
        InlineCode => Some(("`", "`")),
        Strikethrough => Some(("~~", "~~")),
        Link => Some(("[", "](https://)")),
        _ => None,
    };
    if let Some((before, after)) = inline {
        let mut output = source.to_owned();
        let text = &source[selection.clone()];
        output.replace_range(selection.clone(), &format!("{before}{text}{after}"));
        let start = selection.start + before.len();
        return Some((output, start..start + text.len()));
    }
    let start = source[..selection.start].rfind('\n').map_or(0, |i| i + 1);
    let last = if selection.end > selection.start && source[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = source[last..].find('\n').map_or(source.len(), |i| last + i);
    let text = &source[start..end];
    let replacement = match command {
        CodeBlock => {
            let fence = super::persistence::safe_code_fence(text);
            format!("{fence}\n{text}\n{fence}")
        }
        MermaidBlock => {
            let fence = super::persistence::safe_code_fence(text);
            format!("{fence}mermaid\n{text}\n{fence}")
        }
        MathBlock => format!("$$\n{text}\n$$"),
        Table => format!("{text}\n\n|  |  |\n| --- | --- |\n|  |  |\n|  |  |\n"),
        HorizontalRule => format!("{text}\n\n---\n\n"),
        Paragraph | Heading1 | Heading2 | Heading3 | Heading4 | Heading5 | Heading6 | Quote
        | BulletList | OrderedList | TaskList => {
            let prefix = match command {
                Heading1 => "# ",
                Heading2 => "## ",
                Heading3 => "### ",
                Heading4 => "#### ",
                Heading5 => "##### ",
                Heading6 => "###### ",
                Quote => "> ",
                BulletList => "- ",
                OrderedList => "1. ",
                TaskList => "- [ ] ",
                _ => "",
            };
            text.split('\n')
                .map(|line| {
                    let content = strip_block_prefix(line);
                    format!("{prefix}{content}")
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    let mut output = source.to_owned();
    let new_end = start + replacement.len();
    output.replace_range(start..end, &replacement);
    Some((output, start..new_end))
}

fn strip_block_prefix(line: &str) -> &str {
    let trimmed = line.trim_start_matches('#');
    if line.len() - trimmed.len() <= 6 && line.starts_with('#') && trimmed.starts_with(' ') {
        return trimmed.trim_start_matches(' ');
    }
    for prefix in ["- [ ] ", "- [x] ", "- [X] ", "> ", "- ", "* ", "+ "] {
        if let Some(content) = line.strip_prefix(prefix) {
            return content;
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(content) = line[digits..].strip_prefix(". ")
    {
        return content;
    }
    line
}

#[derive(Clone, Debug, Default)]
pub(super) struct DocumentStatistics {
    pub words: usize,
    pub characters: usize,
    pub non_whitespace: usize,
    pub paragraphs: usize,
    pub lines: usize,
}

pub(super) fn document_statistics(source: &str) -> DocumentStatistics {
    let mut plain = String::new();
    let mut paragraphs = 0;
    for event in Parser::new_ext(source, Options::all()) {
        match event {
            Event::Text(text)
            | Event::Code(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => plain.push_str(&text),
            Event::SoftBreak | Event::HardBreak => plain.push('\n'),
            Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock,
            ) => {
                plain.push('\n');
                paragraphs += 1;
            }
            _ => {}
        }
    }
    DocumentStatistics {
        words: super::status_bar::count_words(&plain),
        characters: plain.trim_end().graphemes(true).count(),
        non_whitespace: plain.chars().filter(|c| !c.is_whitespace()).count(),
        paragraphs,
        lines: source.lines().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Editor, action, document_statistics, find_matches, format_source, search_regex};
    use crate::components::WritingCommand;
    use crate::i18n::I18nManager;
    use crate::theme::ThemeManager;
    use gpui::{AppContext, Focusable, TestAppContext, VisualTestContext};

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            I18nManager::init_with_language_id(cx, "en-US");
            ThemeManager::init_with_theme_id(cx, "velotype-light");
            crate::components::init(cx);
        });
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn option_command_c_on_new_empty_line_keeps_previous_paragraph(cx: &mut TestAppContext) {
        init(cx);
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, "previous".into(), None));
        editor.update(visual, |editor, cx| {
            let paragraph = editor.document.first_root().unwrap().clone();
            editor.focus_block_range(&paragraph, 8..8, cx);
        });
        draw(visual);
        visual.simulate_keystrokes("enter");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            let target = editor.current_edit_target_from_state(cx).unwrap();
            assert_eq!(
                target.read(cx).kind(),
                crate::components::BlockKind::Paragraph
            );
            assert_eq!(target.read(cx).record.title.visible_text(), "");
        });
        let before = editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx));
        visual.simulate_keystrokes("cmd-alt-c");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            let first = editor.document.first_root().unwrap().read(cx);
            assert_eq!(first.kind(), crate::components::BlockKind::Paragraph);
            assert_eq!(first.record.title.visible_text(), "previous");
            let target = editor.current_edit_target_from_state(cx).unwrap();
            assert!(target.read(cx).kind().is_code_block());
            assert_eq!(target.read(cx).record.title.visible_text(), "");
            assert_eq!(target.read(cx).selected_range, 0..0);
        });
        let created = editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx));
        visual.simulate_keystrokes("cmd-z");
        draw(visual);
        assert_eq!(
            editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
            before
        );
        visual.simulate_keystrokes("cmd-shift-z");
        draw(visual);
        assert_eq!(
            editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
            created
        );
        visual.simulate_input("df_stun_r");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            let target = editor.current_edit_target_from_state(cx).unwrap();
            assert!(target.read(cx).kind().is_code_block());
            assert_eq!(target.read(cx).record.title.visible_text(), "df_stun_r");
            assert_eq!(
                editor
                    .document
                    .first_root()
                    .unwrap()
                    .read(cx)
                    .record
                    .title
                    .visible_text(),
                "previous"
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn option_command_c_wraps_selection_in_code_block_in_both_views(cx: &mut TestAppContext) {
        init(cx);
        for source_mode in [false, true] {
            for source in ["", "alpha", "alpha\n\nbeta"] {
                let (editor, visual) =
                    cx.add_window_view(|_, cx| Editor::from_markdown(cx, source.into(), None));
                draw(visual);
                editor.update(visual, |editor, cx| {
                    if source_mode {
                        editor.toggle_view_mode_from_ui(cx);
                    }
                    let len = editor.serialized_document_text(cx).len();
                    editor.apply_selection_snapshot_in_current_mode(
                        &crate::editor::UndoSelectionSnapshot {
                            range: if source == "alpha" { 2..2 } else { 0..len },
                            reversed: false,
                        },
                        cx,
                    );
                });
                draw(visual);
                visual.simulate_keystrokes("cmd-alt-c");
                draw(visual);
                editor.read_with(visual, |editor, cx| {
                    let markdown = editor.serialized_document_text(cx);
                    assert_eq!(markdown.trim_end(), format!("```\n{source}\n```"));
                    if !source_mode {
                        assert!(
                            editor
                                .document
                                .first_root()
                                .unwrap()
                                .read(cx)
                                .kind()
                                .is_code_block()
                        );
                    }
                });
                visual.simulate_keystrokes("cmd-z");
                draw(visual);
                assert_eq!(
                    editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
                    source
                );
            }
        }
    }

    #[gpui::test]
    async fn code_command_converts_only_the_focused_empty_paragraph(cx: &mut TestAppContext) {
        init(cx);
        for (source, empty_index) in [
            ("\n\nbefore", 0),
            ("before\n\n\nafter", 1),
            ("before\n\n", 1),
            ("before\n\n\n\n", 2),
        ] {
            let (editor, visual) =
                cx.add_window_view(|_, cx| Editor::from_markdown(cx, source.into(), None));
            let original_blocks = editor.read_with(visual, |editor, cx| {
                editor
                    .document
                    .visible_blocks()
                    .into_iter()
                    .map(|visible| {
                        let block = visible.entity.read(cx);
                        (
                            visible.entity.clone(),
                            block.kind(),
                            block.record.title.visible_text(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            let empty = original_blocks[empty_index].0.clone();
            assert_eq!(original_blocks[empty_index].2, "");
            editor.update(visual, |editor, cx| {
                editor.focus_block_range(&empty, 0..0, cx)
            });
            draw(visual);
            visual.dispatch_action(action(WritingCommand::CodeBlock));
            draw(visual);
            editor.read_with(visual, |editor, cx| {
                assert_eq!(
                    editor.document.visible_blocks().len(),
                    original_blocks.len()
                );
                for (index, (entity, original_kind, original_text)) in
                    original_blocks.iter().enumerate()
                {
                    let block = entity.read(cx);
                    assert_eq!(block.record.title.visible_text(), *original_text);
                    if index == empty_index {
                        assert!(block.kind().is_code_block());
                        assert_eq!(editor.current_edit_target_from_state(cx).unwrap(), *entity);
                        assert_eq!(block.selected_range, 0..0);
                    } else {
                        assert_eq!(block.kind(), *original_kind);
                    }
                }
            });
            visual.dispatch_action(action(WritingCommand::Undo));
            draw(visual);
            assert_eq!(
                editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
                source
            );
        }
    }

    #[gpui::test]
    async fn heading_hints_follow_focus_without_moving_or_changing_text(cx: &mut TestAppContext) {
        init(cx);
        let source =
            "# One\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six\n\nBody";
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, source.into(), None));
        draw(visual);
        let blocks = editor.read_with(visual, |editor, _cx| {
            editor
                .document
                .visible_blocks()
                .into_iter()
                .map(|block| block.entity.clone())
                .collect::<Vec<_>>()
        });
        for (index, block) in blocks.iter().enumerate() {
            let before = block.read_with(visual, |block, _| block.last_bounds.unwrap());
            visual.update(|window, cx| block.read(cx).focus_handle(cx).focus(window, cx));
            draw(visual);
            let hint = visual.debug_bounds("heading-level-hint");
            if (2..=4).contains(&index) {
                let hint = hint.expect("focused H3/H4/H5 should show its level hint");
                let after = block.read_with(visual, |block, _| block.last_bounds.unwrap());
                assert_eq!(before, after, "focusing a heading must not shift its text");
                assert!(hint.right() < after.left());
                assert!(hint.top() >= after.top() && hint.bottom() <= after.bottom());
            } else {
                assert!(hint.is_none(), "only H3/H4/H5 should show a hint");
            }
        }
        visual.update(|window, cx| blocks[2].read(cx).focus_handle(cx).focus(window, cx));
        draw(visual);
        assert!(visual.debug_bounds("heading-level-hint").is_some());
        editor.update(visual, |editor, cx| editor.toggle_view_mode_from_ui(cx));
        draw(visual);
        assert!(visual.debug_bounds("heading-level-hint").is_none());
        assert_eq!(
            editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
            source
        );
    }

    #[gpui::test]
    async fn native_find_unicode_link_fast_navigation_and_input(cx: &mut TestAppContext) {
        init(cx);
        let source = concat!(
            "# 链接编辑\n\n",
            "[带 title 的链接](https://example.com \"悬停提示文字\")\n\n",
            "后续中文😀段落",
        );
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, source.into(), None));
        draw(visual);
        visual.simulate_keystrokes("cmd-f");
        draw(visual);
        visual.simulate_input("带 title 的链接");
        draw(visual);
        visual.simulate_keystrokes("escape home right");
        visual.simulate_input("QA");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(
                editor.serialized_document_text(cx),
                source.replace("[带 title", "[QA带 title")
            );
        });
    }

    #[gpui::test]
    async fn selecting_unicode_link_delimiters_keeps_source_selection_valid(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let (editor, visual) = cx.add_window_view(|_, cx| {
            Editor::from_markdown(
                cx,
                "[**粗体** 与 *斜体* 的链接](https://example.com/formatted)".into(),
                None,
            )
        });
        draw(visual);
        visual.simulate_keystrokes("home shift-right shift-right");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(editor.selected_markdown_text(cx).as_deref(), Some("[**"));
        });
    }

    #[gpui::test]
    async fn editing_partially_expanded_unicode_link_preserves_styles_and_undo(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let source = "[**粗体** 与 *斜体* 的链接](https://example.com/formatted)";
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, source.into(), None));
        draw(visual);
        visual.simulate_keystrokes("home right right");
        visual.simulate_input("QA中文😀");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(
                editor.serialized_document_text(cx),
                source.replace("**粗体", "**QA中文😀粗体")
            );
        });
        visual.simulate_keystrokes("cmd-z");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(editor.serialized_document_text(cx), source);
        });
    }

    #[gpui::test]
    async fn native_find_preserves_input_focus_highlights_and_document(cx: &mut TestAppContext) {
        init(cx);
        let (editor, visual) = cx.add_window_view(|_, cx| {
            Editor::from_markdown(cx, "# 中文 Rust\n\n中文 rust".into(), None)
        });
        draw(visual);
        let original = editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx));
        visual.simulate_keystrokes("cmd-f");
        draw(visual);
        visual.simulate_input("中文");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert!(editor.writing.search_open);
            assert_eq!(editor.writing.matches.len(), 2);
            assert_eq!(editor.serialized_document_text(cx), original);
            let highlights = editor
                .document
                .visible_blocks()
                .iter()
                .flat_map(|block| block.entity.read(cx).search_highlights.clone())
                .collect::<Vec<_>>();
            assert_eq!(highlights.len(), 2);
            assert_eq!(highlights.iter().filter(|(_, current)| *current).count(), 1);
        });
        visual.update(|window, cx| {
            let query = editor.read(cx).writing.query.as_ref().unwrap().clone();
            assert!(query.read(cx).focus_handle(cx).is_focused(window));
        });
        // The Edit menu and global undo shortcuts must edit the field rather
        // than the document while the search input owns focus.
        visual.dispatch_action(action(WritingCommand::SelectAll));
        visual.simulate_input("Rust");
        draw(visual);
        visual.simulate_keystrokes("cmd-z");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(
                editor.writing.query.as_ref().unwrap().read(cx).value(),
                "中文"
            );
            assert_eq!(editor.serialized_document_text(cx), original);
            assert!(editor.undo_history.is_empty());
        });
        let before_next = editor.read_with(visual, |editor, _| editor.writing.current.unwrap());
        visual.simulate_keystrokes("enter");
        draw(visual);
        assert_eq!(
            editor.read_with(visual, |editor, _| editor.writing.current),
            Some((before_next + 1) % 2)
        );
        visual.simulate_keystrokes("escape");
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert!(!editor.writing.search_open);
            assert!(
                editor.document.visible_blocks().iter().all(|block| block
                    .entity
                    .read(cx)
                    .search_highlights
                    .is_empty())
            );
        });
    }

    #[gpui::test]
    async fn native_find_reveals_unmounted_results_without_taking_input_focus(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let sections = |start, end| {
            (start..end)
                .map(|index| {
                    format!(
                        "### Container {index}\n\n```\ndocker run -d \\\n  --name container-{index} \\\n  example/image:latest\n```\n\n"
                    )
                })
                .collect::<String>()
        };
        let source = format!(
            "# Docker CLI\n\n{}### TeamSpeak\n\n```\ndocker run -d \\\n  --name teamspeak \\\n  -v /home/teamspeak:/var/ts3server \\\n  teamspeak\n```\n\n{}The final team result.",
            sections(0, 50),
            sections(50, 100),
        );
        let (editor, visual) = cx.add_window_view(|_, cx| Editor::from_markdown(cx, source, None));
        for _ in 0..4 {
            draw(visual);
        }
        let original = editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx));
        visual.simulate_keystrokes("cmd-f");
        draw(visual);
        visual.simulate_input("team");
        // Visit the heading, each code-block match, a distant paragraph, and
        // then wrap back. Bounds, rather than scroll offsets, prove visibility.
        for index in 0..6 {
            for _ in 0..12 {
                draw(visual);
            }
            visual.update(|window, cx| {
                let editor = editor.read(cx);
                assert_eq!(editor.writing.matches.len(), 5);
                assert_eq!(editor.writing.current, Some(index % 5));
                assert!(
                    editor
                        .writing
                        .query
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
                let target = editor.current_edit_target_from_state(cx).unwrap();
                let bounds = target
                    .read(cx)
                    .active_range_or_cursor_bounds()
                    .expect("the search result must be mounted while the query owns focus");
                let viewport = editor.scroll_handle.bounds();
                assert!(
                    bounds.top() >= viewport.top() + gpui::px(84.0),
                    "result {index} is above the viewport: {bounds:?} / {viewport:?}"
                );
                assert!(
                    bounds.bottom() <= viewport.bottom() - gpui::px(84.0),
                    "result {index} is below the viewport: {bounds:?} / {viewport:?}"
                );
                assert!(!editor.pending_scroll_active_block_into_view);
                assert_eq!(editor.serialized_document_text(cx), original);
            });
            visual.simulate_keystrokes("enter");
        }
    }

    #[gpui::test]
    async fn oversized_source_selection_scroll_settles(cx: &mut TestAppContext) {
        init(cx);
        let source = "A long source document.\n\n".repeat(100);
        let (editor, visual) = cx.add_window_view(|_, cx| {
            let mut editor = Editor::from_markdown(cx, source, None);
            editor.toggle_view_mode(cx);
            editor
        });
        for _ in 0..4 {
            draw(visual);
        }
        visual.simulate_keystrokes("cmd-end cmd-a");
        editor.update(visual, |editor, cx| {
            editor.request_active_block_scroll_into_view(cx);
        });
        for _ in 0..12 {
            draw(visual);
        }
        editor.read_with(visual, |editor, cx| {
            assert!(!editor.pending_scroll_active_block_into_view);
            assert!(editor.scroll_recheck_task.is_none());
            let target = editor.current_edit_target_from_state(cx).unwrap();
            assert_eq!(
                target.read(cx).selected_range.len(),
                target.read(cx).display_text().len()
            );
        });
    }

    #[gpui::test]
    async fn repeated_heading_changes_keep_escaped_text_and_history_bounded(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let markdown = concat!(
            "# Heading\n\n",
            "- 尖括号：\\< \\>\n\n",
            "<https://example.com/a_b\\c>\n\n",
            "[ref]: https://example.com\n",
        );
        let editor = cx.new(|cx| Editor::from_markdown(cx, markdown.into(), None));
        let original = editor.read_with(cx, |editor, cx| editor.serialized_document_text(cx));
        let body = original.split_once('\n').unwrap().1;
        let mut retired = Vec::new();
        for round in 0..64 {
            editor.update(cx, |editor, cx| {
                retired.extend(
                    editor
                        .document
                        .visible_blocks()
                        .iter()
                        .map(|block| block.entity.downgrade()),
                );
                let (command, heading) = if round % 2 == 0 {
                    (WritingCommand::Heading2, "## Heading")
                } else {
                    (WritingCommand::Heading1, "# Heading")
                };
                editor.apply_writing_format(command, cx);
                assert_eq!(
                    editor.serialized_document_text(cx),
                    format!("{heading}\n{body}")
                );
                assert_eq!(editor.undo_history.len(), round + 1);
                assert!(
                    editor
                        .undo_history
                        .iter()
                        .all(|entry| entry.source_text.len() <= original.len() + 1)
                );
            });
            cx.run_until_parked();
            assert!(retired.iter().all(|block| block.upgrade().is_none()));
        }
        editor.update(cx, |editor, cx| {
            for _ in 0..64 {
                editor.undo_document(cx);
                assert_eq!(
                    editor
                        .serialized_document_text(cx)
                        .split_once('\n')
                        .unwrap()
                        .1,
                    body
                );
            }
            assert_eq!(editor.serialized_document_text(cx), original);
            for _ in 0..64 {
                editor.redo_document(cx);
                assert_eq!(
                    editor
                        .serialized_document_text(cx)
                        .split_once('\n')
                        .unwrap()
                        .1,
                    body
                );
            }
            assert_eq!(editor.serialized_document_text(cx), original);
        });
    }

    #[gpui::test]
    async fn regex_replace_all_is_one_undo_step_and_redo_preserves_markdown(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let editor = cx.new(|cx| {
            Editor::from_markdown(cx, "Rust-12 **rust-34**\n\n你好 Rust-56".into(), None)
        });
        editor.update(cx, |editor, cx| {
            let original = editor.serialized_document_text(cx);
            editor.ensure_search_fields(cx);
            editor.writing.search_open = true;
            editor.writing.regex = true;
            editor
                .writing
                .query
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value(r"(rust)-(\d+)", cx));
            editor
                .writing
                .replacement
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value("$1:$2", cx));
            editor.refresh_search(true, cx);
            assert_eq!(editor.writing.matches.len(), 3);
            editor.replace_search_match(true, cx);
            let edited = editor.serialized_document_text(cx);
            assert!(edited.contains("Rust:12 **rust:34**"));
            assert!(edited.contains("你好 Rust:56"));
            assert_eq!(editor.undo_history.len(), 1);
            editor.undo_document(cx);
            assert_eq!(editor.serialized_document_text(cx), original);
            editor.redo_document(cx);
            assert_eq!(editor.serialized_document_text(cx), edited);
        });
    }

    #[gpui::test]
    async fn configured_source_mode_shortcut_replaces_default_and_can_reset(
        cx: &mut TestAppContext,
    ) {
        use std::collections::BTreeMap;
        init(cx);
        let default_key = if cfg!(target_os = "macos") {
            "cmd-/"
        } else {
            "ctrl-/"
        };
        let custom_key = if cfg!(target_os = "macos") {
            "cmd-alt-u"
        } else {
            "ctrl-alt-u"
        };
        cx.update(|cx| {
            cx.clear_key_bindings();
            crate::components::install_keybindings(
                cx,
                &BTreeMap::from([("toggle_view_mode".into(), vec![custom_key.into()])]),
            );
        });
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, "# Hello\n\nWorld".into(), None));
        draw(visual);
        visual.simulate_keystrokes(default_key);
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Rendered));
        visual.simulate_keystrokes(custom_key);
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Source));
        visual.simulate_keystrokes(custom_key);
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Rendered));
        visual.update(|_, cx| {
            cx.clear_key_bindings();
            crate::components::install_keybindings(cx, &BTreeMap::new());
        });
        visual.simulate_keystrokes(custom_key);
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Rendered));
        visual.simulate_keystrokes(default_key);
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Source));
    }

    #[gpui::test]
    async fn writing_shortcuts_switch_modes_and_keep_zoom_local_to_the_window(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let (editor, visual) =
            cx.add_window_view(|_, cx| Editor::from_markdown(cx, "# Hello\n\nWorld".into(), None));
        draw(visual);
        visual.simulate_keystrokes("cmd-shift-1");
        assert!(editor.read_with(visual, |editor, _| editor.writing.focus_mode));
        visual.dispatch_action(action(WritingCommand::ZoomIn));
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert_eq!(editor.writing.zoom_steps, 1);
            assert_eq!(
                editor.document.first_root().unwrap().read(cx).document_zoom,
                1.1
            );
        });
        visual.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-/"
        } else {
            "ctrl-/"
        });
        draw(visual);
        assert!(editor.read_with(visual, |editor, _| editor.view_mode
            == crate::editor::ViewMode::Source));
        visual.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-/"
        } else {
            "ctrl-/"
        });
        draw(visual);
        assert!(editor.read_with(visual, |editor, cx| {
            editor.serialized_document_text(cx).contains("# Hello")
        }));
    }

    #[gpui::test]
    async fn read_only_blocks_typing_structural_keys_and_ime_in_both_views(
        cx: &mut TestAppContext,
    ) {
        use gpui::EntityInputHandler;
        init(cx);
        let (editor, visual) = cx.add_window_view(|_, cx| {
            Editor::from_markdown(
                cx,
                "# Hello\n\n- [ ] Task\n\n| A | B |\n| --- | --- |\n| 1 | 2 |".into(),
                None,
            )
        });
        draw(visual);
        let original = editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx));
        visual.simulate_keystrokes("cmd-shift-r");
        draw(visual);
        visual.simulate_input("should not be inserted");
        visual.simulate_keystrokes("backspace enter tab");
        visual.dispatch_action(action(WritingCommand::Heading2));
        draw(visual);
        editor.read_with(visual, |editor, cx| {
            assert!(editor.writing.read_only);
            assert_eq!(editor.serialized_document_text(cx), original);
            assert!(!editor.document_dirty);
            assert!(
                editor
                    .table_cells
                    .values()
                    .all(|binding| binding.cell.read(cx).read_only)
            );
        });
        visual.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-/"
        } else {
            "ctrl-/"
        });
        draw(visual);
        visual.update(|window, cx| {
            let block = editor.read(cx).document.first_root().unwrap().clone();
            block.update(cx, |block, cx| {
                block.replace_and_mark_text_in_range(None, "中文输入", None, window, cx);
                block.replace_text_in_range(None, "committed text", window, cx);
            });
        });
        assert_eq!(
            editor.read_with(visual, |editor, cx| editor.serialized_document_text(cx)),
            original
        );
        visual.simulate_keystrokes("cmd-shift-r");
        draw(visual);
        visual.simulate_input("editable again");
        draw(visual);
        assert!(editor.read_with(visual, |editor, cx| {
            editor
                .serialized_document_text(cx)
                .contains("editable again")
        }));
    }

    #[test]
    fn regex_supports_unicode_whole_words_captures_and_invalid_patterns() {
        let regex = search_regex(r"(rust)-(\d+)", false, true).unwrap();
        assert_eq!(
            regex.replace_all("RUST-12 rust-34 rust-56suffix", "$1:$2"),
            "RUST:12 rust:34 rust-56suffix"
        );
        assert!(search_regex("[", false, false).is_err());
        assert_eq!(
            search_regex("^", true, false)
                .unwrap()
                .find_iter("甲\n乙")
                .count(),
            2
        );
    }
    #[test]
    fn search_preserves_utf8_offsets_and_case_expansions() {
        let source = "你好 Rust RUST İST 中文";
        let ranges = find_matches(source, "rust", false, false);
        assert_eq!(
            ranges
                .iter()
                .map(|r| &source[r.clone()])
                .collect::<Vec<_>>(),
            vec!["Rust", "RUST"]
        );
        let range = find_matches(source, "i\u{307}st", false, false)
            .pop()
            .unwrap();
        assert_eq!(&source[range], "İST");
        assert!(find_matches("İ", "i", false, false).is_empty());
        assert_eq!(find_matches(source, "中文", true, false).len(), 1);
    }
    #[test]
    fn search_handles_whole_words_empty_queries_and_non_overlapping_results() {
        assert_eq!(
            find_matches("one someone one_one ONE", "one", false, true),
            vec![0..3, 20..23]
        );
        assert!(find_matches("test", "", false, false).is_empty());
        assert_eq!(find_matches("aaaa", "aa", true, false), vec![0..2, 2..4]);
    }
    #[test]
    fn formatting_respects_line_boundaries_and_existing_heading_markers() {
        let source = "## Hello\nworld\nnext";
        assert_eq!(
            format_source(source, 3..9, WritingCommand::Heading1)
                .unwrap()
                .0,
            "# Hello\nworld\nnext"
        );
        assert_eq!(
            format_source(source, 3..8, WritingCommand::Paragraph)
                .unwrap()
                .0,
            "Hello\nworld\nnext"
        );
        assert_eq!(
            format_source("中文", 0..6, WritingCommand::Bold).unwrap(),
            ("**中文**".into(), 2..8)
        );
    }
    #[test]
    fn statistics_exclude_markdown_delimiters_and_link_destinations() {
        let stats = document_statistics("# Hello\n\n**World** [link](https://example.com)\n\n你好");
        assert_eq!(stats.words, 5);
        assert_eq!(stats.non_whitespace, "HelloWorldlink你好".chars().count());
    }
}
