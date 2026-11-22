//! Scroll mode's indicator: an outline on the active area and small digit
//! badges on the others, in a window that takes no clicks and no focus.
//!
//! `WS_EX_LAYERED | TRANSPARENT | NOACTIVATE | TOPMOST | TOOLWINDOW`, so
//! the target app keeps focus and its menus stay open (spike #34). It
//! covers the target window's outer rectangle and is redrawn with
//! `UpdateLayeredWindow` from a premultiplied BGRA buffer. Topmost is
//! re-asserted on every update: a submenu opened after the indicator can
//! otherwise cover it.
//!
//! Drawing is a pure function ([`render`]) so it's unit-tested; digits
//! come from a tiny built-in 5×7 bitmap font scaled by the monitor DPI,
//! which avoids text-rendering machinery for nine glyphs.

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::model::Bounds;
use crate::scroll::Area;

/// Outline colour (opaque RGB) and alpha.
const OUTLINE_RGB: u32 = 0x3B82F6;
const OUTLINE_ALPHA: u8 = 230;
/// Badge background and digit colours.
const BADGE_RGB: u32 = 0x1F2937;
const BADGE_ALPHA: u8 = 220;
const DIGIT_RGB: u32 = 0xFFFFFF;

/// 5×7 glyphs for `1`–`9`, one row per byte (bit 4 = leftmost column).
const GLYPHS: [[u8; 7]; 9] = [
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ], // 1
    [
        0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
    ], // 2
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ], // 3
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ], // 4
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ], // 5
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ], // 6
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ], // 7
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ], // 8
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ], // 9
];

/// Premultiplied BGRA pixel (as stored little-endian in the DIB).
fn px(rgb: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let r = ((rgb >> 16) & 0xFF) * a / 255;
    let g = ((rgb >> 8) & 0xFF) * a / 255;
    let b = (rgb & 0xFF) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

struct Canvas<'a> {
    buf: &'a mut [u32],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        for yy in y0..y1 {
            let row = (yy * self.w) as usize;
            for xx in x0..x1 {
                self.buf[row + xx as usize] = color;
            }
        }
    }

    fn outline(&mut self, b: Bounds, t: i32, color: u32) {
        self.fill(b.x, b.y, b.width, t, color);
        self.fill(b.x, b.y + b.height - t, b.width, t, color);
        self.fill(b.x, b.y + t, t, b.height - 2 * t, color);
        self.fill(b.x + b.width - t, b.y + t, t, b.height - 2 * t, color);
    }

    fn badge(&mut self, x: i32, y: i32, digit: usize, scale: i32) {
        let pad = 2 * scale;
        let (gw, gh) = (5 * scale, 7 * scale);
        self.fill(x, y, gw + 2 * pad, gh + 2 * pad, px(BADGE_RGB, BADGE_ALPHA));
        let Some(glyph) = digit.checked_sub(1).and_then(|i| GLYPHS.get(i)) else {
            return;
        };
        let ink = px(DIGIT_RGB, 255);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    self.fill(
                        x + pad + col * scale,
                        y + pad + row as i32 * scale,
                        scale,
                        scale,
                        ink,
                    );
                }
            }
        }
    }
}

