//! Native single-line settings input with UTF-16 IME ranges and a horizontal viewport.

use std::ops::Range;

use gpui::*;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::theme::ThemeManager;

/// Fixed outer height of a settings input, in pixels.
const SINGLE_LINE_INPUT_HEIGHT: f32 = 36.0;
/// Number of trailing graphemes faded in unfocused overflow mode.
const FADE_GRAPHEME_COUNT: usize = 4;

/// Emitted after the field's stored value is replaced.
pub(crate) struct InputChanged;

/// Controls how unfocused input text is presented when it exceeds the field.
/// Focused fields always use a clipped, cursor-following viewport so editing
/// remains predictable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // All modes are part of the component API; callers choose as needed.
pub(crate) enum SingleLineOverflow {
    /// Show the longest fitting grapheme prefix followed by an ellipsis.
    #[default]
    Ellipsis,
    /// Clip the shaped text at the field edge.
    Clip,
    /// Reduce opacity across the last visible graphemes.
    Fade,
}

/// Editable single-line value with UTF-8 selection state and UTF-16 GPUI input.
pub(crate) struct SingleLineInput {
    /// Focus target registered with GPUI.
    focus: FocusHandle,
    /// Source text, without the displayed placeholder or ellipsis.
    content: SharedString,
    /// Ordered selection in UTF-8 byte offsets of `content`.
    selection: Range<usize>,
    /// Whether the active selection endpoint is `selection.start`.
    reversed: bool,
    /// IME composition range in UTF-8 byte offsets of `content`.
    marked: Option<Range<usize>>,
    /// Most recently painted line, which may contain display-only text.
    last_layout: Option<ShapedLine>,
    /// Bounds using the line's painted origin, including viewport scroll.
    last_bounds: Option<Bounds<Pixels>>,
    /// Source byte endpoint shown before any display-only ellipsis.
    last_source_end: usize,
    /// Text drawn when the source value is empty.
    placeholder: SharedString,
    /// Whether a pointer drag is extending the selection.
    is_selecting: bool,
    /// Presentation mode used only while unfocused and overflowing.
    overflow: SingleLineOverflow,
    /// Horizontal offset shared by text, cursor, selection, and hit testing.
    viewport_offset: Pixels,
    /// Width of the shaped text displayed in the last paint.
    content_width: Pixels,
    /// Width available to text in the last paint.
    viewport_width: Pixels,
    /// Requests caret visibility on the next prepaint.
    ensure_cursor_visible: bool,
    /// Whether the field was focused during the preceding prepaint.
    was_focused: bool,
}

