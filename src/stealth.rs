use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    SmartAssist,        // Ctrl + Shift + Enter
    SolveCode,          // Ctrl + H
    FixCode,            // Ctrl + Shift + F
    ToggleClickThrough, // Ctrl + Shift + C
    ToggleVisibility,   // Ctrl + / or Ctrl + \
}

static SHARED_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[cfg(windows)]
pub fn enable_stealth(hwnd: *mut std::ffi::c_void) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WINDOW_DISPLAY_AFFINITY,
    };

    if hwnd.is_null() {
        return Err("HWND pointer is null".to_string());
    }

    SHARED_HWND.store(hwnd as isize, std::sync::atomic::Ordering::SeqCst);

    // 0x00000011 is WDA_EXCLUDEFROMCAPTURE
    let affinity = WINDOW_DISPLAY_AFFINITY(0x00000011);
    unsafe {
        SetWindowDisplayAffinity(HWND(hwnd as _), affinity)
            .map_err(|e| format!("Failed to set window display affinity: {e}"))?;
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
        SetWindowLongW(hwnd_val, GWL_EXSTYLE, current_style | (WS_EX_LAYERED.0 as i32));
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
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow,
        HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW,
    };
    use windows::core::HSTRING;

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
pub fn start_hotkey_listener(
    tx: UnboundedSender<HotkeyAction>,
    egui_ctx: egui::Context,
) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_CONTROL, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, WM_HOTKEY,
    };

    std::thread::spawn(move || {
        unsafe {
            // Hotkey 1: Ctrl + Shift + Enter (0x0D = VK_RETURN)
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                1,
                MOD_CONTROL | MOD_SHIFT,
                0x0D,
            ) {
                eprintln!("Failed to register hotkey 1 (Ctrl+Shift+Enter): {e}");
            }

            // Hotkey 2: Ctrl + H (0x48 = 'H')
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                2,
                MOD_CONTROL,
                0x48,
            ) {
                eprintln!("Failed to register hotkey 2 (Ctrl+H): {e}");
            }

            // Hotkey 3: Ctrl + Shift + C (0x43 = 'C') - Toggle Click-Through
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                3,
                MOD_CONTROL | MOD_SHIFT,
                0x43,
            ) {
                eprintln!("Failed to register hotkey 3 (Ctrl+Shift+C): {e}");
            }

            // Hotkey 4: Ctrl + / (0xBF = VK_OEM_2) - Primary Toggle Visibility (Hide/Show)
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                4,
                MOD_CONTROL,
                0xBF,
            ) {
                eprintln!("Failed to register hotkey 4 (Ctrl+/): {e}");
            }

            // Hotkey 5: Ctrl + \ (0xDC = VK_OEM_5) - Alternate Toggle Visibility
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                5,
                MOD_CONTROL,
                0xDC,
            ) {
                eprintln!("Failed to register hotkey 5 (Ctrl+\\): {e}");
            }

            // Hotkey 6: Ctrl + Shift + F (0x46 = 'F') - Fix Failed Test Case / Debug Error
            if let Err(e) = RegisterHotKey(
                HWND(std::ptr::null_mut()),
                6,
                MOD_CONTROL | MOD_SHIFT,
                0x46,
            ) {
                eprintln!("Failed to register hotkey 6 (Ctrl+Shift+F): {e}");
            }

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
                if msg.message == WM_HOTKEY {
                    match msg.wParam.0 {
                        1 => { let _ = tx.send(HotkeyAction::SmartAssist); }
                        2 => { let _ = tx.send(HotkeyAction::SolveCode); }
                        3 => { let _ = tx.send(HotkeyAction::ToggleClickThrough); }
                        4 | 5 => {
                            // Directly toggle Win32 window visibility in the message pump thread
                            // (avoids Winit event loop starvation when window is hidden)
                            let _ = toggle_window_visibility(std::ptr::null_mut());
                        }
                        6 => { let _ = tx.send(HotkeyAction::FixCode); }
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
