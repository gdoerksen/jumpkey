//! High-resolution timestamps.
//!
//! `QueryPerformanceCounter` values are system-wide, so a QPC reading
//! taken in this process can be compared with one taken inside
//! jumpkey (the in-app telemetry records raw QPC too). That's what lets
//! the harness measure hotkey → overlay visible across two processes.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Raw QPC ticks.
pub fn qpc() -> i64 {
    let mut v = 0i64;
    // SAFETY: valid out-pointer; QPC can't fail on XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// QPC ticks per second.
pub fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: valid out-pointer.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

/// Microseconds between two QPC readings.
pub fn qpc_delta_us(from: i64, to: i64) -> u64 {
    let d = (to - from).max(0) as i128;
    (d * 1_000_000 / qpc_freq() as i128) as u64
}

/// Rate of the counter behind `QueryProcessCycleTime` /
/// `QueryThreadCycleTime`, in cycles per second.
///
/// On x86-64 Windows those APIs report time-stamp-counter ticks, which
/// run at a fixed rate on every CPU from the last decade (invariant
/// TSC), regardless of turbo or power-saving clock changes. Measuring
/// that rate against QPC once lets us turn cycle deltas into
/// approximate CPU milliseconds. `None` where we can't read the TSC.
pub fn tsc_hz() -> Option<f64> {
    static HZ: OnceLock<Option<f64>> = OnceLock::new();
    *HZ.get_or_init(calibrate_tsc)
}

#[cfg(target_arch = "x86_64")]
fn calibrate_tsc() -> Option<f64> {
    use std::arch::x86_64::_rdtsc;
    // 200 ms is plenty for 4+ significant digits and only runs once.
    let q0 = qpc();
    // SAFETY: RDTSC is available on every x86-64 CPU.
    let t0 = unsafe { _rdtsc() };
    std::thread::sleep(std::time::Duration::from_millis(200));
    let q1 = qpc();
    let t1 = unsafe { _rdtsc() };
    let secs = (q1 - q0) as f64 / qpc_freq() as f64;
    if secs <= 0.0 {
        return None;
    }
    Some((t1 - t0) as f64 / secs)
}

#[cfg(not(target_arch = "x86_64"))]
fn calibrate_tsc() -> Option<f64> {
    None
}

/// Convert a cycle delta to approximate CPU milliseconds.
pub fn cycles_to_ms(cycles: u64) -> Option<f64> {
    tsc_hz().map(|hz| cycles as f64 * 1000.0 / hz)
}