impl EventEmitter<InputChanged> for SingleLineInput {}
impl Focusable for SingleLineInput {
    /// Returns the field's focus target.
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SingleLineInput {
    /// Creates a field with its caret at the end of the initial value.
    pub(crate) fn new(
        value: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = value.into();
        let end = content.len();
        Self {
            focus: cx.focus_handle(),
            content,
            selection: end..end,
            reversed: false,
            marked: None,
            last_layout: None,
            last_bounds: None,
            last_source_end: end,
            placeholder: placeholder.into(),
            is_selecting: false,
            overflow: SingleLineOverflow::default(),
            viewport_offset: px(0.0),
            content_width: px(0.0),
            viewport_width: px(0.0),
            ensure_cursor_visible: true,
            was_focused: false,
        }
    }

    /// Sets how overflowing text is presented when the field is unfocused.
    pub(crate) fn with_overflow(mut self, overflow: SingleLineOverflow) -> Self {
        self.overflow = overflow;
        self
    }
    /// Returns the unmodified source value.
    pub(crate) fn value(&self) -> &str {
        &self.content
    }
    #[cfg(test)]
    /// Replaces the source value and emits a change event in component tests.
    pub(crate) fn set_value(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = value.into();
        self.selection = self.content.len()..self.content.len();
        self.marked = None;
        self.ensure_cursor_visible = true;
        cx.emit(InputChanged);
        cx.notify();
    }
    /// Returns the active selection endpoint as a UTF-8 byte offset.
    fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }
    /// Finds the preceding grapheme boundary in source text.
    fn previous(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }
    /// Finds the following grapheme boundary in source text.
    fn next(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.content.len())
    }
    /// Collapses the selection at a scalar-aligned UTF-8 byte offset.
    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = crate::text_offsets::floor_char_boundary(&self.content, offset);
        self.selection = offset..offset;
        self.reversed = false;
        self.ensure_cursor_visible = true;
        cx.notify();
    }
    /// Extends the selection to a scalar-aligned UTF-8 byte offset.
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = crate::text_offsets::floor_char_boundary(&self.content, offset);
        if self.reversed {
            self.selection.start = offset;
        } else {
            self.selection.end = offset;
        }
        if self.selection.end < self.selection.start {
            self.reversed = !self.reversed;
            self.selection = self.selection.end..self.selection.start;
        }
        self.ensure_cursor_visible = true;
        cx.notify();
    }
    /// Hit-tests painted text, clamping any display-only suffix to source text.
    fn index_at(&self, point: Point<Pixels>) -> usize {
        let index = match (&self.last_bounds, &self.last_layout) {
            (_, _) if self.content.is_empty() => 0,
            (Some(bounds), Some(line)) => line.closest_index_for_x(point.x - bounds.left()),
            _ => 0,
        };
        let index = clamp_source_index(&self.content, index, self.last_source_end);
        if self.content.is_char_boundary(index) {
            index
        } else {
            self.previous(index)
        }
    }
    /// Focuses the field and begins a pointer selection.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        self.is_selecting = true;
        let index = self.index_at(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }
    /// Extends the selection while dragging.
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_at(event.position), cx);
        }
    }
    /// Ends the pointer selection session.
    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }
    /// Handles selection, navigation, clipboard, and deletion keys locally.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let shift = event.keystroke.modifiers.shift;
        let command = event.keystroke.modifiers.platform;
        match (command, shift, key) {
            (true, _, "a") => {
                self.selection = 0..self.content.len();
                self.reversed = false;
                self.ensure_cursor_visible = true;
                cx.notify();
            }
            (true, _, "c") => self.copy(cx),
            (true, _, "x") => {
                self.copy(cx);
                self.replace_text_in_range(None, "", window, cx);
            }
            (true, _, "v") => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace_text_in_range(None, &text, window, cx);
                }
            }
            (_, true, "left") => {
                let p = self.previous(self.cursor());
                self.select_to(p, cx);
            }
            (_, true, "right") => {
                let p = self.next(self.cursor());
                self.select_to(p, cx);
            }
            (_, _, "left") => {
                let p = if self.selection.is_empty() {
                    self.previous(self.cursor())
                } else {
                    self.selection.start
                };
                self.move_to(p, cx);
            }
            (_, _, "right") => {
                let p = if self.selection.is_empty() {
                    self.next(self.cursor())
                } else {
                    self.selection.end
                };
                self.move_to(p, cx);
            }
            (_, true, "home") => self.select_to(0, cx),
            (_, true, "end") => self.select_to(self.content.len(), cx),
            (_, _, "home") => self.move_to(0, cx),
            (_, _, "end") => self.move_to(self.content.len(), cx),
            (_, _, "backspace") => {
                if self.selection.is_empty() {
                    self.selection = self.previous(self.cursor())..self.cursor();
                }
                self.replace_text_in_range(None, "", window, cx);
            }
            (_, _, "delete") => {
                if self.selection.is_empty() {
                    self.selection = self.cursor()..self.next(self.cursor());
                }
                self.replace_text_in_range(None, "", window, cx);
            }
            (_, _, "enter") => {}
            _ => return,
        }
        cx.stop_propagation();
    }
    /// Copies the selected source text, if any.
    fn copy(&self, cx: &mut App) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selection.clone()].to_string(),
            ));
        }
    }
    /// Converts a source byte offset to UTF-16 code units.
    fn utf8_to_utf16(&self, offset: usize) -> usize {
        crate::text_offsets::utf8_to_utf16(&self.content, offset)
    }
    /// Converts an IME range to scalar-aligned source byte offsets.
    fn utf16_range_to_utf8(&self, range: Range<usize>) -> Range<usize> {
        crate::text_offsets::utf16_range_to_utf8(&self.content, &range)
    }
    /// Converts an ordered source byte range to UTF-16 code units.
    fn utf8_range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        crate::text_offsets::utf8_range_to_utf16(&self.content, &range)
    }
    /// Scrolls the focused field only for dominant horizontal overflow gestures.
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.is_focused(window) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        let delta_x = f32::from(delta.x);
        let delta_y = f32::from(delta.y);
        if !should_handle_horizontal_scroll(
            delta_x,
            delta_y,
            f32::from(self.content_width),
            f32::from(self.viewport_width),
        ) {
            return;
        }
        self.viewport_offset = px(clamp_viewport_offset(
            f32::from(self.viewport_offset) + delta_x,
            f32::from(self.content_width),
            f32::from(self.viewport_width),
        ));
        // A deliberate, overflowing horizontal gesture owns this event.
        // Vertical gestures (including their small trackpad x jitter) bubble
        // to the preferences page.
        cx.stop_propagation();
        cx.notify();
    }
}

