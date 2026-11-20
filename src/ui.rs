use eframe::egui::{self, Color32, ResizeDirection, Rounding, Shadow, Stroke, Vec2};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::groq::{GroqClient, GroqEvent, KeyPool};
use crate::screen;
use crate::stealth::{self, HotkeyAction};

pub struct CueApp {
    rt: Arc<tokio::runtime::Runtime>,
    hotkey_rx: mpsc::UnboundedReceiver<HotkeyAction>,
    window_title: String,
    stealth_applied: bool,
    hwnd_raw: *mut std::ffi::c_void,
    pub click_through: bool,

    // Keys and config
    keys_input: String,
    show_settings: bool,
    active_key_info: String,

    // Chat and interaction
    prompt: String,
    include_screenshot: bool,
    streaming_text: String,
    status_message: String,
    is_streaming: bool,

    // Async stream receiver
    stream_rx: Option<mpsc::Receiver<GroqEvent>>,
    http_client: reqwest::Client,
    egui_ctx: egui::Context,
    last_copied: Option<(usize, std::time::Instant)>,
    last_generated_code: Option<String>,

    // Multi-turn session memory for iterative testcase fixing
    conversation_history: Vec<serde_json::Value>,
    pending_user_message: Option<serde_json::Value>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ContentSegment {
    Text(String),
    CodeBlock { language: String, code: String },
}

pub fn parse_content_segments(raw: &str) -> Vec<ContentSegment> {
    let mut segments = Vec::new();
    let mut remaining = raw;

    while let Some(start_idx) = remaining.find("```") {
        let (before, after_start) = remaining.split_at(start_idx);
        let trimmed_before = before.trim();
        if !trimmed_before.is_empty() {
            segments.push(ContentSegment::Text(trimmed_before.to_string()));
        }

        let after_fence = &after_start[3..];
        if let Some(newline_idx) = after_fence.find('\n') {
            let lang = after_fence[..newline_idx].trim().to_string();
            let code_body = &after_fence[newline_idx + 1..];

            if let Some(end_idx) = code_body.find("```") {
                let code = code_body[..end_idx].trim_end().to_string();
                segments.push(ContentSegment::CodeBlock {
                    language: if lang.is_empty() { "code".to_string() } else { lang },
                    code,
                });
                remaining = &code_body[end_idx + 3..];
            } else {
                let code = code_body.trim_end().to_string();
                segments.push(ContentSegment::CodeBlock {
                    language: if lang.is_empty() { "code".to_string() } else { lang },
                    code,
                });
                remaining = "";
                break;
            }
        } else {
            remaining = "";
            break;
        }
    }

    let trimmed_remaining = remaining.trim();
    if !trimmed_remaining.is_empty() {
        segments.push(ContentSegment::Text(trimmed_remaining.to_string()));
    }

    segments
}

impl CueApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Arc<tokio::runtime::Runtime>,
        hotkey_rx: mpsc::UnboundedReceiver<HotkeyAction>,
        window_title: &str,
    ) -> Self {
        // Theme egui visuals for dark translucent frosted glass
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = Color32::from_rgba_unmultiplied(15, 17, 24, 175);
        visuals.panel_fill = Color32::from_rgba_unmultiplied(15, 17, 24, 175);
        visuals.extreme_bg_color = Color32::from_rgba_unmultiplied(10, 12, 16, 140);
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgba_unmultiplied(24, 28, 38, 140);
        visuals.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(30, 36, 48, 150);
        visuals.widgets.hovered.bg_fill = Color32::from_rgba_unmultiplied(45, 55, 75, 190);
        visuals.widgets.active.bg_fill = Color32::from_rgba_unmultiplied(50, 65, 90, 210);
        visuals.override_text_color = Some(Color32::from_rgb(240, 244, 250));
        cc.egui_ctx.set_visuals(visuals);

        // Load Windows Segoe UI Symbol font so symbols render cleanly with zero tofu squares
        let mut fonts = egui::FontDefinitions::default();
        #[cfg(windows)]
        {
            if let Ok(data) = fs::read("C:\\Windows\\Fonts\\seguisym.ttf") {
                fonts.font_data.insert(
                    "seguisym".to_owned(),
                    Arc::new(egui::FontData::from_owned(data)),
                );
                if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    family.push("seguisym".to_owned());
                }
                if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    family.push("seguisym".to_owned());
                }
            }
        }
        cc.egui_ctx.set_fonts(fonts);

        // Load stored keys (groq_keys.txt, .env, or environment variables)
        let mut loaded_keys = String::new();
        if Path::new("groq_keys.txt").exists() {
            if let Ok(content) = fs::read_to_string("groq_keys.txt") {
                loaded_keys = content;
            }
        }
        if loaded_keys.trim().is_empty() {
            loaded_keys = Self::load_from_env_files();
        }

        let show_settings = loaded_keys.trim().is_empty();

        Self {
            rt,
            hotkey_rx,
            window_title: window_title.to_string(),
            stealth_applied: false,
            hwnd_raw: std::ptr::null_mut(),
            click_through: false,
            keys_input: loaded_keys,
            show_settings,
            active_key_info: "No keys configured".to_string(),
            prompt: String::new(),
            include_screenshot: true,
            streaming_text: String::new(),
            status_message: String::new(),
            is_streaming: false,
            stream_rx: None,
            http_client: reqwest::Client::builder()
                .tcp_nodelay(true)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .unwrap_or_default(),
            egui_ctx: cc.egui_ctx.clone(),
            last_copied: None,
            last_generated_code: None,
            conversation_history: Vec::new(),
            pending_user_message: None,
        }
    }

    fn load_from_env_files() -> String {
        let candidates = [".env", "cue-rust/.env", "../.env"];
        for p in candidates {
            if let Ok(content) = fs::read_to_string(p) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed
                        .strip_prefix("GROQ_API_KEYS=")
                        .or_else(|| trimmed.strip_prefix("GROQ_API_KEY="))
                    {
                        let val = rest.trim().trim_matches('"').trim_matches('\'');
                        if !val.is_empty() {
                            return val.to_string();
                        }
                    }
                }
            }
        }
        if let Ok(k) = std::env::var("GROQ_API_KEYS").or_else(|_| std::env::var("GROQ_API_KEY")) {
            return k;
        }
        String::new()
    }

    fn parsed_keys(&self) -> Vec<String> {
        self.keys_input
            .split(['\n', ',', ';'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && (s.starts_with("gsk_") || s.len() > 10))
            .collect()
    }

    fn save_keys(&self) {
        let _ = fs::write("groq_keys.txt", &self.keys_input);
    }

    pub fn toggle_click_through(&mut self) {
        self.click_through = !self.click_through;
        let _ = stealth::set_click_through(self.hwnd_raw, self.click_through);
        self.status_message = if self.click_through {
            "Pass-Through ON (clicks pass through to apps below)".to_string()
        } else {
            "Interactive mode".to_string()
        };
    }

    pub fn extract_last_code(&self) -> Option<String> {
        let segments = parse_content_segments(&self.streaming_text);
        for seg in segments.into_iter().rev() {
            if let ContentSegment::CodeBlock { code, .. } = seg {
                if !code.trim().is_empty() {
                    return Some(code);
                }
            }
        }
        if let Some(ref c) = self.last_generated_code {
            if !c.trim().is_empty() {
                return Some(c.clone());
            }
        }
        None
    }

    pub fn trigger_fix_error(&mut self) {
        let prompt = if self.conversation_history.is_empty() {
            if let Some(code) = self.extract_last_code() {
                format!(
                    "The previous code solution below failed a test case shown on screen:\n\n```\n{}\n```\n\n\
                     1. Read the failing test case input, actual output, and expected output from the test result panel on screen.\n\
                     2. Reason through why the code failed this test case and identify the bug or flawed logic.\n\
                     3. Fix the bug to pass all test cases. Output ONLY the complete, corrected runnable code block. No explanations.",
                    code
                )
            } else {
                "The code on screen failed a test case. Read the failing test input, actual output vs expected output in the test result panel on screen. Diagnose why it failed and fix the code. Output ONLY the complete, corrected runnable code block. No explanations.".to_string()
            }
        } else {
            "The previous code solution failed on the test case shown in the test result panel on screen.\n\
             1. Read the failing test case (Input, Output, Expected) or runtime error in the test result panel on screen.\n\
             2. Reason through why the previous code failed this test case.\n\
             3. Fix the bug and output ONLY the complete, updated runnable code solution in a code block. No explanations."
                .to_string()
        };
        self.trigger_generation(Some(prompt), true, false);
    }

    fn trigger_generation(&mut self, custom_prompt: Option<String>, capture_screen: bool, is_fresh: bool) {
        if self.is_streaming {
            return;
        }

        let keys = self.parsed_keys();
        if keys.is_empty() {
            self.status_message = "Add at least one valid Groq API key in Settings.".to_string();
            self.show_settings = true;
            return;
        }

        let user_prompt = custom_prompt.unwrap_or_else(|| {
            let p = self.prompt.trim().to_string();
            if p.is_empty() {
                "Help me with what is on screen right now. Be direct and concise.".to_string()
            } else {
                p
            }
        });

        // Cache last generated code before clearing text
        if let Some(c) = self.extract_last_code() {
            self.last_generated_code = Some(c);
        }

        if is_fresh {
            self.conversation_history.clear();
        }

        self.streaming_text.clear();
        self.is_streaming = true;
        self.status_message = if is_fresh {
            "Screen captured -> querying Groq...".to_string()
        } else {
            "Screen captured -> fixing testcase failure...".to_string()
        };

        let pool = KeyPool::new(keys);
        self.active_key_info = format!("Key 1/{} active", pool.total_keys());
        let client = GroqClient::new_with_client(pool, self.http_client.clone());

        let (tx, rx) = mpsc::channel(100);
        self.stream_rx = Some(rx);

        // Retain problem context while pruning older turns to prevent TPM token overflow
        let prior_history = if is_fresh {
            Vec::new()
        } else if self.conversation_history.len() > 6 {
            let mut pruned = Vec::new();
            if self.conversation_history.len() >= 2 {
                pruned.push(self.conversation_history[0].clone());
                pruned.push(self.conversation_history[1].clone());
            }
            let tail_start = self.conversation_history.len().saturating_sub(4);
            for msg in &self.conversation_history[tail_start..] {
                pruned.push(msg.clone());
            }
            pruned
        } else {
            self.conversation_history.clone()
        };

        let system_prompt = "You are Cue, an elite coding interview and competitive programming assistant. \
                             NEVER explain concepts, teach, or lecture. \
                             Analyze problem constraints and edge cases carefully. \
                             Verify state transitions and select optimal algorithms (O(1) space or greedy/two-pointer where applicable) to prevent TLE. \
                             Output ONLY the complete, optimal, runnable code solution inside a clean code block. \
                             Zero commentary, zero conversational filler, no code walkthrough."
            .to_string();

        let user_start = std::time::Instant::now();
        let egui_ctx = self.egui_ctx.clone();

        self.rt.spawn(async move {
            let cap_start = std::time::Instant::now();
            let image_base64 = if capture_screen {
                tokio::task::spawn_blocking(screen::capture_screen_jpeg_base64)
                    .await
                    .unwrap_or_else(|e| Err(format!("Capture task join error: {e}")))
            } else {
                Ok(String::new())
            };
            let capture_ms = if capture_screen {
                cap_start.elapsed().as_millis() as u64
            } else {
                0
            };

            let img_opt = match image_base64 {
                Ok(b64) if !b64.is_empty() => Some(b64),
                Ok(_) => None,
                Err(e) => {
                    let _ = tx
                        .send(GroqEvent::Error(format!("Screenshot error: {e}")))
                        .await;
                    egui_ctx.request_repaint();
                    return;
                }
            };

            let image_kb = img_opt.as_ref().map(|s| (s.len() * 3) / 4 / 1024).unwrap_or(0);

            let current_user_msg = if let Some(img) = img_opt {
                serde_json::json!({
                    "role": "user",
                    "content": [
                        { "type": "text", "text": user_prompt },
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:image/jpeg;base64,{}", img)
                            }
                        }
                    ]
                })
            } else {
                serde_json::json!({
                    "role": "user",
                    "content": user_prompt
                })
            };

            let _ = tx.send(GroqEvent::UserTurn(current_user_msg.clone())).await;

            let mut messages = Vec::new();
            messages.push(serde_json::json!({
                "role": "system",
                "content": system_prompt
            }));

            for m in prior_history {
                messages.push(m);
            }
            messages.push(current_user_msg);

            client
                .stream_messages_request(messages, capture_ms, image_kb, user_start, egui_ctx, tx)
                .await;
        });
    }

    fn apply_stealth_if_needed(&mut self) {
        #[cfg(windows)]
        if !self.stealth_applied {
            use windows::core::HSTRING;
            use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

            let title_wide = HSTRING::from(&self.window_title);
            unsafe {
                if let Ok(hwnd) = FindWindowW(None, &title_wide) {
                    if !hwnd.is_invalid() {
                        let _ = stealth::enable_stealth(hwnd.0 as _);
                        // Apply OS-level semi-transparency (alpha = 190 out of 255 for sleek frosted glass)
                        let _ = stealth::set_window_opacity(hwnd.0 as _, 190);
                        self.hwnd_raw = hwnd.0 as _;
                        self.stealth_applied = true;
                    }
                }
            }
        }
    }
}

