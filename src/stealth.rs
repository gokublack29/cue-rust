use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    SmartAssist,        // Ctrl + Shift + Enter
    SolveCode,          // Ctrl + H
    FixCode,            // Ctrl + Shift + F
    ToggleClickThrough, // Ctrl + Shift + C
    ToggleVisibility,   // Ctrl + / or Ctrl + \
    RegistrationWarning(&'static str),
}

static SHARED_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[cfg(windows)]
pub fn stored_hwnd() -> *mut std::ffi::c_void {
    SHARED_HWND.load(std::sync::atomic::Ordering::SeqCst) as *mut std::ffi::c_void
}

#[cfg(not(windows))]
pub fn stored_hwnd() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[cfg(windows)]
pub fn enable_stealth(hwnd: *mut std::ffi::c_void) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, GetWindowLongW, SetWindowDisplayAffinity, SetWindowLongW,
        SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        WINDOW_DISPLAY_AFFINITY, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    if hwnd.is_null() {
        return Err("HWND pointer is null".to_string());
    }

    SHARED_HWND.store(hwnd as isize, std::sync::atomic::Ordering::SeqCst);

    unsafe {
        let hwnd_val = HWND(hwnd as _);

        // A tool window stays visible but leaves the taskbar and Alt+Tab.
        let current = GetWindowLongW(hwnd_val, GWL_EXSTYLE);
        let wanted = (current | (WS_EX_TOOLWINDOW.0 as i32)) & !(WS_EX_APPWINDOW.0 as i32);
        if wanted != current {
            SetWindowLongW(hwnd_val, GWL_EXSTYLE, wanted);
            let _ = SetWindowPos(
                hwnd_val,
                HWND(std::ptr::null_mut()),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }

        // 0x00000011 is WDA_EXCLUDEFROMCAPTURE
        let mut affinity = 0u32;
        let _ = GetWindowDisplayAffinity(hwnd_val, &mut affinity);
        if affinity != 0x00000011 {
            SetWindowDisplayAffinity(hwnd_val, WINDOW_DISPLAY_AFFINITY(0x00000011))
                .map_err(|e| format!("Failed to set window display affinity: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn set_window_opacity(hwnd: *mut std::ffi::c_void, alpha: u8) -> Result<(), String> {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, GWL_EXSTYLE,
        LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_LAYERED,
    };

    if hwnd.is_null() {
        return Err("HWND pointer is null".to_string());
    }

    unsafe {
        let hwnd_val = HWND(hwnd as _);
        let current_style = GetWindowLongW(hwnd_val, GWL_EXSTYLE);
        SetWindowLongW(
            hwnd_val,
            GWL_EXSTYLE,
            current_style | (WS_EX_LAYERED.0 as i32),
        );
        SetLayeredWindowAttributes(hwnd_val, COLORREF(0), alpha, LWA_ALPHA)
            .map_err(|e| format!("SetLayeredWindowAttributes failed: {e}"))?;
        let _ = SetWindowPos(
            hwnd_val,
            HWND(std::ptr::null_mut()),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
    Ok(())
}

#[cfg(windows)]
pub fn set_click_through(hwnd: *mut std::ffi::c_void, enable: bool) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };

    if hwnd.is_null() {
        return Err("HWND pointer is null".to_string());
    }

    unsafe {
        let hwnd_val = HWND(hwnd as _);
        let current_style = GetWindowLongW(hwnd_val, GWL_EXSTYLE);
        let new_style = if enable {
            current_style | (WS_EX_TRANSPARENT.0 as i32) | (WS_EX_LAYERED.0 as i32)
        } else {
            current_style & !(WS_EX_TRANSPARENT.0 as i32)
        };
        SetWindowLongW(hwnd_val, GWL_EXSTYLE, new_style);
        let _ = SetWindowPos(
            hwnd_val,
            HWND(std::ptr::null_mut()),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
    Ok(())
}

#[cfg(windows)]
pub fn toggle_window_visibility(hwnd: *mut std::ffi::c_void) -> Result<bool, String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, HWND_TOPMOST,
        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW,
    };

    let target_hwnd = if !hwnd.is_null() {
        hwnd as isize
    } else {
        let stored = SHARED_HWND.load(std::sync::atomic::Ordering::SeqCst);
        if stored != 0 {
            stored
        } else {
            let title = HSTRING::from("Cue - AI Stealth Assistant");
            if let Ok(found) = unsafe { FindWindowW(None, &title) } {
                if !found.is_invalid() {
                    SHARED_HWND.store(found.0 as isize, std::sync::atomic::Ordering::SeqCst);
                    found.0 as isize
                } else {
                    0
                }
            } else {
                0
            }
        }
    };

    if target_hwnd == 0 {
        return Err("HWND pointer is null and window not found".to_string());
    }

    unsafe {
        let hwnd_val = HWND(target_hwnd as _);
        let is_visible = IsWindowVisible(hwnd_val).as_bool();
        if is_visible {
            let _ = ShowWindow(hwnd_val, SW_HIDE);
            Ok(false)
        } else {
            let _ = ShowWindow(hwnd_val, SW_SHOW);
            let _ = SetWindowPos(
                hwnd_val,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd_val);
            Ok(true)
        }
    }
}

#[cfg(windows)]
pub fn ensure_window_visible_no_activate() {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE,
    };

    let stored = SHARED_HWND.load(std::sync::atomic::Ordering::SeqCst);
    if stored != 0 {
        unsafe {
            let hwnd_val = HWND(stored as _);
            if !IsWindowVisible(hwnd_val).as_bool() {
                let _ = ShowWindow(hwnd_val, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    hwnd_val,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
                );
            }
        }
    }
}

#[cfg(windows)]
pub fn start_hotkey_listener(
    tx: UnboundedSender<HotkeyAction>,
    egui_ctx: egui::Context,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_CONTROL, MOD_SHIFT};
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    std::thread::spawn(move || {
        unsafe {
            let bindings = [
                (1i32, MOD_CONTROL | MOD_SHIFT, 0x0Du32, "Ctrl+Shift+Enter"),
                (2, MOD_CONTROL, 0x48, "Ctrl+H"),
                (3, MOD_CONTROL | MOD_SHIFT, 0x43, "Ctrl+Shift+C"),
                (4, MOD_CONTROL, 0xBF, "Ctrl+/"),
                (5, MOD_CONTROL, 0xDC, "Ctrl+\\"),
                (6, MOD_CONTROL | MOD_SHIFT, 0x46, "Ctrl+Shift+F"),
            ];

            for (id, mods, vk, label) in bindings {
                if RegisterHotKey(HWND(std::ptr::null_mut()), id, mods, vk).is_err() {
                    let _ = tx.send(HotkeyAction::RegistrationWarning(label));
                    egui_ctx.request_repaint();
                }
            }

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
                if msg.message == WM_HOTKEY {
                    match msg.wParam.0 {
                        1 => {
                            ensure_window_visible_no_activate();
                            let _ = tx.send(HotkeyAction::SmartAssist);
                        }
                        2 => {
                            ensure_window_visible_no_activate();
                            let _ = tx.send(HotkeyAction::SolveCode);
                        }
                        3 => {
                            let _ = tx.send(HotkeyAction::ToggleClickThrough);
                        }
                        4 | 5 => {
                            // Directly toggle Win32 window visibility in the message pump thread
                            // (avoids Winit event loop starvation when window is hidden)
                            let _ = toggle_window_visibility(std::ptr::null_mut());
                        }
                        6 => {
                            ensure_window_visible_no_activate();
                            let _ = tx.send(HotkeyAction::FixCode);
                        }
                        _ => {}
                    }
                    // CRITICAL: Force egui event loop to wake up immediately even in click-through mode!
                    egui_ctx.request_repaint();
                }
            }
        }
    });

    Ok(())
}

#[cfg(not(windows))]
pub fn enable_stealth(_hwnd: *mut std::ffi::c_void) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn set_window_opacity(_hwnd: *mut std::ffi::c_void, _alpha: u8) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn set_click_through(_hwnd: *mut std::ffi::c_void, _enable: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn toggle_window_visibility(_hwnd: *mut std::ffi::c_void) -> Result<bool, String> {
    Ok(true)
}

#[cfg(not(windows))]
pub fn start_hotkey_listener(
    _tx: UnboundedSender<HotkeyAction>,
    _egui_ctx: egui::Context,
) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enable_stealth_null_hwnd() {
        assert!(enable_stealth(std::ptr::null_mut()).is_err());
    }

    #[test]
    fn test_set_click_through_null_hwnd() {
        assert!(set_click_through(std::ptr::null_mut(), true).is_err());
    }

    #[test]
    fn test_hotkey_action_enum() {
        assert_ne!(HotkeyAction::SmartAssist, HotkeyAction::SolveCode);
        assert_ne!(HotkeyAction::SmartAssist, HotkeyAction::ToggleClickThrough);
    }
}