impl EntityInputHandler for SingleLineInput {
    /// Returns source text for a UTF-16 IME range and its normalized range.
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.utf16_range_to_utf8(range);
        actual.replace(self.utf8_range_to_utf16(range.clone()));
        Some(self.content[range].to_string())
    }
    /// Reports the current UTF-8 selection as a UTF-16 range.
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.utf8_range_to_utf16(self.selection.clone()),
            reversed: self.reversed,
        })
    }
    /// Reports the current IME mark as a UTF-16 range.
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .clone()
            .map(|range| self.utf8_range_to_utf16(range))
    }
    /// Clears the IME mark without changing source text.
    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }
    /// Replaces an explicit UTF-16 range, or the marked/selected range.
    ///
    /// Line breaks become spaces so the stored value remains single-line.
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.utf16_range_to_utf8(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        let range = crate::text_offsets::normalize_utf8_range(&self.content, range);
        let text = text.replace(['\r', '\n'], " ");
        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            text,
            &self.content[range.end..]
        )
        .into();
        let end = range.start + text.len();
        self.selection = end..end;
        self.marked = None;
        self.reversed = false;
        self.ensure_cursor_visible = true;
        cx.emit(InputChanged);
        cx.notify();
    }
    /// Replaces text during composition and selects within the insertion.
    ///
    /// The optional selection is a UTF-16 range relative to the newline-flattened
    /// inserted text; a nonempty insertion remains marked.
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = range
            .as_ref()
            .map(|range| self.utf16_range_to_utf8(range.clone()).start)
            .or_else(|| self.marked.as_ref().map(|r| r.start))
            .unwrap_or(self.selection.start);
        self.replace_text_in_range(range, text, window, cx);
        let sanitized = text.replace(['\r', '\n'], " ");
        if !sanitized.is_empty() {
            self.marked = Some(start..start + sanitized.len());
        }
        if let Some(selected) = selected {
            let relative = crate::text_offsets::utf16_range_to_utf8(&sanitized, &selected);
            self.selection = start + relative.start..start + relative.end;
        }
        self.ensure_cursor_visible = true;
    }
    /// Resolves a UTF-16 source range against the last painted line origin.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.utf16_range_to_utf8(range);
        let line = self.last_layout.as_ref()?;
        let origin_x = self
            .last_bounds
            .as_ref()
            .map(Bounds::left)
            .unwrap_or_else(|| bounds.left());
        Some(Bounds::from_corners(
            point(origin_x + line.x_for_index(range.start), bounds.top()),
            point(origin_x + line.x_for_index(range.end), bounds.bottom()),
        ))
    }
    /// Hit-tests the painted line and returns a UTF-16 source offset.
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.utf8_to_utf16(self.index_at(point)))
    }
}

