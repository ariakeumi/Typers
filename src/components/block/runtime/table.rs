//! Native table cell and axis runtime state.

use super::*;

impl Block {
    /// Returns whether this block edits a native table cell.
    pub(crate) fn is_table_cell(&self) -> bool {
        self.table_cell_position.is_some()
    }

    /// Returns this cell's row and column position, if applicable.
    pub(crate) fn table_cell_position(&self) -> Option<TableCellPosition> {
        self.table_cell_position
    }

    /// Returns the alignment configured for this cell's column.
    pub(crate) fn table_cell_alignment(&self) -> Option<TableColumnAlignment> {
        self.table_cell_alignment
    }

    /// Maps the table column alignment to GPUI text alignment.
    pub(crate) fn text_align(&self) -> TextAlign {
        match self
            .table_cell_alignment()
            .unwrap_or(TableColumnAlignment::Default)
        {
            TableColumnAlignment::Default | TableColumnAlignment::Left => TextAlign::Left,
            TableColumnAlignment::Center => TextAlign::Center,
            TableColumnAlignment::Right => TextAlign::Right,
        }
    }

    /// Configures cell position and alignment, then refreshes rendered editing.
    pub(crate) fn set_table_cell_mode(
        &mut self,
        position: TableCellPosition,
        alignment: TableColumnAlignment,
    ) {
        self.table_cell_position = Some(position);
        self.table_cell_alignment = Some(alignment);
        self.edit_mode = EditMode::RenderedRich;
        self.clear_inline_projection();
        self.sync_render_cache();
    }

    /// Attaches the native table render data to its owning block.
    pub(crate) fn set_table_runtime(&mut self, runtime: TableRuntime) {
        self.table_runtime = Some(runtime);
    }

    /// Removes table render data and all transient axis and append controls.
    pub(crate) fn clear_table_runtime(&mut self) {
        self.table_runtime = None;
        self.table_axis_preview = None;
        self.table_axis_selection = None;
        self.table_axis_highlight = TableAxisHighlight::None;
        self.table_append_column_edge_hovered = false;
        self.table_append_column_hovered = false;
        self.table_append_column_zone_hovered = false;
        self.table_append_column_button_hovered = false;
        self.table_append_column_close_task = None;
        self.table_append_row_edge_hovered = false;
        self.table_append_row_hovered = false;
        self.table_append_row_zone_hovered = false;
        self.table_append_row_button_hovered = false;
        self.table_append_row_close_task = None;
    }

    /// Sets the hovered and selected row or column markers.
    pub(crate) fn set_table_axis_visual_state(
        &mut self,
        preview: Option<TableAxisMarker>,
        selection: Option<TableAxisMarker>,
    ) {
        self.table_axis_preview = preview;
        self.table_axis_selection = selection;
    }

    /// Sets the row or column highlight shown over the native table.
    pub(crate) fn set_table_axis_highlight(&mut self, highlight: TableAxisHighlight) {
        self.table_axis_highlight = highlight;
    }
}
