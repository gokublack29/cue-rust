#[cfg(windows)]
mod windows_impl {
    use base64::prelude::*;
    use image::codecs::jpeg::JpegEncoder;

    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject,
        SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    struct ScreenDcGuard {
        hwnd: HWND,
        hdc: HDC,
    }

    impl Drop for ScreenDcGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseDC(self.hwnd, self.hdc);
            }
        }
    }

    struct MemDcGuard(HDC);

    impl Drop for MemDcGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }

    struct BitmapGuard(HBITMAP);

    impl Drop for BitmapGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0 .0));
            }
        }
    }

    struct SelectGuard {
        hdc: HDC,
        prev: HGDIOBJ,
    }

    impl Drop for SelectGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = SelectObject(self.hdc, self.prev);
            }
        }
    }

    pub fn capture_screen_jpeg_base64() -> Result<String, String> {
        unsafe {
            let mut origin_x = 0i32;
            let mut origin_y = 0i32;
            let mut width = GetSystemMetrics(SM_CXSCREEN);
            let mut height = GetSystemMetrics(SM_CYSCREEN);

            // Capture the monitor of the active non-Cue window; if Cue itself is focused, fall back to cursor monitor
            let cue_hwnd = crate::stealth::stored_hwnd() as isize;
            let fg = GetForegroundWindow();
            let hmon = if !fg.is_invalid() && (fg.0 as isize) != cue_hwnd {
                MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
            } else {
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() {
                    MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
                } else {
                    MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
                }
            };
            if !hmon.is_invalid() {
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                    let mw = mi.rcMonitor.right - mi.rcMonitor.left;
                    let mh = mi.rcMonitor.bottom - mi.rcMonitor.top;
                    if mw > 0 && mh > 0 {
                        origin_x = mi.rcMonitor.left;
                        origin_y = mi.rcMonitor.top;
                        width = mw;
                        height = mh;
                    }
                }
            }

            if width <= 0 || height <= 0 {
                return Err(format!("Invalid screen dimensions: {}x{}", width, height));
            }

            // Max long edge of 1920px preserves 100% native 1:1 resolution on 1080p, 1536x864, and laptop displays.
            // When 1:1 matches, direct BitBlt avoids any scaling, blur, or dropped scanlines.
            const MAX_LONG_EDGE: i32 = 1920;
            let (target_w, target_h) = if width > MAX_LONG_EDGE || height > MAX_LONG_EDGE {
                if width >= height {
                    let h =
                        ((height as f64) * (MAX_LONG_EDGE as f64 / width as f64)).round() as i32;
                    (MAX_LONG_EDGE, h.max(1))
                } else {
                    let w =
                        ((width as f64) * (MAX_LONG_EDGE as f64 / height as f64)).round() as i32;
                    (w.max(1), MAX_LONG_EDGE)
                }
            } else {
                (width, height)
            };

            // NULL HWND gives the virtual-screen DC spanning all monitors
            let hwnd = HWND(std::ptr::null_mut());
            let screen_hdc = GetDC(hwnd);
            if screen_hdc.is_invalid() {
                return Err("Failed to obtain desktop DC".to_string());
            }
            let _screen_guard = ScreenDcGuard {
                hwnd,
                hdc: screen_hdc,
            };

            let mem_hdc = CreateCompatibleDC(screen_hdc);
            if mem_hdc.is_invalid() {
                return Err("Failed to create compatible memory DC".to_string());
            }
            let _mem_dc_guard = MemDcGuard(mem_hdc);

            let hbitmap = CreateCompatibleBitmap(screen_hdc, target_w, target_h);
            if hbitmap.is_invalid() {
                return Err("Failed to create compatible bitmap".to_string());
            }
            let _bitmap_guard = BitmapGuard(hbitmap);

            let prev_obj = SelectObject(mem_hdc, HGDIOBJ(hbitmap.0));
            if prev_obj.is_invalid() {
                return Err("Failed to select bitmap into memory DC".to_string());
            }
            let _select_guard = SelectGuard {
                hdc: mem_hdc,
                prev: prev_obj,
            };

            if target_w == width && target_h == height {
                // Pixel-perfect 1:1 native hardware copy: zero interpolation, razor-sharp code text
                if let Err(e) = BitBlt(
                    mem_hdc, 0, 0, target_w, target_h, screen_hdc, origin_x, origin_y, SRCCOPY,
                ) {
                    return Err(format!("BitBlt failed: {e}"));
                }
            } else {
                // High-fidelity halftone filtering for ultra-wide / 4K displays
                SetStretchBltMode(mem_hdc, HALFTONE);
                let _ = SetBrushOrgEx(mem_hdc, 0, 0, None);
                let blt_ok = StretchBlt(
                    mem_hdc, 0, 0, target_w, target_h, screen_hdc, origin_x, origin_y, width,
                    height, SRCCOPY,
                );
                if !blt_ok.as_bool() {
                    return Err("StretchBlt failed".to_string());
                }
            }

            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: target_w,
                    biHeight: -target_h, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default()],
            };

            let total_pixels = (target_w as usize) * (target_h as usize);
            let mut bgra_buf: Vec<u8> = vec![0u8; total_pixels * 4];

            let lines = GetDIBits(
                mem_hdc,
                hbitmap,
                0,
                target_h as u32,
                Some(bgra_buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );

            if lines == 0 {
                return Err("GetDIBits failed to extract screen pixels".to_string());
            }

            // Convert BGRA to RGB directly in memory
            let mut rgb_buf: Vec<u8> = Vec::with_capacity(total_pixels * 3);
            for chunk in bgra_buf.as_chunks::<4>().0 {
                rgb_buf.push(chunk[2]);
                rgb_buf.push(chunk[1]);
                rgb_buf.push(chunk[0]);
            }

            let mut jpeg_bytes = Vec::new();
            let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_bytes, 85);
            encoder
                .encode(
                    &rgb_buf,
                    target_w as u32,
                    target_h as u32,
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| format!("JPEG encoding failed: {e}"))?;

            let base64_str = BASE64_STANDARD.encode(&jpeg_bytes);
            Ok(base64_str)
        }
    }
}

#[cfg(windows)]
pub use windows_impl::capture_screen_jpeg_base64;

#[cfg(not(windows))]
pub fn capture_screen_jpeg_base64() -> Result<String, String> {
    Err("Screen capture is only supported on Windows".to_string())
}
