//! Watching jumpkey's windows appear and disappear, from outside.
//!
//! The harness never looks for a particular window class or count: per
//! the bench contract, "overlay visible" means *any* top-level window
//! of the jumpkey process raising `EVENT_OBJECT_SHOW`. That keeps working
//! if the overlay becomes one window per monitor.
//!
//! The hook is out-of-context, so Windows queues the callback onto this
//! thread; every wait in the harness pumps messages (see [`pump_until`])
//! so callbacks run promptly. The QPC timestamp is taken inside the
//! callback, so it includes the (sub-millisecond) delivery delay.

use std::cell::RefCell;
use std::time::Duration;

use anyhow::{bail, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetAncestor, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage,
    CHILDID_SELF, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW, GA_ROOT, MSG, MWMO_INPUTAVAILABLE,
    OBJID_WINDOW, PM_REMOVE, QS_ALLINPUT, WINEVENT_OUTOFCONTEXT,
};

use super::clock::{qpc, qpc_freq};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Show,
    Hide,
}

#[derive(Debug, Clone, Copy)]
pub struct WinEvent {
    pub kind: Kind,
    pub hwnd: isize,
    pub qpc: i64,
}

thread_local! {
    static QUEUE: RefCell<Vec<WinEvent>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let stamp = qpc();
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.0.is_null() {
        return;
    }
    // Top-level windows only: child controls of a dialog also raise
    // show/hide and would otherwise look like an overlay.
    if GetAncestor(hwnd, GA_ROOT) != hwnd {
        return;
    }
    let kind = match event {
        EVENT_OBJECT_SHOW => Kind::Show,
        EVENT_OBJECT_HIDE => Kind::Hide,
        _ => return,
    };
    QUEUE.with(|q| {
        q.borrow_mut().push(WinEvent {
            kind,
            hwnd: hwnd.0 as isize,
            qpc: stamp,
        })
    });
}

/// Show/hide watcher for one process. Unhooks on drop.
pub struct Watcher {
    hook: HWINEVENTHOOK,
    thread: u32,
}

impl Watcher {
    pub fn new(pid: u32) -> Result<Self> {
        // SAFETY: plain hook registration; the callback only touches a
        // thread-local and is unregistered in Drop on this same thread.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_SHOW,
                EVENT_OBJECT_HIDE,
                None,
                Some(on_event),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            bail!("SetWinEventHook failed for pid {pid}");
        }
        clear();
        Ok(Self {
            hook,
            thread: unsafe { GetCurrentThreadId() },
        })
    }

    /// Wait for the next show event (any jumpkey top-level window).
    pub fn wait_show(&self, timeout: Duration) -> Option<WinEvent> {
        self.wait_for(timeout, |e| e.kind == Kind::Show)
    }

    /// Wait for a hide event of a specific window.
    pub fn wait_hide(&self, hwnd: isize, timeout: Duration) -> Option<WinEvent> {
        self.wait_for(timeout, |e| e.kind == Kind::Hide && e.hwnd == hwnd)
    }

    fn wait_for(&self, timeout: Duration, pred: impl Fn(&WinEvent) -> bool) -> Option<WinEvent> {
        debug_assert_eq!(self.thread, unsafe { GetCurrentThreadId() });
        let mut found = None;
        pump_until(timeout, || {
            QUEUE.with(|q| {
                let mut q = q.borrow_mut();
                if let Some(pos) = q.iter().position(&pred) {
                    found = Some(q[pos]);
                    q.drain(..=pos);
                    true
                } else {
                    false
                }
            })
        });
        found
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: hook handle from SetWinEventHook on this thread.
        unsafe {
            let _ = UnhookWinEvent(self.hook);
        }
        clear();
    }
}

/// Drop any events queued so far (e.g. before pressing the hotkey, so
/// a stale show can't be mistaken for the overlay).
pub fn clear() {
    QUEUE.with(|q| q.borrow_mut().clear());
}

/// Pump this thread's messages until `done()` returns true or `timeout`
/// elapses. Returns whether `done` became true. Used for every wait in
/// the harness so WinEvent callbacks and our own window keep running.
///
/// `done()` is checked after every message and at least every 1 ms, so
/// cheap checks of external state (the foreground window, a queued
/// WinEvent) are timed precisely. Use [`pump_until_every`] for checks
/// that are expensive or don't need that precision.
pub fn pump_until(timeout: Duration, done: impl FnMut() -> bool) -> bool {
    pump_until_every(timeout, Duration::from_millis(1), done)
}

/// [`pump_until`], checking `done()` after every message and at least
/// every `poll` (e.g. a process snapshot or a directory delete every
/// 100 ms instead of every millisecond).
pub fn pump_until_every(timeout: Duration, poll: Duration, mut done: impl FnMut() -> bool) -> bool {
    let freq = qpc_freq() as f64;
    let deadline = qpc() + (timeout.as_secs_f64() * freq) as i64;
    let poll_ms = poll.as_millis().clamp(1, u32::MAX as u128) as u32;
    loop {
        drain_messages();
        if done() {
            return true;
        }
        let now = qpc();
        if now >= deadline {
            return false;
        }
        let remaining_ms = (((deadline - now) as f64 / freq) * 1000.0).ceil() as u32;
        // Sleep until a message arrives (WinEvent callbacks arrive as
        // messages), the next poll, or the deadline.
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                remaining_ms.clamp(1, poll_ms),
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}

/// Sleep while pumping messages. Wakes only for messages and the
/// deadline.
pub fn pump_sleep(d: Duration) {
    pump_until_every(d, d, || false);
}

fn drain_messages() {
    let mut msg = MSG::default();
    // SAFETY: standard message pump on the calling thread.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
