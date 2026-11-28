//! Responsiveness probe: how long jumpkey's main thread goes without
//! handling messages during a press.
//!
//! A background thread sends `WM_NULL` to jumpkey's hidden IPC window
//! (the window `jumpkey --close` already finds by class, bench contract
//! item 1) every [`INTERVAL`] and times each round trip. A sent message
//! is handled only when the owning thread pumps messages, so the slowest
//! round trip is the longest stretch the main loop was blocked: today a
//! UI Automation search on the main thread, after step 6 only the work
//! the main thread still does itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SendMessageTimeoutW, SMTO_NORMAL, WM_NULL,
};

use super::clock::{qpc, qpc_delta_us};
use super::window::{hwnd, raw, window_pid};

/// Gap between probes. Short enough to catch stalls of a few ms, rare
/// enough that handling them costs jumpkey nothing measurable.
const INTERVAL: Duration = Duration::from_millis(5);
/// A probe that takes longer than this is recorded at this value; a
/// main thread blocked this long is a timeout anyway.
const SEND_TIMEOUT_MS: u32 = 10_000;

/// What one press's probe saw.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeResult {
    /// Slowest `WM_NULL` round trip, µs.
    pub max_us: u64,
    /// Probes completed.
    pub count: u32,
}

/// A running probe; [`Probe::finish`] stops it and returns the result.
pub struct Probe {
    stop: Arc<AtomicBool>,
    join: JoinHandle<ProbeResult>,
}

/// jumpkey's IPC window, if it exists and belongs to `pid`.
pub fn ipc_window(pid: u32) -> Option<isize> {
    // SAFETY: plain window lookup by class.
    let h = unsafe { FindWindowW(w!("JumpkeyIpcWindowClass_v1"), None) }.ok()?;
    (!h.0.is_null() && window_pid(h) == pid).then(|| raw(h))
}

impl Probe {
    /// Start probing jumpkey process `pid`. `None` if its IPC window
    /// can't be found (an old build, or it failed to create one).
    pub fn start(pid: u32) -> Option<Self> {
        let target = ipc_window(pid)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let join = std::thread::Builder::new()
            .name("pick-bench-probe".into())
            .spawn(move || run(target, &flag))
            .ok()?;
        Some(Self { stop, join })
    }

    pub fn finish(self) -> ProbeResult {
        self.stop.store(true, Ordering::SeqCst);
        self.join.join().unwrap_or_default()
    }
}

fn run(target: isize, stop: &AtomicBool) -> ProbeResult {
    let h: HWND = hwnd(target);
    let mut r = ProbeResult::default();
    while !stop.load(Ordering::SeqCst) {
        let t0 = qpc();
        let mut result = 0usize;
        // SAFETY: WM_NULL has no parameters; the result pointer is valid.
        let ok = unsafe {
            SendMessageTimeoutW(
                h,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_NORMAL,
                SEND_TIMEOUT_MS,
                Some(&mut result),
            )
        };
        let dt = qpc_delta_us(t0, qpc());
        if ok.0 == 0 && dt < u64::from(SEND_TIMEOUT_MS) * 1000 {
            // The window is gone (jumpkey exited): nothing more to see.
            break;
        }
        r.max_us = r.max_us.max(dt);
        r.count += 1;
        std::thread::sleep(INTERVAL);
    }
    r
}
