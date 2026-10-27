//! AppKit menu integration missing from GPUI's localized menu support.

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

/// Registers the localized Window menu so AppKit supplies its window commands.
pub(super) fn register_window_menu(title: &str) {
    // GPUI 0.3.9 only calls setWindowsMenu: for a menu literally named
    // "Window". Register after every set_menus call, which replaces NSMenu
    // objects on language/theme/file changes and while writing state changes.
    // AppKit owns the tiling commands, shortcuts, and list of open windows.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(main_menu) = app.mainMenu() else {
        return;
    };
    for index in 0..main_menu.numberOfItems() {
        let Some(item) = main_menu.itemAtIndex(index) else {
            continue;
        };
        let Some(menu) = item.submenu() else {
            continue;
        };
        if menu.title().to_string() == title {
            app.setWindowsMenu(Some(&menu));
            return;
        }
    }
}