impl eframe::App for CueApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_stealth_if_needed();

        // Default cursor: always normal arrow so no random I-beam appears on screen recordings
        ctx.set_cursor_icon(egui::CursorIcon::Default);

        // Keep event loop active while streaming so every token paints smoothly in real-time
        if self.is_streaming {
            ctx.request_repaint();
        }

        // Escape key: dismiss text focus or hide window completely
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::NULL));
            let _ = stealth::toggle_window_visibility(self.hwnd_raw);
        }

        // Ctrl+Q or Alt+F4: quit Cue completely
        if ctx.input(|i| (i.modifiers.ctrl && i.key_pressed(egui::Key::Q)) || (i.modifiers.alt && i.key_pressed(egui::Key::F4))) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Process global hotkeys
        while let Ok(action) = self.hotkey_rx.try_recv() {
            match action {
                HotkeyAction::SmartAssist => {
                    self.trigger_generation(
                        Some("Directly answer or solve what is shown on screen. Return ONLY the direct answer or complete code solution. Zero explanation, zero teaching.".to_string()),
                        true,
                        true,
                    );
                }
                HotkeyAction::SolveCode => {
                    self.trigger_generation(
                        Some("Solve the code problem shown on screen. Output ONLY the complete, working code solution in a code block. Do NOT explain how it works.".to_string()),
                        true,
                        true,
                    );
                }
                HotkeyAction::FixCode => {
                    self.trigger_fix_error();
                }
                HotkeyAction::ToggleClickThrough => {
                    self.toggle_click_through();
                    ctx.request_repaint();
                }
                HotkeyAction::ToggleVisibility => {
                    let _ = stealth::toggle_window_visibility(self.hwnd_raw);
                    ctx.request_repaint();
                }
            }
        }

        // Process streaming tokens from Groq
        if let Some(rx) = &mut self.stream_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    GroqEvent::Token(token) => {
                        if self.streaming_text.is_empty() {
                            self.status_message = "Receiving response...".to_string();
                        }
                        self.streaming_text.push_str(&token);
                        ctx.request_repaint();
                    }
                    GroqEvent::UserTurn(user_msg) => {
                        self.pending_user_message = Some(user_msg);
                        ctx.request_repaint();
                    }
                    GroqEvent::KeyRotated {
                        from_index,
                        to_index,
                        reason,
                    } => {
                        self.status_message = format!(
                            "Key {from_index} {reason} -> Switched to Key {to_index}!"
                        );
                        self.active_key_info = format!("Key {to_index} active (rotated)");
                        ctx.request_repaint();
                    }
                    GroqEvent::Done(metrics_opt) => {
                        self.is_streaming = false;
                        if let Some(user_msg) = self.pending_user_message.take() {
                            self.conversation_history.push(user_msg);
                            if !self.streaming_text.trim().is_empty() {
                                self.conversation_history.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": self.streaming_text.trim()
                                }));
                            }
                        }
                        let segments = parse_content_segments(&self.streaming_text);
                        for seg in segments.into_iter().rev() {
                            if let ContentSegment::CodeBlock { code, .. } = seg {
                                if !code.trim().is_empty() {
                                    self.last_generated_code = Some(code);
                                    break;
                                }
                            }
                        }
                        if let Some(m) = metrics_opt {
                            let turn_info = if self.conversation_history.len() > 2 {
                                format!(" · Turn {}", self.conversation_history.len() / 2)
                            } else {
                                String::new()
                            };
                            self.status_message = format!(
                                "Wall-clock: {}ms (TTFT: {}ms · Screen: {}ms/{}KB · {} tok @ {:.0} tok/s{})",
                                m.total_ms, m.ttft_ms, m.capture_ms, m.image_kb, m.token_count, m.tokens_per_sec, turn_info
                            );
                        } else {
                            self.status_message = "Complete.".to_string();
                        }
                        ctx.request_repaint();
                    }
                    GroqEvent::Error(err) => {
                        self.pending_user_message = None;
                        self.status_message = format!("Error: {err}");
                        self.is_streaming = false;
                        ctx.request_repaint();
                    }
                }
            }
        }

        // Custom Frosted Translucent Dark Glass Container
        let frame = egui::Frame::none()
            .fill(Color32::from_rgba_unmultiplied(15, 17, 24, 175))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 26)))
            .rounding(Rounding::same(14.0))
            .shadow(Shadow {
                offset: [0.0, 4.0].into(),
                blur: 12.0,
                spread: 0.0,
                color: Color32::from_rgba_unmultiplied(0, 0, 0, 90),
            })
            .inner_margin(12.0);

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            // Header Bar
            ui.horizontal(|ui| {
                // Draggable Title Header
                let drag_rect = ui.allocate_rect(
                    egui::Rect::from_min_size(ui.cursor().min, Vec2::new(110.0, 26.0)),
                    egui::Sense::drag(),
                );
                if drag_rect.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.label(
                    egui::RichText::new("CUE")
                        .size(14.0)
                        .color(Color32::from_rgb(96, 165, 250))
                        .strong(),
                );

                // Live status pill badge
                if self.click_through {
                    ui.colored_label(
                        Color32::from_rgb(251, 191, 36),
                        "[PASS-THROUGH] (Ctrl+Shift+C)",
                    );
                } else {
                    let key_count = self.parsed_keys().len();
                    ui.colored_label(
                        Color32::from_rgb(52, 211, 153),
                        format!("• LIVE · {} keys", key_count),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(egui::RichText::new("×").size(14.0).strong()).on_hover_text("Quit Cue (Ctrl+Q)").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button(egui::RichText::new("Hide").size(11.0)).on_hover_text("Hide window (Ctrl+/)").clicked() {
                        let _ = stealth::toggle_window_visibility(self.hwnd_raw);
                    }
                    if ui
                        .button(if self.show_settings { "▲ Close" } else { "Keys" })
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
                    }
                    // Click-Through Mode Toggle Button
                    let pass_btn = ui.button(if self.click_through {
                        egui::RichText::new("Pass-Through ON").color(Color32::from_rgb(251, 191, 36))
                    } else {
                        egui::RichText::new("Pass-Through").color(Color32::from_rgb(203, 213, 225))
                    });
                    if pass_btn.clicked() {
                        self.toggle_click_through();
                        ctx.request_repaint();
                    }
                });
            });

            ui.add_space(6.0);

            // Collapsible settings for multiple Groq keys
            if self.show_settings {
                ui.group(|ui| {
                    ui.label(
                        egui::RichText::new("Groq API Key Pool (auto-rotates on 429 rate limit):")
                            .size(11.5)
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new("Loaded from .env or groq_keys.txt. Model: qwen/qwen3.8-27b.")
                            .size(10.5)
                            .color(Color32::from_rgb(148, 163, 184)),
                    );

                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut self.keys_input)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                    if edit.changed() {
                        self.save_keys();
                    }

                    ui.horizontal(|ui| {
                        if ui.button("Save Keys").clicked() {
                            self.save_keys();
                            self.status_message = "Keys saved to groq_keys.txt".to_string();
                        }
                        if ui.button("Done").clicked() {
                            self.save_keys();
                            self.show_settings = false;
                        }
                        if ui.button(egui::RichText::new("Quit Cue").color(Color32::from_rgb(248, 113, 113))).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
                ui.add_space(6.0);
            }

            // Quick Hotkey Actions with clear visual hierarchy
            ui.horizontal(|ui| {
                let assist_btn = egui::Button::new(
                    egui::RichText::new("Smart Assist")
                        .size(11.5)
                        .color(Color32::from_rgb(147, 197, 253))
                        .strong(),
                )
                .fill(Color32::from_rgba_unmultiplied(59, 130, 246, 45))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(59, 130, 246, 110)))
                .rounding(Rounding::same(8.0));

                if ui.add(assist_btn).on_hover_text("Ctrl+Shift+Enter: Solve or answer screen").clicked() {
                    self.trigger_generation(None, true, true);
                }

                let code_btn = egui::Button::new(
                    egui::RichText::new("Solve Code")
                        .size(11.5)
                        .color(Color32::from_rgb(216, 180, 254))
                        .strong(),
                )
                .fill(Color32::from_rgba_unmultiplied(168, 85, 247, 45))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(168, 85, 247, 110)))
                .rounding(Rounding::same(8.0));

                if ui.add(code_btn).on_hover_text("Ctrl+H: Solve code problem on screen (starts fresh)").clicked() {
                    self.trigger_generation(
                        Some("Solve the code problem shown on screen. Output ONLY the complete, working code solution in a code block. Do NOT explain how it works.".to_string()),
                        true,
                        true,
                    );
                }

                let fix_btn = egui::Button::new(
                    egui::RichText::new("Fix Error")
                        .size(11.5)
                        .color(Color32::from_rgb(252, 165, 165))
                        .strong(),
                )
                .fill(Color32::from_rgba_unmultiplied(239, 68, 68, 45))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(239, 68, 68, 110)))
                .rounding(Rounding::same(8.0));

                if ui.add(fix_btn).on_hover_text("Ctrl+Shift+F: Fix failed test case using multi-turn problem memory + 1:1 screen").clicked() {
                    self.trigger_fix_error();
                }

                if !self.conversation_history.is_empty() {
                    let turns = (self.conversation_history.len() + 1) / 2;
                    let reset_btn = egui::Button::new(
                        egui::RichText::new(format!("Reset ({turns} turns)"))
                            .size(11.0)
                            .color(Color32::from_rgb(148, 163, 184)),
                    )
                    .fill(Color32::from_rgba_unmultiplied(71, 85, 105, 45))
                    .rounding(Rounding::same(8.0));

                    if ui.add(reset_btn).on_hover_text("Reset problem thread memory for a brand new problem").clicked() {
                        self.conversation_history.clear();
                        self.status_message = "Memory reset. Ready for new problem.".to_string();
                    }
                }
            });

            ui.add_space(6.0);

            // Refined Prompt Input Bar
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.prompt)
                        .hint_text("Ask, or press Ctrl+H (Solve) / Ctrl+Shift+F (Fix Failed Test)...")
                        .desired_width(ui.available_width() - 84.0),
                );
                if response.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let p = self.prompt.clone();
                    let inc_scr = self.include_screenshot;
                    ctx.memory_mut(|m| m.surrender_focus(egui::Id::NULL));
                    self.trigger_generation(Some(p), inc_scr, true);
                }

                let send_btn = egui::Button::new(
                    egui::RichText::new(if self.is_streaming { "..." } else { "Send" })
                        .color(Color32::WHITE)
                        .strong(),
                )
                .fill(Color32::from_rgba_unmultiplied(59, 130, 246, 180))
                .rounding(Rounding::same(6.0));

                if ui.add(send_btn).clicked() {
                    let p = self.prompt.clone();
                    let inc_scr = self.include_screenshot;
                    ctx.memory_mut(|m| m.surrender_focus(egui::Id::NULL));
                    self.trigger_generation(Some(p), inc_scr, true);
                }
            });

            ui.horizontal(|ui| {
                ui.checkbox(&mut self.include_screenshot, "Capture screen");
                if !self.status_message.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.status_message)
                                .size(10.5)
                                .color(Color32::from_rgb(147, 197, 253)),
                        )
                        .truncate(),
                    );
                }
            });

            ui.separator();

            // Streaming Output Viewport with default arrow cursor
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    // Force default arrow cursor over entire viewport
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Default);

                    if self.streaming_text.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(48.0);
                            ui.label(
                                egui::RichText::new("Ready. Press Ctrl+Shift+Enter or type above.")
                                    .size(13.0)
                                    .color(Color32::from_rgb(148, 163, 184)),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("Ctrl+Shift+Enter: Answer · Ctrl+H: Solve Code · Ctrl+Shift+F: Fix Error")
                                    .size(11.0)
                                    .color(Color32::from_rgb(125, 211, 252)),
                            );
                            ui.label(
                                egui::RichText::new("Ctrl+/ : Hide/Show Window · Ctrl+Shift+C: Mouse Pass-Through")
                                    .size(11.0)
                                    .color(Color32::from_rgb(110, 231, 183)),
                            );
                        });
                    } else {
                        let segments = parse_content_segments(&self.streaming_text);
                        let mut block_counter = 0;

                        for seg in segments {
                            match seg {
                                ContentSegment::CodeBlock { language, code } => {
                                    block_counter += 1;
                                    let current_idx = block_counter;

                                    let code_frame = egui::Frame::none()
                                        .fill(Color32::from_rgba_unmultiplied(8, 10, 16, 185))
                                        .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(59, 130, 246, 70)))
                                        .rounding(Rounding::same(8.0))
                                        .inner_margin(egui::Margin::symmetric(10.0, 8.0));

                                    code_frame.show(ui, |ui| {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::Default);

                                        // Header: Language badge + Copy button
                                        ui.horizontal(|ui| {
                                            let tag = language.to_uppercase();
                                            ui.label(
                                                egui::RichText::new(&tag)
                                                    .size(11.0)
                                                    .color(Color32::from_rgb(96, 165, 250))
                                                    .strong(),
                                            );

                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                let is_copied = self
                                                    .last_copied
                                                    .as_ref()
                                                    .map(|(idx, time)| *idx == current_idx && time.elapsed().as_secs_f32() < 2.0)
                                                    .unwrap_or(false);

                                                let btn_text = if is_copied { "Copied!" } else { "Copy Code" };
                                                let btn = egui::Button::new(
                                                    egui::RichText::new(btn_text)
                                                        .size(11.0)
                                                        .color(if is_copied { Color32::from_rgb(52, 211, 153) } else { Color32::WHITE })
                                                        .strong(),
                                                )
                                                .fill(if is_copied {
                                                    Color32::from_rgba_unmultiplied(16, 185, 129, 60)
                                                } else {
                                                    Color32::from_rgba_unmultiplied(59, 130, 246, 130)
                                                })
                                                .rounding(Rounding::same(4.0));

                                                if ui.add(btn).clicked() {
                                                    ui.output_mut(|o| o.copied_text = code.clone());
                                                    self.last_copied = Some((current_idx, std::time::Instant::now()));
                                                    self.status_message = format!("Copied {tag} code cleanly!");
                                                }
                                            });
                                        });

                                        ui.add_space(4.0);

                                        // Code text: wrap_mode prevents horizontal boundary breaks, selectable(false) prevents I-beam cursor
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&code)
                                                    .monospace()
                                                    .size(12.5)
                                                    .color(Color32::from_rgb(226, 232, 240)),
                                            )
                                            .wrap_mode(egui::TextWrapMode::Wrap)
                                            .selectable(false),
                                        );
                                    });

                                    ui.add_space(8.0);
                                }
                                ContentSegment::Text(t) => {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
                                    // Non-code text: wrap_mode prevents horizontal boundary breaks
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&t)
                                                .size(13.0)
                                                .color(Color32::from_rgb(241, 245, 249)),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Wrap)
                                        .selectable(false),
                                    );
                                    ui.add_space(6.0);
                                }
                            }
                        }
                    }
                });

            // Bottom bar
            if !self.streaming_text.is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("📋 Copy All").clicked() {
                        ui.output_mut(|o| o.copied_text = self.streaming_text.clone());
                        self.status_message = "Full response copied to clipboard!".to_string();
                    }
                    if ui.button("🗑 Clear").clicked() {
                        self.streaming_text.clear();
                        self.status_message.clear();
                    }
                });
            }
        });

        // Window edge and corner resize handling (silent: cursor stays default arrow, no giveaway arrows in recordings)
        handle_window_resizing(ctx);
    }
}

