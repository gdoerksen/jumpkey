//! Finding, placing and focusing target windows, plus the harness's
//! own "neutral" window used to move focus away from the target.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows,
    GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, RegisterClassW,
    SetForegroundWindow, SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_OWNER, HWND_TOP, SWP_NOZORDER,
    SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
};

use super::events::{pump_sleep, pump_until};
use super::input::tap_alt;
use super::process::exe_name;

pub fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

pub fn raw(h: HWND) -> isize {
    h.0 as isize
}

pub fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: valid out-pointer; a stale HWND just yields pid 0.
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    pid
}

pub fn foreground() -> isize {
    raw(unsafe { GetForegroundWindow() })
}

pub fn is_window(raw_hwnd: isize) -> bool {
    unsafe { IsWindow(hwnd(raw_hwnd)).as_bool() }
}

/// Any visible top-level window of process `pid` (tool windows and
/// splash screens included).
pub fn visible_windows_of_pid(pid: u32) -> usize {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let (pid, count) = &mut *(lp.0 as *mut (u32, usize));
        if IsWindowVisible(h).as_bool() && window_pid(h) == *pid {
            *count += 1;
        }
        BOOL(1)
    }
    let mut state = (pid, 0usize);
    // SAFETY: the callback only runs during this call.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut state as *mut _ as isize));
    }
    state.1
}

/// Visible, unowned, non-tool top-level windows: the ones a user would
/// call "an app window".
pub fn app_windows() -> Vec<isize> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<isize>);
        if IsWindowVisible(h).as_bool()
            && GetWindow(h, GW_OWNER)
                .map(|o| o.0.is_null())
                .unwrap_or(true)
            && (GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) == 0
        {
            out.push(h.0 as isize);
        }
        BOOL(1)
    }
    let mut out: Vec<isize> = Vec::new();
    // SAFETY: the callback only runs during this call and writes to `out`.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// App windows whose process image is `exe` (case-insensitive basename).
/// Returned in z-order (topmost first), as `EnumWindows` delivers them.
pub fn app_windows_of_exe(exe: &str) -> Vec<isize> {
    app_windows()
        .into_iter()
        .filter(|&h| {
            exe_name(window_pid(hwnd(h)))
                .map(|n| n.eq_ignore_ascii_case(exe))
                .unwrap_or(false)
        })
        .collect()
}

/// Wait for a new app window of `exe` that wasn't in `before`.
pub fn wait_new_window(exe: &str, before: &HashSet<isize>, timeout: Duration) -> Option<isize> {
    let mut found = None;
    pump_until(timeout, || {
        found = app_windows_of_exe(exe)
            .into_iter()
            .find(|h| !before.contains(h));
        found.is_some()
    });
    found
}

/// Move and size a window, and return where it actually ended up.
///
/// Un-maximizes first: `SetWindowPos` on a maximized window doesn't
/// leave it at the requested rect. Applied twice because moving a window
/// onto a monitor with a different DPI makes DPI-aware apps (Chromium,
/// Electron) resize themselves for the new scale; the second call sets
/// the exact size on the destination monitor.
pub fn place(raw_hwnd: isize, rect: [i32; 4]) -> Option<[i32; 4]> {
    let h = hwnd(raw_hwnd);
    // SAFETY: plain window positioning; failures are harmless.
    unsafe {
        if IsIconic(h).as_bool() || IsZoomed(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
            pump_sleep(Duration::from_millis(200));
        }
        for _ in 0..2 {
            let _ = SetWindowPos(
                h,
                HWND_TOP,
                rect[0],
                rect[1],
                rect[2],
                rect[3],
                SWP_NOZORDER,
            );
            pump_sleep(Duration::from_millis(250));
        }
    }
    self::rect(raw_hwnd)
}

/// Effective DPI of the monitor the window is on.
pub fn dpi(raw_hwnd: isize) -> u32 {
    unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd(raw_hwnd)) }
}

pub fn rect(raw_hwnd: isize) -> Option<[i32; 4]> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd(raw_hwnd), &mut r).ok()? };
    Some([r.left, r.top, r.right - r.left, r.bottom - r.top])
}

/// Bring a window to the foreground and confirm it got there.
///
/// Windows restricts `SetForegroundWindow` to the process that received
/// the last input. A synthesized Alt tap makes that us. When the
/// harness's own neutral window is in front we already qualify.
pub fn focus(raw_hwnd: isize) -> Result<()> {
    let h = hwnd(raw_hwnd);
    for attempt in 0..5 {
        if foreground() == raw_hwnd {
            return Ok(());
        }
        if attempt > 0 {
            let _ = tap_alt();
        }
        // SAFETY: plain focus calls.
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            let _ = BringWindowToTop(h);
            let _ = SetForegroundWindow(h);
        }
        if pump_until(Duration::from_millis(300), || foreground() == raw_hwnd) {
            return Ok(());
        }
    }
    bail!(
        "couldn't bring window {raw_hwnd:#x} to the foreground (foreground is {:#x})",
        foreground()
    )
}

/// Ask a window to close (WM_CLOSE), as clicking its X would.
pub fn close(raw_hwnd: isize) {
    unsafe {
        let _ = PostMessageW(hwnd(raw_hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// A plain, empty top-level window owned by the harness. Focusing it
/// moves the foreground away from the target app (as switching apps
/// would), and makes the harness the foreground process so it's
/// allowed to hand focus to the target next.
pub struct NeutralWindow {
    hwnd: isize,
}

unsafe extern "system" fn neutral_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

impl NeutralWindow {
    pub fn create(rect: [i32; 4]) -> Result<Self> {
        let class = w!("pick-bench-neutral");
        // SAFETY: standard class registration + window creation.
        unsafe {
            let hinstance = GetModuleHandleW(PCWSTR::null())?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(neutral_proc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let h = CreateWindowExW(
                Default::default(),
                class,
                w!("pick-bench (neutral window)"),
                WS_OVERLAPPEDWINDOW,
                rect[0],
                rect[1],
                rect[2],
                rect[3],
                None,
                None,
                hinstance,
                None,
            )
            .context("creating neutral window")?;
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            pump_sleep(Duration::from_millis(50));
            Ok(Self { hwnd: raw(h) })
        }
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }
}

impl Drop for NeutralWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(hwnd(self.hwnd));
        }
    }
}