impl Render for SingleLineInput {
    /// Builds the input surface and its custom text element.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<ThemeManager>().current();
        let c = &theme.colors;
        let d = &theme.dimensions;
        let t = &theme.typography;
        div()
            .id("single-line-input")
            .key_context("SingleLineInput")
            .track_focus(&self.focus)
            .w_full()
            .min_w(px(0.0))
            .h(px(SINGLE_LINE_INPUT_HEIGHT))
            .px(px(10.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(d.menu_item_radius))
            .border(px(d.dialog_border_width))
            .border_color(c.dialog_border)
            .bg(c.dialog_secondary_button_bg)
            .text_color(c.dialog_body)
            .text_size(px(t.dialog_body_size))
            .cursor_text()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(InputTextElement { input: cx.entity() })
    }
}

/// Custom text element used for shaping, viewport painting, and IME registration.
struct InputTextElement {
    /// Backing field whose text and focus state are painted.
    input: Entity<SingleLineInput>,
}
/// Geometry and paint data shared between prepaint and paint.
struct InputPrepaint {
    /// Shaped display line, possibly truncated while unfocused.
    line: ShapedLine,
    /// Source byte endpoint shown in `line`.
    source_end: usize,
    /// Painted x origin after applying the focused viewport offset.
    paint_origin_x: Pixels,
    /// Caret quad, if focused with no selection.
    cursor: Option<PaintQuad>,
    /// Selection quad, if focused with a nonempty selection.
    selection: Option<PaintQuad>,
}
impl IntoElement for InputTextElement {
    /// The custom element's concrete GPUI element type.
    type Element = Self;
    /// Uses this custom element directly in the GPUI tree.
    fn into_element(self) -> Self {
        self
    }
}
impl Element for InputTextElement {
    /// Layout requests do not retain intermediate state.
    type RequestLayoutState = ();
    /// Prepaint data consumed by the paint pass.
    type PrepaintState = InputPrepaint;
    /// This anonymous element has no persistent element ID.
    fn id(&self) -> Option<ElementId> {
        None
    }
    /// This element does not report a source location.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    /// Requests a full-width, full-height text viewport.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut s = Style::default();
        s.size.width = relative(1.).into();
        s.size.height = relative(1.).into();
        (window.request_layout(s, [], cx), ())
    }
    /// Shapes displayed text and computes viewport, caret, and selection geometry.
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> InputPrepaint {
        let input = self.input.read(cx);
        let empty = input.content.is_empty();
        let text = if empty {
            input.placeholder.clone()
        } else {
            input.content.clone()
        };
        let style = window.text_style();
        let color = if empty {
            style.color.opacity(0.5)
        } else {
            style.color
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let full_line = window.text_system().shape_line(
            text.clone(),
            font_size,
            std::slice::from_ref(&run),
            None,
        );
        let focused = input.focus.is_focused(window);
        let overflowed = full_line.width > bounds.size.width;
        let (line, source_end) = if !focused && overflowed {
            match input.overflow {
                SingleLineOverflow::Clip => (full_line, input.content.len()),
                SingleLineOverflow::Ellipsis => {
                    let ellipsis = "…";
                    let ellipsis_line = window.text_system().shape_line(
                        ellipsis.into(),
                        font_size,
                        &[TextRun {
                            len: ellipsis.len(),
                            ..run.clone()
                        }],
                        None,
                    );
                    let end = fitting_grapheme_end(
                        &text,
                        bounds.size.width - ellipsis_line.width,
                        &full_line,
                    );
                    let display = format!("{}{}", &text[..end], ellipsis);
                    (
                        window.text_system().shape_line(
                            display.clone().into(),
                            font_size,
                            &[TextRun {
                                len: display.len(),
                                ..run.clone()
                            }],
                            None,
                        ),
                        end.min(input.content.len()),
                    )
                }
                SingleLineOverflow::Fade => {
                    let end = fitting_grapheme_end(&text, bounds.size.width, &full_line);
                    let display = &text[..end];
                    let mut runs = Vec::new();
                    let segments = fade_segments(display);
                    let fade_start = segments.first().map_or(0, |segment| segment.range.start);
                    if fade_start != 0 {
                        runs.push(TextRun {
                            len: fade_start,
                            ..run.clone()
                        });
                    }
                    for segment in segments {
                        runs.push(TextRun {
                            len: segment.range.len(),
                            color: color.opacity(segment.opacity),
                            ..run.clone()
                        });
                    }
                    debug_assert_eq!(display.len(), runs.iter().map(|run| run.len).sum::<usize>());
                    (
                        window.text_system().shape_line(
                            display.to_string().into(),
                            font_size,
                            &runs,
                            None,
                        ),
                        end.min(input.content.len()),
                    )
                }
            }
        } else {
            (full_line, input.content.len())
        };
        let content_width = line.width;
        let viewport_width = bounds.size.width;
        let selection = input.selection.clone();
        let selection_empty = selection.is_empty();
        let cursor_index = input.cursor();
        let mut viewport_offset = if focused {
            f32::from(input.viewport_offset)
        } else {
            0.0
        };
        if focused && (!input.was_focused || input.ensure_cursor_visible) {
            viewport_offset = cursor_visible_offset(
                viewport_offset,
                f32::from(line.x_for_index(cursor_index)),
                f32::from(content_width),
                f32::from(viewport_width),
            );
        }
        viewport_offset = clamp_viewport_offset(
            viewport_offset,
            f32::from(content_width),
            f32::from(viewport_width),
        );
        self.input.update(cx, |input, _| {
            input.content_width = content_width;
            input.viewport_width = viewport_width;
            input.viewport_offset = px(viewport_offset);
            input.ensure_cursor_visible = false;
            input.was_focused = focused;
        });
        let paint_origin_x = bounds.left() - px(if focused { viewport_offset } else { 0.0 });
        let focused_line = focused.then_some((&line, paint_origin_x));
        let selection = focused_line
            .filter(|_| !selection_empty)
            .map(|(line, origin_x)| {
                fill(
                    Bounds::from_corners(
                        point(origin_x + line.x_for_index(selection.start), bounds.top()),
                        point(origin_x + line.x_for_index(selection.end), bounds.bottom()),
                    ),
                    hsla(0.58, 0.7, 0.5, 0.35),
                )
            });
        let cursor = (focused && selection_empty).then(|| {
            fill(
                Bounds::new(
                    point(
                        paint_origin_x + line.x_for_index(cursor_index),
                        bounds.top() + px(4.),
                    ),
                    size(px(1.), bounds.size.height - px(8.)),
                ),
                style.color,
            )
        });
        InputPrepaint {
            line,
            source_end,
            paint_origin_x,
            cursor,
            selection,
        }
    }
    /// Registers the focused IME handler, paints clipped text, and saves hit-test data.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut InputPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        if focus.is_focused(window) {
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(q) = state.selection.take() {
                window.paint_quad(q)
            }
            state
                .line
                .paint(
                    point(state.paint_origin_x, bounds.top()),
                    bounds.size.height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .ok();
            if let Some(q) = state.cursor.take() {
                window.paint_quad(q)
            }
        });
        let layout_bounds = Bounds::new(point(state.paint_origin_x, bounds.top()), bounds.size);
        self.input.update(cx, |input, _| {
            input.last_layout = Some(state.line.clone());
            input.last_bounds = Some(layout_bounds);
            input.last_source_end = state.source_end;
        });
    }
}

