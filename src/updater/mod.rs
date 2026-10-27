//! Application-wide update preparation and explicit, guarded installation.

mod download;
pub(crate) mod install;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::*;

use crate::i18n::I18nManager;
use crate::net::update::{UpdateCheckResult, UpdateVersionInfo, check_latest_version};
use download::PreparedUpdate;

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One shared state machine, independent of how many editor windows are open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdatePhase {
    Idle,
    Checking,
    Downloading(String),
    Ready(String),
    Installing(String),
}

impl UpdatePhase {
    pub(crate) fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading(_) | Self::Installing(_)
        )
    }
}

struct UpdateService(Entity<UpdateManager>);
impl Global for UpdateService {}

/// Owns the prepared archive and its lifetime; UI consumers only read the phase.
pub(crate) struct UpdateManager {
    phase: UpdatePhase,
    automatic: Arc<AtomicBool>,
    manual: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    manual_windows: Vec<AnyWindowHandle>,
    ready: Option<PreparedUpdate>,
    handoff: Option<install::Handoff>,
    startup_error: Option<String>,
}

impl Drop for UpdateManager {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

enum WorkerMessage {
    Downloading(String),
    Finished(anyhow::Result<WorkerOutcome>),
}

enum WorkerOutcome {
    UpToDate(UpdateVersionInfo),
    Disabled,
    Ready(PreparedUpdate),
}

fn manager(cx: &App) -> Option<Entity<UpdateManager>> {
    cx.try_global::<UpdateService>()
        .map(|service| service.0.clone())
}

/// Starts the single startup check even when automatic downloading is disabled.
pub(crate) fn init(cx: &mut App, auto_download: bool, startup_error: Option<String>) {
    let entity = cx.new(|_| UpdateManager::new(auto_download, startup_error));
    cx.set_global(UpdateService(entity.clone()));
    entity.update(cx, |manager, cx| manager.start(cx));
}

pub(crate) fn phase(cx: &App) -> UpdatePhase {
    manager(cx)
        .map(|manager| manager.read(cx).phase.clone())
        .unwrap_or(UpdatePhase::Idle)
}

pub(crate) fn ready_version(cx: &App) -> Option<String> {
    match phase(cx) {
        UpdatePhase::Ready(version) => Some(version),
        _ => None,
    }
}

/// Applies saved preferences immediately while preserving user-requested downloads.
pub(crate) fn set_auto_download(cx: &mut App, enabled: bool) {
    if let Some(manager) = manager(cx) {
        manager.update(cx, |manager, cx| {
            let previous = manager.automatic.swap(enabled, Ordering::Relaxed);
            if !enabled
                && !manager.manual.load(Ordering::Relaxed)
                && matches!(
                    manager.phase,
                    UpdatePhase::Checking | UpdatePhase::Downloading(_)
                )
            {
                manager.cancelled.store(true, Ordering::Relaxed);
            }
            if enabled
                && matches!(
                    manager.phase,
                    UpdatePhase::Checking | UpdatePhase::Downloading(_)
                )
            {
                manager.cancelled.store(false, Ordering::Relaxed);
            }
            if enabled && !previous && manager.phase == UpdatePhase::Idle {
                manager.start(cx);
            }
        });
    }
}

/// Attaches a manual caller to the existing worker instead of creating a duplicate.
pub(crate) fn request_manual(window: &mut Window, cx: &mut App) {
    let Some(manager) = manager(cx) else {
        return;
    };
    let handle = window.window_handle();
    if let Some(version) = ready_version(cx) {
        show_ready_prompt(window, cx, &version);
        return;
    }
    manager.update(cx, |manager, cx| {
        if matches!(manager.phase, UpdatePhase::Installing(_)) {
            return;
        }
        if !manager
            .manual_windows
            .iter()
            .any(|item| item.window_id() == handle.window_id())
        {
            manager.manual_windows.push(handle);
        }
        manager.manual.store(true, Ordering::Relaxed);
        manager.cancelled.store(false, Ordering::Relaxed);
        if manager.phase == UpdatePhase::Idle {
            manager.start(cx);
        }
    });
}

impl UpdateManager {
    fn new(auto_download: bool, startup_error: Option<String>) -> Self {
        Self {
            phase: UpdatePhase::Idle,
            automatic: Arc::new(AtomicBool::new(auto_download)),
            manual: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            manual_windows: Vec::new(),
            ready: None,
            handoff: None,
            startup_error,
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.phase != UpdatePhase::Idle {
            return;
        }
        self.cancelled = Arc::new(AtomicBool::new(false));
        self.manual
            .store(!self.manual_windows.is_empty(), Ordering::Relaxed);
        self.phase = UpdatePhase::Checking;
        cx.refresh_windows();
        let automatic = self.automatic.clone();
        let manual = self.manual.clone();
        let cancelled = self.cancelled.clone();
        let (sender, mut receiver) = mpsc::unbounded();
        let worker = std::thread::Builder::new()
            .name("velotype-update".into())
            .spawn(move || {
                download::cleanup_stale();
                let result = (|| -> anyhow::Result<WorkerOutcome> {
                    match check_latest_version(env!("CARGO_PKG_VERSION"))? {
                        UpdateCheckResult::UpToDate(info) => Ok(WorkerOutcome::UpToDate(info)),
                        UpdateCheckResult::UpdateAvailable(info) => {
                            if !automatic.load(Ordering::Relaxed) && !manual.load(Ordering::Relaxed)
                            {
                                return Ok(WorkerOutcome::Disabled);
                            }
                            let _ = sender.unbounded_send(WorkerMessage::Downloading(
                                info.latest_version.clone(),
                            ));
                            Ok(WorkerOutcome::Ready(download::prepare(
                                &info.latest_version,
                                &cancelled,
                            )?))
                        }
                    }
                })();
                let _ = sender.unbounded_send(WorkerMessage::Finished(result));
            });
        if let Err(error) = worker {
            self.finish(Err(error.into()), cx);
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Some(message) = receiver.next().await {
                let finished = matches!(message, WorkerMessage::Finished(_));
                if this
                    .update(cx, |manager, cx| match message {
                        WorkerMessage::Downloading(version) => {
                            manager.phase = UpdatePhase::Downloading(version);
                            cx.refresh_windows();
                        }
                        WorkerMessage::Finished(result) => manager.finish(result, cx),
                    })
                    .is_err()
                {
                    return;
                }
                if finished {
                    return;
                }
            }
            let _ = this.update(cx, |manager, cx| {
                if manager.phase.is_busy() {
                    manager.finish(
                        Err(anyhow::anyhow!("update worker stopped unexpectedly")),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    fn finish(&mut self, result: anyhow::Result<WorkerOutcome>, cx: &mut Context<Self>) {
        let result = if self.cancelled.load(Ordering::Relaxed) && self.manual_windows.is_empty() {
            Ok(WorkerOutcome::Disabled)
        } else {
            result
        };
        let result = match result {
            Ok(WorkerOutcome::Ready(prepared)) => {
                let version = prepared.version.clone();
                self.ready = Some(prepared);
                self.phase = UpdatePhase::Ready(version.clone());
                Ok(Some(version))
            }
            Ok(WorkerOutcome::UpToDate(info)) => {
                self.phase = UpdatePhase::Idle;
                Ok(Some(info.latest_version))
            }
            Ok(WorkerOutcome::Disabled) => {
                self.phase = UpdatePhase::Idle;
                Ok(None)
            }
            Err(error) => {
                self.phase = UpdatePhase::Idle;
                Err(format!("{error:#}"))
            }
        };
        let is_ready = matches!(self.phase, UpdatePhase::Ready(_));
        let windows = std::mem::take(&mut self.manual_windows);
        self.manual.store(false, Ordering::Relaxed);
        cx.refresh_windows();
        cx.defer(move |cx| {
            for window in windows {
                let _ = window.update(cx, |_, window, cx| match &result {
                    Ok(Some(version)) if is_ready => show_ready_prompt(window, cx, version),
                    Ok(Some(version)) => show_up_to_date(window, cx, version),
                    Ok(None) => {}
                    Err(detail) => show_error(window, cx, detail),
                });
            }
        });
    }
}

/// Requires saved documents in every editor before launching the installer.
pub(crate) fn request_install(window: &mut Window, cx: &mut App) {
    window.defer(cx, begin_install);
}

fn begin_install(window: &mut Window, cx: &mut App) {
    let Some(manager) = manager(cx) else {
        return;
    };
    if !matches!(manager.read(cx).phase, UpdatePhase::Ready(_)) {
        return;
    }
    // The invoking window is already borrowed; check its editor through the root entity.
    if let Some(editor) = window.root::<crate::editor::Editor>().flatten() {
        if !editor.update(cx, |editor, cx| editor.allow_update_restart(window, cx)) {
            return;
        }
    }
    if !crate::app_menu::editors_allow_update_except(Some(window.window_handle()), cx) {
        return;
    }
    let paths = restart_file_paths(window, cx);
    let restart_snapshot = paths.clone();
    let prepared = manager.update(cx, |manager, cx| {
        let prepared = manager.ready.take()?;
        manager.phase = UpdatePhase::Installing(prepared.version.clone());
        cx.refresh_windows();
        Some(prepared)
    });
    let Some(prepared) = prepared else {
        return;
    };
    let handle = window.window_handle();
    let weak = manager.downgrade();
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let result = install::launch(&prepared, paths);
        let _ = sender.send((prepared, result));
    });
    cx.spawn(async move |cx| {
        let Ok((prepared, result)) = receiver.await else {
            return;
        };
        let target_window = cx.update(|cx| {
            let windows = cx.windows();
            windows
                .iter()
                .find(|window| window.window_id() == handle.window_id())
                .copied()
                .or_else(|| {
                    windows
                        .into_iter()
                        .find(|window| window.downcast::<crate::editor::Editor>().is_some())
                })
        });
        let Some(target_window) = target_window else {
            let _ = weak.update(cx, |manager, cx| manager.restore_ready(prepared, cx));
            return;
        };
        let _ = cx.update_window(target_window, |_, window, cx| {
            let Some(manager) = weak.upgrade() else {
                return;
            };
            let allow = if let Some(editor) = window.root::<crate::editor::Editor>().flatten() {
                editor.update(cx, |editor, cx| editor.allow_update_restart(window, cx))
            } else {
                true
            } && crate::app_menu::editors_allow_update_except(Some(target_window), cx)
                // Retry if the user opened, closed, or renamed files during helper preparation.
                && restart_file_paths(window, cx) == restart_snapshot;
            match result {
                Ok(mut handoff) if allow => {
                    if let Err(error) = handoff.authorize() {
                        manager.update(cx, |manager, cx| manager.restore_ready(prepared, cx));
                        show_error(window, cx, &error.to_string());
                        return;
                    }
                    // The helper now owns cleanup; process exit must not delete its input.
                    let _ = prepared.directory.keep();
                    manager.update(cx, |manager, _| manager.handoff = Some(handoff));
                    cx.quit();
                }
                Ok(_) => manager.update(cx, |manager, cx| manager.restore_ready(prepared, cx)),
                Err(error) => {
                    manager.update(cx, |manager, cx| manager.restore_ready(prepared, cx));
                    show_error(window, cx, &format!("{error:#}"));
                }
            }
        });
    })
    .detach();
}

/// Collects saved files without borrowing the window that is already being updated.
fn restart_file_paths(window: &mut Window, cx: &mut App) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(editor) = window.root::<crate::editor::Editor>().flatten() {
        if let Some(path) = editor.read(cx).restart_file_path() {
            paths.push(path);
        }
    }
    for handle in cx.windows() {
        if handle.window_id() == window.window_handle().window_id() {
            continue;
        }
        if let Some(handle) = handle.downcast::<crate::editor::Editor>() {
            if let Ok(Some(path)) = handle.update(cx, |editor, _, _| editor.restart_file_path()) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

impl UpdateManager {
    fn restore_ready(&mut self, prepared: PreparedUpdate, cx: &mut Context<Self>) {
        self.phase = UpdatePhase::Ready(prepared.version.clone());
        self.ready = Some(prepared);
        cx.refresh_windows();
    }
}

pub(crate) fn show_startup_error(window: &mut Window, cx: &mut App) {
    if let Some(manager) = manager(cx) {
        if let Some(detail) = manager.update(cx, |manager, _| manager.startup_error.take()) {
            show_error(window, cx, &detail);
        }
    }
}

fn show_error(window: &mut Window, cx: &mut App, detail: &str) {
    let strings = cx.global::<I18nManager>().strings_arc();
    let message = strings
        .update_failed_message_template
        .replace("{error}", detail);
    let _ = window.prompt(
        PromptLevel::Critical,
        &strings.update_failed_title,
        Some(&message),
        &[strings.info_dialog_ok.as_str()],
        cx,
    );
}

fn show_up_to_date(window: &mut Window, cx: &mut App, version: &str) {
    let strings = cx.global::<I18nManager>().strings_arc();
    let message = strings
        .update_up_to_date_message_template
        .replace("{current}", env!("CARGO_PKG_VERSION"))
        .replace("{latest}", version);
    let _ = window.prompt(
        PromptLevel::Info,
        &strings.update_up_to_date_title,
        Some(&message),
        &[strings.info_dialog_ok.as_str()],
        cx,
    );
}

fn show_ready_prompt(window: &mut Window, cx: &mut App, version: &str) {
    let strings = cx.global::<I18nManager>().strings_arc();
    let message = strings.update_ready_message.replace("{version}", version);
    let prompt = window.prompt(
        PromptLevel::Info,
        &strings.update_available_title,
        Some(&message),
        &[
            strings.update_restart.as_str(),
            strings.update_later.as_str(),
        ],
        cx,
    );
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        if prompt.await == Ok(0) {
            let _ = cx.update_window(handle, |_, window, cx| request_install(window, cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedUpdate, UpdateManager, UpdatePhase, UpdateService, install, request_install,
        request_manual, set_auto_download,
    };
    use crate::i18n::I18nManager;
    use gpui::{AppContext, TestAppContext};
    use std::sync::atomic::Ordering;

    fn init_test_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            I18nManager::init_with_language_id(cx, "en-US");
            crate::theme::ThemeManager::init(cx);
            crate::components::init(cx);
        });
    }

    #[gpui::test]
    async fn updater_manual_checks_reuse_one_shared_task(cx: &mut TestAppContext) {
        init_test_app(cx);
        let manager = cx.new(|_| {
            let mut manager = UpdateManager::new(false, None);
            manager.phase = UpdatePhase::Checking;
            manager
        });
        cx.update(|cx| cx.set_global(UpdateService(manager.clone())));
        let (_, cx) = cx
            .add_window_view(|_, cx| crate::editor::Editor::from_markdown(cx, String::new(), None));
        cx.update(|window, cx| {
            request_manual(window, cx);
            request_manual(window, cx);
        });
        manager.read_with(cx, |manager, _| {
            assert_eq!(manager.phase, UpdatePhase::Checking);
            assert_eq!(manager.manual_windows.len(), 1);
            assert!(manager.manual.load(Ordering::Relaxed));
        });
        manager.update(cx, |manager, _| {
            manager.phase = UpdatePhase::Downloading("1.2.3".into())
        });
        cx.update(|_, cx| set_auto_download(cx, false));
        manager.read_with(cx, |manager, _| {
            assert!(!manager.cancelled.load(Ordering::Relaxed))
        });
        manager.update(cx, |manager, _| {
            manager.manual.store(false, Ordering::Relaxed);
            manager.manual_windows.clear();
        });
        cx.update(|_, cx| set_auto_download(cx, false));
        manager.read_with(cx, |manager, _| {
            assert!(manager.cancelled.load(Ordering::Relaxed))
        });
    }

    #[gpui::test]
    async fn updater_cancelled_background_result_cannot_restore_a_ready_update(
        cx: &mut TestAppContext,
    ) {
        let entity = cx.new(|_| {
            let manager = UpdateManager::new(false, None);
            manager.cancelled.store(true, Ordering::Relaxed);
            manager
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let prepared = PreparedUpdate {
            version: "1.2.3".into(),
            _lease: tempfile::tempfile().unwrap(),
            directory,
            payload: path.join("payload"),
            target: install::InstallTarget::current().unwrap(),
        };
        entity.update(cx, |manager, cx| {
            manager.finish(Ok(super::WorkerOutcome::Ready(prepared)), cx)
        });
        entity.read_with(cx, |manager, _| {
            assert_eq!(manager.phase, UpdatePhase::Idle);
            assert!(manager.ready.is_none());
        });
        assert!(!path.exists());
    }

    #[gpui::test]
    async fn updater_restart_snapshot_tracks_files_independently_of_active_window(
        cx: &mut TestAppContext,
    ) {
        init_test_app(cx);
        let (_, cx) = cx.add_window_view(|_, cx| {
            crate::editor::Editor::from_markdown(cx, String::new(), Some("first.md".into()))
        });
        let first = cx.update(|window, _| window.window_handle());
        let (_, cx) = cx.cx.add_window_view(|_, cx| {
            crate::editor::Editor::from_markdown(cx, String::new(), Some("second.md".into()))
        });
        let snapshot = cx.update(super::restart_file_paths);
        let from_first = first
            .update(cx, |_, window, cx| super::restart_file_paths(window, cx))
            .unwrap();
        assert_eq!(snapshot, from_first);
        let (_, cx) = cx.cx.add_window_view(|_, cx| {
            crate::editor::Editor::from_markdown(cx, String::new(), Some("third.md".into()))
        });
        let changed = cx.update(super::restart_file_paths);
        assert_ne!(snapshot, changed);
        assert_eq!(changed.len(), 3);
    }

    #[gpui::test]
    async fn updater_does_not_launch_helper_with_a_dirty_second_window(cx: &mut TestAppContext) {
        init_test_app(cx);
        let directory = tempfile::Builder::new()
            .prefix("velotype-update-")
            .tempdir()
            .unwrap();
        let job_path = directory.path().join("job.json");
        let manager = cx.new(|_| {
            let mut manager = UpdateManager::new(false, None);
            manager.phase = UpdatePhase::Ready("1.2.3".into());
            manager.ready = Some(PreparedUpdate {
                version: "1.2.3".into(),
                payload: directory.path().join("payload"),
                directory,
                _lease: tempfile::tempfile().unwrap(),
                target: install::InstallTarget::current().unwrap(),
            });
            manager
        });
        cx.update(|cx| cx.set_global(UpdateService(manager.clone())));
        let (_, cx) = cx
            .add_window_view(|_, cx| crate::editor::Editor::from_markdown(cx, String::new(), None));
        let first = cx.update(|window, _| window.window_handle());
        let (_, cx) = cx
            .cx
            .add_window_view(|_, cx| crate::editor::Editor::from_markdown(cx, String::new(), None));
        cx.simulate_input("unsaved text");
        first
            .update(cx, |_, window, cx| request_install(window, cx))
            .unwrap();
        cx.run_until_parked();
        manager.read_with(cx, |manager, _| {
            assert_eq!(manager.phase, UpdatePhase::Ready("1.2.3".into()))
        });
        assert!(!job_path.exists());
    }
}
