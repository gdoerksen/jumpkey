//! The `pick` root span and the real-usage timing recorded on it.
//!
//! Besides timing the pick itself, each `pick` record answers "how do I
//! actually use the picker?":
//!
//! - `since_foreground_change_ms`: how long the target window had been
//!   in front when the hotkey arrived (the 500 ms cache only helps if
//!   this is often short).
//! - `since_last_pick_ms` and `same_window_as_last_pick`: repeat presses.
//! - `queue_ms`: how long `WM_HOTKEY` sat in the queue before the main
//!   loop picked it up.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};

use tracing::field::Empty;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, CHILDID_SELF, EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_HOTKEY,
};

use super::clock::{private_bytes, process_cycles, qpc, qpc_us};
use super::{layer, machine, SessionInfo, TARGET};

/// HWND of the last foreground window seen by the hook.
static LAST_FG_HWND: AtomicIsize = AtomicIsize::new(0);
/// QPC time it came to the front; 0 = not seen since startup.
static LAST_FG_QPC: AtomicI64 = AtomicI64::new(0);

thread_local! {
    /// `(qpc, queue_ms)` of the `WM_HOTKEY` the main loop is handling.
    static HOTKEY: Cell<Option<(i64, u32)>> = const { Cell::new(None) };
    /// `(characters, backspaces)` typed into the current overlay.
    static KEYS: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    /// `(end qpc, foreground hwnd)` of the previous pick.
    static LAST_PICK: Cell<Option<(i64, isize)>> = const { Cell::new(None) };
}

/// Called by the main loop for every message, before dispatch. Stashes
/// the arrival time of a `WM_HOTKEY` so the pick it triggers can record
/// its queue time; any other message clears the stash, so a stale one
/// can't be attributed to a later pick.
pub fn note_message(message: u32, time: u32) {
    let v = (message == WM_HOTKEY).then(|| {
        // SAFETY: no preconditions.
        let now_tick = unsafe { GetTickCount() };
        (qpc(), now_tick.wrapping_sub(time))
    });
    HOTKEY.with(|h| h.set(v));
}

/// A character typed into the overlay.
pub fn note_key() {
    KEYS.with(|k| {
        let (c, b) = k.get();
        k.set((c + 1, b));
    });
}

/// Backspace pressed in the overlay.
pub fn note_backspace() {
    KEYS.with(|k| {
        let (c, b) = k.get();
        k.set((c, b + 1));
    });
}

/// Root of one picker trace: created when the hotkey is handled,
/// written when dropped. Main thread only.
pub struct Pick {
    span: Option<tracing::span::EnteredSpan>,
    foreground: isize,
    cycles_before: u64,
    outcome_set: Cell<bool>,
}

impl Pick {
    /// Start the trace for one hotkey press. `kind` is `element` or
    /// `window`.
    pub fn begin(kind: &'static str, scope: &dyn std::fmt::Debug) -> Self {
        let now = qpc();
        let cycles_before = process_cycles();
        // SAFETY: no preconditions.
        let foreground = unsafe { GetForegroundWindow() }.0 as isize;
        let (hotkey_qpc, queue_ms, hotkey_source) = match HOTKEY.with(|h| h.take()) {
            Some((q, ms)) => (q, Some(ms), "message"),
            None => (now, None, "dispatch"),
        };
        let fg_qpc = LAST_FG_QPC.load(Ordering::Relaxed);
        let since_fg = (fg_qpc != 0).then(|| qpc_us(fg_qpc, hotkey_qpc) / 1000);
        let last = LAST_PICK.with(|l| l.get());
        let since_last = last.map(|(end, _)| qpc_us(end, hotkey_qpc) / 1000);
        let same_window = last.map(|(_, hwnd)| hwnd == foreground);
        KEYS.with(|k| k.set((0, 0)));

        let span = tracing::info_span!(
            target: TARGET,
            "pick",
            kind,
            scope = ?scope,
            count = Empty,
            outcome = Empty,
            modifier = Empty,
            keystrokes = Empty,
            backspaces = Empty,
            hotkey_source,
            queue_ms,
            since_foreground_change_ms = since_fg,
            since_last_pick_ms = since_last,
            same_window_as_last_pick = same_window,
            process_cpu_cycles = Empty,
            private_bytes_before = private_bytes(),
            private_bytes_after = Empty,
        )
        .entered();
        tracing::info!(target: TARGET, milestone = "hotkey", at_qpc = hotkey_qpc);
        Self {
            span: Some(span),
            foreground,
            cycles_before,
            outcome_set: Cell::new(false),
        }
    }

    fn record(&self, field: &str, value: impl tracing::Value) {
        if let Some(s) = &self.span {
            s.record(field, value);
        }
    }

    /// Number of hints shown.
    pub fn count(&self, n: usize) {
        self.record("count", n as u64);
    }

    /// How the pick ended: `picked`, `cancelled` or `empty`. A pick
    /// dropped without one (an error path) records `error`.
    pub fn outcome(&self, outcome: &'static str) {
        self.outcome_set.set(true);
        self.record("outcome", outcome);
    }