/// Returns the furthest horizontal scroll offset for the shaped content.
fn max_viewport_offset(content_width: f32, viewport_width: f32) -> f32 {
    (content_width - viewport_width).max(0.0)
}

/// Clamps a horizontal scroll offset to the available overflow.
fn clamp_viewport_offset(offset: f32, content_width: f32, viewport_width: f32) -> f32 {
    offset.clamp(0.0, max_viewport_offset(content_width, viewport_width))
}

/// Adjusts a scroll offset only when the caret lies beyond the viewport edge.
fn cursor_visible_offset(
    offset: f32,
    cursor_x: f32,
    content_width: f32,
    viewport_width: f32,
) -> f32 {
    /// Space kept to the right of a caret brought into view, in pixels.
    const CURSOR_MARGIN: f32 = 2.0;
    let offset = clamp_viewport_offset(offset, content_width, viewport_width);
    let target = if cursor_x < offset {
        cursor_x
    } else if cursor_x > offset + viewport_width - CURSOR_MARGIN {
        cursor_x - viewport_width + CURSOR_MARGIN
    } else {
        offset
    };
    clamp_viewport_offset(target, content_width, viewport_width)
}

/// Accepts a horizontal gesture only while content actually overflows.
fn should_handle_horizontal_scroll(
    delta_x: f32,
    delta_y: f32,
    content_width: f32,
    viewport_width: f32,
) -> bool {
    content_width > viewport_width && delta_x.abs() > delta_y.abs()
}

