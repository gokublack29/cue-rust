#![windows_subsystem = "windows"]

pub mod groq;
pub mod screen;
pub mod stealth;
pub mod ui;

use eframe::egui;
use std::sync::Arc;
use tokio::sync::mpsc;

fn main() -> eframe::Result {
    if !stealth::acquire_single_instance() {
        return Ok(());
    }

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to initialize Tokio runtime"),
    );

    // Global hotkey channel
    let (hotkey_tx, hotkey_rx) = mpsc::unbounded_channel();

    let window_title = "Cue - AI Stealth Assistant";

    let screen_w = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        )
    } as f32;
    let default_width = 540.0;
    let default_height = 360.0;
    let pos_x = ((screen_w - default_width) / 2.0).max(0.0);
    let pos_y = 24.0;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title)
            .with_position([pos_x, pos_y])
            .with_inner_size([default_width, default_height])
            .with_min_inner_size([380.0, 220.0])
            .with_transparent(true)
            .with_decorations(false)
            .with_resizable(true)
            .with_always_on_top()
            .with_taskbar(false),
        ..Default::default()
    };

    eframe::run_native(
        window_title,
        native_options,
        Box::new(move |cc| {
            // Start hotkey listener with egui_ctx so hotkeys instantly wake up the event loop, even in pass-through mode
            if let Err(e) = stealth::start_hotkey_listener(hotkey_tx, cc.egui_ctx.clone()) {
                eprintln!("Warning: could not register global hotkeys: {e}");
            }
            Ok(Box::new(ui::CueApp::new(cc, rt, hotkey_rx)))
        }),
    )
}