    /// Modifier held on the completing keystroke.
    pub fn modifier(&self, modifier: &'static str) {
        self.record("modifier", modifier);
    }
}

impl Drop for Pick {
    fn drop(&mut self) {
        if !self.outcome_set.get() {
            self.record("outcome", "error");
        }
        let (keys, backspaces) = KEYS.with(|k| k.get());
        self.record("keystrokes", keys);
        self.record("backspaces", backspaces);
        self.record(
            "process_cpu_cycles",
            process_cycles().saturating_sub(self.cycles_before),
        );
        self.record("private_bytes_after", private_bytes());
        LAST_PICK.with(|l| l.set(Some((qpc(), self.foreground))));
        // Closing the root makes the layer write the trace.
        drop(self.span.take());
        if let Some(summary) = layer::take_summary() {
            // Explicit target: this module's default target
            // (`jumpkey::telemetry::pick`) is filtered out of the log
            // along with the telemetry spans.
            tracing::info!(target: "jumpkey", "{summary}");
        }
    }
}

/// Handle from [`start`]: unhooks the foreground tracker when dropped.
pub struct Started {
    hook: Option<HWINEVENTHOOK>,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(h) = self.hook.take() {
            // SAFETY: hook handle from SetWinEventHook on this thread.
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}

/// Start foreground tracking (call on the main thread, which pumps
/// messages) and write the `session` record from a background thread.
pub fn start(info: SessionInfo) -> Started {
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() }.0 as isize;
    LAST_FG_HWND.store(fg, Ordering::Relaxed);
    // SAFETY: out-of-context hook with a valid callback; the callback
    // only touches atomics.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    let hook = (!hook.is_invalid()).then_some(hook);
    if hook.is_none() {
        tracing::warn!("telemetry: foreground hook failed; since_foreground_change_ms unavailable");
    }

    if let Some(shared) = layer::global() {
        let shared = shared.clone();
        let _ = std::thread::Builder::new()
            .name("telemetry-session".into())
            .spawn(move || {
                let rec = machine::session_record(shared.session(), &info);
                shared.write_line(&rec);
            });
    }
    Started { hook }
}

/// Foreground-change hook. Only counts a change when the window differs
/// from the last one seen: when the overlay closes, Windows hands focus
/// back to the app it covered, and that event (from the app's process,
/// so not filtered by `WINEVENT_SKIPOWNPROCESS`) must not reset the
/// clock.
unsafe extern "system" fn foreground_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.0.is_null() {
        return;
    }
    record_foreground(hwnd.0 as isize, qpc());
}

/// Serializes tests that touch the foreground statics.
#[cfg(test)]
pub(super) static TEST_FG_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub(super) fn record_foreground(hwnd: isize, at: i64) {
    if LAST_FG_HWND.swap(hwnd, Ordering::Relaxed) != hwnd {
        LAST_FG_QPC.store(at, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::layer::tests::capture;

    #[test]
    fn foreground_return_to_same_window_keeps_the_clock() {
        let _lock = TEST_FG_LOCK.lock();
        record_foreground(0x1111, 100);
        record_foreground(0x2222, 200);
        assert_eq!(LAST_FG_QPC.load(Ordering::Relaxed), 200);
        // Overlay closes, focus returns to the same window: not a switch.
        record_foreground(0x2222, 300);
        assert_eq!(LAST_FG_QPC.load(Ordering::Relaxed), 200);
        record_foreground(0x1111, 400);
        assert_eq!(LAST_FG_QPC.load(Ordering::Relaxed), 400);
    }

    #[test]
    fn pick_records_usage_and_keys() {
        let lines = capture(|| {
            note_message(WM_HOTKEY, unsafe { GetTickCount() });
            {
                let p = Pick::begin("element", &"ActiveWindow");
                note_key();
                note_key();
                note_backspace();
                p.count(12);
                p.outcome("picked");
            }
            // A non-hotkey message clears the stash.
            note_message(0, 0);
            let _p = Pick::begin("window", &"ActiveWindow");
            // Dropped without an outcome → "error".
        });
        assert_eq!(lines.len(), 2);
        let a = &lines[0]["attributes"];
        assert_eq!(a["kind"], "element");
        assert_eq!(a["count"], 12);
        assert_eq!(a["outcome"], "picked");
        assert_eq!(a["keystrokes"], 2);
        assert_eq!(a["backspaces"], 1);
        assert_eq!(a["hotkey_source"], "message");
        assert!(a["queue_ms"].is_u64());
        assert!(a.get("since_last_pick_ms").is_none() || a["since_last_pick_ms"].is_i64());
        assert_eq!(lines[0]["events"][0]["name"], "hotkey");

        let b = &lines[1]["attributes"];
        assert_eq!(b["kind"], "window");
        assert_eq!(b["outcome"], "error");
        assert_eq!(b["hotkey_source"], "dispatch");
        assert!(b.get("queue_ms").is_none());
        assert!(b["since_last_pick_ms"].is_i64());
        assert_eq!(b["same_window_as_last_pick"], true);
        assert_eq!(b["keystrokes"], 0);
    }
}