/// Draw the indicator for a `frame` (screen rectangle the window covers):
/// an outline on `areas[active]`, a numbered badge on each other area.
/// `dpi` scales line widths and badges. Returns `frame.width ×
/// frame.height` premultiplied BGRA pixels, top row first.
pub fn render(frame: Bounds, areas: &[Area], active: usize, dpi: u32) -> Vec<u32> {
    let (w, h) = (frame.width.max(0), frame.height.max(0));
    let mut buf = vec![0u32; (w as usize) * (h as usize)];
    let mut c = Canvas {
        buf: &mut buf,
        w,
        h,
    };
    let scale = ((dpi.max(96) as f64 / 96.0) * 2.0).round().max(2.0) as i32;
    let local = |b: Bounds| Bounds {
        x: b.x - frame.x,
        y: b.y - frame.y,
        ..b
    };
    for (i, a) in areas.iter().enumerate() {
        let b = local(a.bounds);
        if i == active {
            c.outline(b, (scale * 3 / 2).max(3), px(OUTLINE_RGB, OUTLINE_ALPHA));
        } else {
            c.badge(b.x + 3 * scale, b.y + 3 * scale, i + 1, scale);
        }
    }
    buf
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// The indicator window. Create on the thread that pumps messages (the
/// main thread); dropping it destroys the window.
pub struct Indicator {
    hwnd: HWND,
}

impl Indicator {
    /// Create the (hidden) indicator window.
    pub fn new() -> Result<Self> {
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None)
                .context("GetModuleHandleW for scroll indicator")?
                .into();
            let class = w!("jumpkey-scroll-indicator");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinst,
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW,
                class,
                None,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                hinst,
                None,
            )
            .context("creating scroll indicator window")?;
            Ok(Self { hwnd })
        }
    }

    /// Draw over `frame` (the target window's outer rectangle). The DPI
    /// comes from `target`.
    pub fn show(&self, target: HWND, frame: Bounds, areas: &[Area], active: usize) {
        if frame.width <= 0 || frame.height <= 0 {
            return;
        }
        let dpi = unsafe { GetDpiForWindow(target) };
        let pixels = render(frame, areas, active, if dpi == 0 { 96 } else { dpi });
        unsafe {
            self.push(frame, &pixels);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    unsafe fn push(&self, frame: Bounds, pixels: &[u32]) {
        let mem_dc = CreateCompatibleDC(HDC::default());
        if mem_dc.is_invalid() {
            return;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: frame.width,
                biHeight: -frame.height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(mem_dc);
            return;
        };
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
        }
        let old = SelectObject(mem_dc, dib);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT {
            x: frame.x,
            y: frame.y,
        };
        let size = SIZE {
            cx: frame.width,
            cy: frame.height,
        };
        let src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            self.hwnd,
            HDC::default(),
            Some(&dst),
            Some(&size),
            mem_dc,
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem_dc, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
    }

    /// Hide the window (kept for the next session).
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: i32, y: i32, w: i32, h: i32) -> Area {
        Area {
            bounds: Bounds {
                x,
                y,
                width: w,
                height: h,
            },
            can_h: false,
            can_v: true,
        }
    }

    const FRAME: Bounds = Bounds {
        x: 100,
        y: 50,
        width: 400,
        height: 300,
    };

    fn at(buf: &[u32], x: i32, y: i32) -> u32 {
        buf[(y * FRAME.width + x) as usize]
    }

    #[test]
    fn active_area_gets_an_outline_and_a_clear_middle() {
        let areas = [area(100, 50, 400, 300)];
        let buf = render(FRAME, &areas, 0, 96);
        assert_eq!(buf.len(), 400 * 300);
        assert_eq!(at(&buf, 0, 0), px(OUTLINE_RGB, OUTLINE_ALPHA), "corner");
        assert_eq!(at(&buf, 399, 299), px(OUTLINE_RGB, OUTLINE_ALPHA), "corner");
        assert_eq!(at(&buf, 200, 150), 0, "middle stays transparent");
    }

    #[test]
    fn other_areas_get_numbered_badges() {
        let areas = [area(100, 50, 400, 300), area(300, 150, 150, 150)];
        let buf = render(FRAME, &areas, 0, 96);
        // Badge for area 2 sits at its top-left inset (scale 2 at 96 dpi).
        let (bx, by) = (200 + 6, 100 + 6);
        assert_eq!(at(&buf, bx, by), px(BADGE_RGB, BADGE_ALPHA));
        let ink = px(DIGIT_RGB, 255);
        let badge = &buf[(by * 400) as usize..((by + 22) * 400) as usize];
        assert!(badge.contains(&ink), "digit drawn");
    }

    #[test]
    fn areas_outside_the_frame_are_clipped_not_panicking() {
        let areas = [area(-500, -500, 2000, 2000), area(450, 300, 300, 300)];
        let buf = render(FRAME, &areas, 0, 144);
        assert_eq!(buf.len(), 400 * 300);
    }

    #[test]
    fn premultiplied_pixels() {
        assert_eq!(px(0xFFFFFF, 255), 0xFFFF_FFFF);
        assert_eq!(px(0xFF0000, 0), 0);
        assert_eq!(px(0xFF0000, 128) >> 24, 128);
        assert_eq!((px(0xFF0000, 128) >> 16) & 0xFF, 128);
    }
}
