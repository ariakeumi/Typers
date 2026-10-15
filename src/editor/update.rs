//! Editor entry point for the application-wide updater.

use gpui::*;

use super::{Editor, InfoDialogKind};

impl Editor {
    /// Attaches the invoking window to the shared check/download task.
    pub(crate) fn request_check_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_unsaved_changes_dialog {
            return;
        }
        crate::updater::request_manual(window, cx);
        if crate::updater::phase(cx).is_busy() {
            self.show_info_dialog(InfoDialogKind::CheckForUpdates, cx);
        }
    }
}
