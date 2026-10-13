//! Native IME compatibility that cannot be expressed by GPUI 0.2 key events.
//!
//! Windows marks keys consumed by an IME as VK_PROCESSKEY. GPUI 0.2 unwraps
//! these back into ordinary shortcuts, unlike Zed's fix in 2ead8c42fb67. Filter
//! that native message before GPUI sees it; do not use time-based key suppression.

/// Installs the Windows IME key filter once per native window; other OSes need none.
pub(crate) fn install_window_input_filter(window: &gpui::Window) {
    #[cfg(all(target_os = "windows", not(test)))]
    if let Err(error) = windows::install(window) {
        eprintln!("failed to install IME input filter: {error:#}");
    }
    #[cfg(any(not(target_os = "windows"), test))]
    let _ = window;
}

#[cfg(target_os = "windows")]
mod windows {
    #[cfg(not(test))]
    use anyhow::{anyhow, bail};
    #[cfg(not(test))]
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[cfg(test)]
    use windows_sys::Win32::Foundation::WPARAM;
    #[cfg(not(test))]
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_PROCESSKEY;
    #[cfg(not(test))]
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    #[cfg(not(test))]
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, MSG, TranslateMessage};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_KEYDOWN, WM_KEYUP, WM_NCDESTROY, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    /// Stable subclass identity; reinstallation updates rather than stacking hooks.
    #[cfg(not(test))]
    const IME_SUBCLASS_ID: usize = 0x564c5449;

    /// Attaches the stateless filter on the GPUI thread that owns the native HWND.
    #[cfg(not(test))]
    pub(super) fn install(window: &gpui::Window) -> anyhow::Result<()> {
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow!("native window handle unavailable: {error:?}"))?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            bail!("expected a Win32 window handle");
        };
        // SAFETY: GPUI owns this HWND on the current UI thread. The callback is
        // static and stores no Rust references; WM_NCDESTROY removes it.
        let installed = unsafe {
            SetWindowSubclass(handle.hwnd.get() as HWND, Some(filter), IME_SUBCLASS_ID, 0)
        };
        if installed == 0 {
            bail!("SetWindowSubclass failed");
        }
        Ok(())
    }

    /// Identifies only native IME-owned keys, independent of composition timing.
    fn ime_owns_key(message: u32, key: WPARAM) -> bool {
        key == VK_PROCESSKEY as usize
            && matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
    }

    /// Keeps IME-owned keys out of GPUI's keymap and forwards all other messages.
    #[cfg(not(test))]
    unsafe extern "system" fn filter(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        subclass: usize,
        _: usize,
    ) -> LRESULT {
        if ime_owns_key(message, wparam) {
            if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let native_message = MSG {
                    hwnd,
                    message,
                    wParam: wparam,
                    lParam: lparam,
                    ..Default::default()
                };
                // SAFETY: TranslateMessage receives the current, valid native
                // key message. GPUI 0.2 normally translates inside its handler,
                // which we intentionally bypass only for VK_PROCESSKEY.
                unsafe { TranslateMessage(&native_message) };
            }
            // SAFETY: Preserve Windows' default processing without entering
            // GPUI's ImmGetVirtualKey/keybinding path for this IME-owned key.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        if message == WM_NCDESTROY {
            // SAFETY: Remove this exact callback/id while the HWND still exists.
            unsafe { RemoveWindowSubclass(hwnd, Some(filter), subclass) };
        }
        // SAFETY: Forward the unchanged message through the remaining subclass
        // chain, including GPUI, for normal keyboard, mouse and window behavior.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ime_owned_keys_never_become_editor_actions() {
            for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
                assert!(ime_owns_key(message, VK_PROCESSKEY as usize));
                for key in [0x08, 0x0d, 0x20, 0x41] {
                    assert!(!ime_owns_key(message, key));
                }
            }
            assert!(!ime_owns_key(WM_NCDESTROY, VK_PROCESSKEY as usize));
        }
    }
}