fn handle_window_resizing(ctx: &egui::Context) {
    let screen_rect = ctx.screen_rect();
    let border_width = 8.0;

    // Corner: SouthEast (bottom-right 24x24)
    let br_rect = egui::Rect::from_min_max(
        egui::pos2(screen_rect.max.x - 24.0, screen_rect.max.y - 24.0),
        screen_rect.max,
    );
    // Right border
    let right_rect = egui::Rect::from_min_max(
        egui::pos2(screen_rect.max.x - border_width, screen_rect.min.y + 16.0),
        egui::pos2(screen_rect.max.x, screen_rect.max.y - 24.0),
    );
    // Bottom border
    let bottom_rect = egui::Rect::from_min_max(
        egui::pos2(screen_rect.min.x + 16.0, screen_rect.max.y - border_width),
        egui::pos2(screen_rect.max.x - 24.0, screen_rect.max.y),
    );
    // Left border
    let left_rect = egui::Rect::from_min_max(
        screen_rect.min,
        egui::pos2(screen_rect.min.x + border_width, screen_rect.max.y - 16.0),
    );

    let pointer_pos = ctx.input(|i| i.pointer.hover_pos());
    let pointer_down = ctx.input(|i| i.pointer.primary_down());

    if let Some(pos) = pointer_pos {
        if pointer_down {
            if br_rect.contains(pos) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::SouthEast));
            } else if right_rect.contains(pos) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::East));
            } else if bottom_rect.contains(pos) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::South));
            } else if left_rect.contains(pos) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(ResizeDirection::West));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_env_keys() {
        let keys_str = CueApp::load_from_env_files();
        assert!(!keys_str.is_empty(), "Expected to read GROQ_API_KEYS from .env");
        let parsed: Vec<_> = keys_str
            .split(['\n', ',', ';'])
            .map(|s| s.trim())
            .filter(|s| s.starts_with("gsk_"))
            .collect();
        assert_eq!(parsed.len(), 27, "Expected 27 Groq keys parsed from .env");
    }

    #[test]
    fn test_parse_content_segments_code_block() {
        let input = "Here is the code:\n```python\ndef solve():\n    return 42\n```\nDone.";
        let segments = parse_content_segments(input);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0], ContentSegment::Text("Here is the code:".to_string()));
        assert_eq!(
            segments[1],
            ContentSegment::CodeBlock {
                language: "python".to_string(),
                code: "def solve():\n    return 42".to_string(),
            }
        );
        assert_eq!(segments[2], ContentSegment::Text("Done.".to_string()));
    }

    #[test]
    fn test_parse_content_segments_unclosed_stream() {
        let input = "```rust\nfn main() {\n    println!(\"hello\");\n";
        let segments = parse_content_segments(input);
        assert_eq!(segments.len(), 1);
        assert_eq!(
            segments[0],
            ContentSegment::CodeBlock {
                language: "rust".to_string(),
                code: "fn main() {\n    println!(\"hello\");".to_string(),
            }
        );
    }

    #[test]
    fn test_conversation_history_pruning() {
        let mut history = Vec::new();
        for i in 0..8 {
            history.push(serde_json::json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": format!("message {i}")
            }));
        }
        assert_eq!(history.len(), 8);

        let mut pruned = Vec::new();
        pruned.push(history[0].clone());
        pruned.push(history[1].clone());
        let tail_start = history.len().saturating_sub(4);
        for msg in &history[tail_start..] {
            pruned.push(msg.clone());
        }
        assert_eq!(pruned.len(), 6);
        assert_eq!(pruned[0]["content"], "message 0");
        assert_eq!(pruned[1]["content"], "message 1");
        assert_eq!(pruned[2]["content"], "message 4");
        assert_eq!(pruned[5]["content"], "message 7");
    }
}