/// Finds the last complete grapheme fitting in the shaped line width.
fn fitting_grapheme_end(text: &str, available_width: Pixels, line: &ShapedLine) -> usize {
    fitting_grapheme_end_by(text, |end| line.x_for_index(end) <= available_width)
}

/// Finds the longest grapheme prefix satisfying a caller-supplied width test.
fn fitting_grapheme_end_by(text: &str, mut fits: impl FnMut(usize) -> bool) -> usize {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .take_while(|end| fits(*end))
        .last()
        .unwrap_or(0)
}

/// Removes any display-only suffix and floors the result to a scalar boundary.
fn clamp_source_index(content: &str, display_index: usize, source_end: usize) -> usize {
    let mut index = display_index.min(source_end).min(content.len());
    while !content.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Debug, PartialEq)]
/// Byte range and opacity of one trailing grapheme in fade mode.
struct FadeSegment {
    /// Grapheme byte range in the shaped display text.
    range: Range<usize>,
    /// Opacity applied to that grapheme's text run.
    opacity: f32,
}

/// Splits the trailing graphemes into progressively dimmer text runs.
fn fade_segments(text: &str) -> Vec<FadeSegment> {
    let graphemes = text.grapheme_indices(true).collect::<Vec<_>>();
    let fade_start = graphemes.len().saturating_sub(FADE_GRAPHEME_COUNT);
    graphemes[fade_start..]
        .iter()
        .enumerate()
        .map(|(index, (start, grapheme))| FadeSegment {
            range: *start..*start + grapheme.len(),
            opacity: 1.0 - (index + 1) as f32 / (graphemes.len() - fade_start + 1) as f32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        FADE_GRAPHEME_COUNT, SingleLineOverflow, clamp_source_index, clamp_viewport_offset,
        cursor_visible_offset, fade_segments, fitting_grapheme_end_by,
        should_handle_horizontal_scroll,
    };
    use gpui::{AppContext as _, EntityInputHandler as _, TestAppContext};
    use unicode_segmentation::UnicodeSegmentation as _;

    #[test]
    fn fitting_end_respects_unicode_grapheme_boundaries() {
        let text = "A👨‍👩‍👧‍👦藏e\u{301}Z";
        let boundaries = text
            .grapheme_indices(true)
            .map(|(start, grapheme)| start + grapheme.len())
            .collect::<Vec<_>>();
        let end = fitting_grapheme_end_by(text, |end| end <= boundaries[2]);
        assert_eq!(end, boundaries[2]);
        assert_eq!(&text[..end], "A👨‍👩‍👧‍👦藏");
    }

    #[test]
    fn displayed_suffix_cannot_move_the_source_index_past_its_end() {
        let content = "ab藏defghijklmnopqrstuvwxyz";
        let displayed_source_end = 12;
        let ellipsis_display_end = displayed_source_end + "…".len();
        assert_eq!(
            clamp_source_index(content, ellipsis_display_end, displayed_source_end),
            displayed_source_end
        );
        assert_eq!(clamp_source_index(content, 4, content.len()), 2);
    }

    #[test]
    fn fade_segments_cover_complete_trailing_graphemes() {
        let text = "prefix👨‍👩‍👧‍👦藏e\u{301}Z";
        let segments = fade_segments(text);
        assert_eq!(segments.len(), FADE_GRAPHEME_COUNT);
        assert_eq!(segments.first().unwrap().range.start, "prefix".len());
        assert_eq!(segments.last().unwrap().range.end, text.len());
        assert!(segments.windows(2).all(|pair| {
            pair[0].range.end == pair[1].range.start && pair[0].opacity > pair[1].opacity
        }));
    }

    #[test]
    fn all_overflow_modes_are_distinct() {
        let modes = [
            SingleLineOverflow::Ellipsis,
            SingleLineOverflow::Clip,
            SingleLineOverflow::Fade,
        ];
        assert_ne!(modes[0], modes[1]);
        assert_ne!(modes[1], modes[2]);
    }

    #[test]
    fn viewport_offset_is_zero_when_content_does_not_overflow() {
        assert_eq!(clamp_viewport_offset(42.0, 80.0, 100.0), 0.0);
    }

    #[test]
    fn viewport_offset_is_clamped_to_content_edges() {
        assert_eq!(clamp_viewport_offset(-20.0, 300.0, 100.0), 0.0);
        assert_eq!(clamp_viewport_offset(240.0, 300.0, 100.0), 200.0);
    }

    #[test]
    fn viewport_scroll_position_is_preserved_without_cursor_request() {
        assert_eq!(clamp_viewport_offset(72.0, 300.0, 100.0), 72.0);
    }

    #[test]
    fn cursor_visibility_moves_the_viewport_only_past_its_edges() {
        assert_eq!(cursor_visible_offset(50.0, 80.0, 300.0, 100.0), 50.0);
        assert_eq!(cursor_visible_offset(50.0, 40.0, 300.0, 100.0), 40.0);
        assert_eq!(cursor_visible_offset(50.0, 180.0, 300.0, 100.0), 82.0);
        assert_eq!(cursor_visible_offset(150.0, 299.0, 300.0, 100.0), 200.0);
    }

    #[test]
    fn horizontal_scroll_leaves_vertical_and_non_overflowing_inputs_to_the_page() {
        assert!(!should_handle_horizontal_scroll(0.5, 12.0, 300.0, 100.0));
        assert!(!should_handle_horizontal_scroll(12.0, 0.0, 100.0, 100.0));
        assert!(should_handle_horizontal_scroll(12.0, 0.5, 300.0, 100.0));
    }

    #[gpui::test]
    async fn component_accepts_every_overflow_mode(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        for mode in [
            SingleLineOverflow::Ellipsis,
            SingleLineOverflow::Clip,
            SingleLineOverflow::Fade,
        ] {
            let input =
                cx.new(|cx| super::SingleLineInput::new("long value", "", cx).with_overflow(mode));
            input.read_with(cx, |input, _cx| assert_eq!(input.overflow, mode));
        }
    }

    #[gpui::test]
    async fn ime_mark_replace_and_cancel_preserve_unicode_ranges(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let input = cx.new(|cx| super::SingleLineInput::new("a😀b", "", cx));

        cx.update(|window, cx| {
            input.update(cx, |input, input_cx| {
                input.replace_and_mark_text_in_range(
                    Some(1..3),
                    "测e\u{301}",
                    Some(2..3),
                    window,
                    input_cx,
                );
                assert_eq!(input.value(), "a测e\u{301}b");
                assert_eq!(input.marked, Some(1..7));
                assert_eq!(input.selection, 5..7);

                let mut actual = None;
                assert_eq!(
                    input.text_for_range(2..3, &mut actual, window, input_cx),
                    Some("e".to_string())
                );
                assert_eq!(actual, Some(2..3));
                input.replace_text_in_range(None, "", window, input_cx);
                assert_eq!(input.value(), "ab");
                assert_eq!(input.marked, None);
            });
        });
    }
}
