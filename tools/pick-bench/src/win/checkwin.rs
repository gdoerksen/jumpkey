//! A small Win32 window owned by the harness, for `check` cases that
//! need a classic Win32 target: a menu bar (lone-Alt check) and a popup
//! menu that must stay open while jumpkey's labels show (menu check).
//!
//! It runs on its own thread because `TrackPopupMenu` runs a modal loop
//! until the menu closes, and the harness's main thread must keep
//! pumping its WinEvent hook meanwhile.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EnumWindows, GetClassNameW, GetClientRect, GetGUIThreadInfo, GetMessageW,
    IsWindowVisible, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    ShowWindow, TrackPopupMenu, TranslateMessage, BS_PUSHBUTTON, GUITHREADINFO, GUI_INMENUMODE,
    HMENU, MF_POPUP, MF_STRING, MSG, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};

use super::window::{hwnd, raw, window_pid};

/// Posted to the window: open the popup menu (modal until it closes).
const WM_OPEN_POPUP: u32 = WM_APP + 1;

pub struct CheckWindow {
    hwnd: isize,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

unsafe extern "system" fn proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_OPEN_POPUP => {
            let popup = CreatePopupMenu().unwrap_or_default();
            for (i, text) in [w!("Alpha"), w!("Bravo"), w!("Charlie"), w!("Delta")]
                .into_iter()
                .enumerate()
            {
                let _ = AppendMenuW(popup, MF_STRING, 100 + i, text);
            }
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            let mut pt = windows::Win32::Foundation::POINT {
                x: r.right / 2,
                y: r.bottom / 2,
            };
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(h, &mut pt);
            // Required for the menu to close normally when it loses focus.
            let _ = SetForegroundWindow(h);
            let _ = TrackPopupMenu(
                popup,
                TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                h,
                None,
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(popup);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}

unsafe fn menu_bar() -> HMENU {
    let bar = CreateMenu().unwrap_or_default();
    for (title, items) in [
        (w!("File"), [w!("Open"), w!("Save")]),
        (w!("Edit"), [w!("Copy"), w!("Paste")]),
    ] {
        let sub = CreatePopupMenu().unwrap_or_default();
        for (i, text) in items.into_iter().enumerate() {
            let _ = AppendMenuW(sub, MF_STRING, 200 + i, text);
        }
        let _ = AppendMenuW(bar, MF_POPUP, sub.0 as usize, title);
    }
    bar
}

impl CheckWindow {
    pub fn create(rect: [i32; 4]) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Result<(isize, u32)>>();
        let join = std::thread::Builder::new()
            .name("pick-bench-checkwin".into())
            .spawn(move || {
                // SAFETY: standard window creation and message loop on this thread.
                unsafe {
                    let made = (|| -> Result<isize> {
                        let class = w!("pick-bench-check");
                        let hinstance = GetModuleHandleW(PCWSTR::null())?;
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(proc),
                            hInstance: hinstance.into(),
                            lpszClassName: class,
                            ..Default::default()
                        };
                        RegisterClassW(&wc);
                        let h = CreateWindowExW(
                            Default::default(),
                            class,
                            w!("pick-bench check window"),
                            WS_OVERLAPPEDWINDOW,
                            rect[0],
                            rect[1],
                            rect[2],
                            rect[3],
                            None,
                            menu_bar(),
                            hinstance,
                            None,
                        )
                        .context("creating the check window")?;
                        for (i, text) in [w!("One"), w!("Two"), w!("Three"), w!("Four")]
                            .into_iter()
                            .enumerate()
                        {
                            let _ = CreateWindowExW(
                                Default::default(),
                                w!("BUTTON"),
                                text,
                                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                                20 + 130 * i as i32,
                                20,
                                110,
                                36,
                                h,
                                HMENU((300 + i) as *mut core::ffi::c_void),
                                hinstance,
                                None,
                            );
                        }
                        let _ = ShowWindow(h, SW_SHOWNORMAL);
                        Ok(raw(h))
                    })();
                    let ok = made.is_ok();
                    let _ = tx.send(made.map(|h| (h, GetCurrentThreadId())));
                    if !ok {
                        return;
                    }
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })?;
        let (hwnd, thread_id) = rx
            .recv_timeout(Duration::from_secs(5))
            .context("check window thread didn't start")??;
        Ok(Self {
            hwnd,
            thread_id,
            join: Some(join),
        })
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Open the popup menu (asynchronously; it stays open until closed).
    pub fn open_popup(&self) {
        unsafe {
            let _ = PostMessageW(hwnd(self.hwnd), WM_OPEN_POPUP, WPARAM(0), LPARAM(0));
        }
    }

    /// Whether a popup menu (`#32768`) of this process is visible.
    pub fn popup_open(&self) -> bool {
        unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
            let found = &mut *(lp.0 as *mut bool);
            if IsWindowVisible(h).as_bool() && window_pid(h) == std::process::id() {
                let mut buf = [0u16; 16];
                let n = GetClassNameW(h, &mut buf) as usize;
                if String::from_utf16_lossy(&buf[..n]) == "#32768" {
                    *found = true;
                    return BOOL(0);
                }
            }
            BOOL(1)
        }
        let mut found = false;
        // SAFETY: the callback only runs during this call.
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut found as *mut _ as isize));
        }
        found
    }

    /// Whether the window's thread is in a menu loop (menu bar activated
    /// by Alt, or a popup menu open).
    pub fn in_menu_mode(&self) -> bool {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetGUIThreadInfo(self.thread_id, &mut info).is_ok()
                && (info.flags.0 & GUI_INMENUMODE.0) != 0
        }
    }
}

impl Drop for CheckWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(hwnd(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
