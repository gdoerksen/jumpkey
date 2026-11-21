//! Clocks and process counters for telemetry.
//!
//! QPC timestamps are system-wide, so a `start_qpc` written here can be
//! lined up with the benchmark harness's own QPC readings
//! (`tools/pick-bench`). CPU is measured in time-stamp-counter cycles
//! (`QueryThreadCycleTime` / `QueryProcessCycleTime`); the TSC rate is
//! calibrated once against QPC on a background thread so cycles can be
//! shown as microseconds. The harness has its own copy of this logic on
//! purpose: it must not depend on the app.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetCurrentThreadId};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

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

/// Signed microseconds from `from` to `to` (QPC ticks).
pub fn qpc_us(from: i64, to: i64) -> i64 {
    ((to - from) as i128 * 1_000_000 / qpc_freq() as i128) as i64
}

/// Cycles charged to the calling thread so far.
pub fn thread_cycles() -> u64 {
    let mut c = 0u64;
    // SAFETY: pseudo-handle for the current thread; valid out-pointer.
    unsafe {
        let _ = QueryThreadCycleTime(GetCurrentThread(), &mut c);
    }
    c
}

/// Cycles charged to this process so far (all threads).
pub fn process_cycles() -> u64 {
    let mut c = 0u64;
    // SAFETY: pseudo-handle for the current process; valid out-pointer.
    unsafe {
        let _ = QueryProcessCycleTime(GetCurrentProcess(), &mut c);
    }
    c
}

/// This process's private bytes (commit charge).
pub fn private_bytes() -> u64 {
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the struct is sized correctly and `cb` says so.
    unsafe {
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut _, m.cb).is_err() {
            return 0;
        }
    }
    m.PrivateUsage as u64
}

/// Win32 id of the calling thread.
pub fn thread_id() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetCurrentThreadId() }
}

static TSC_HZ: OnceLock<Option<f64>> = OnceLock::new();

/// Start calibrating the TSC rate on a background thread (takes 50 ms).
/// Until it finishes, [`tsc_hz`] returns `None` and records carry
/// cycles only.
pub fn calibrate_in_background() {
    if TSC_HZ.get().is_some() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("telemetry-tsc".into())
        .spawn(|| {
            let _ = TSC_HZ.set(calibrate());
        });
}

/// Calibrate synchronously (tests).
#[cfg(test)]
pub fn calibrate_now() {
    let _ = TSC_HZ.set(calibrate());
}

/// TSC cycles per second, once calibrated.
pub fn tsc_hz() -> Option<f64> {
    TSC_HZ.get().copied().flatten()
}

/// Convert a cycle count to microseconds, once calibrated.
pub fn cycles_to_us(cycles: u64) -> Option<f64> {
    tsc_hz().map(|hz| cycles as f64 * 1_000_000.0 / hz)
}

#[cfg(target_arch = "x86_64")]
fn calibrate() -> Option<f64> {
    use std::arch::x86_64::_rdtsc;
    let q0 = qpc();
    // SAFETY: RDTSC exists on every x86-64 CPU.
    let t0 = unsafe { _rdtsc() };
    std::thread::sleep(std::time::Duration::from_millis(50));
    let q1 = qpc();
    let t1 = unsafe { _rdtsc() };
    let secs = (q1 - q0) as f64 / qpc_freq() as f64;
    (secs > 0.0).then(|| (t1 - t0) as f64 / secs)
}

#[cfg(not(target_arch = "x86_64"))]
fn calibrate() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_us_is_signed() {
        let f = qpc_freq();
        assert_eq!(qpc_us(0, f), 1_000_000);
        assert_eq!(qpc_us(f, 0), -1_000_000);
    }

    #[test]
    fn calibrated_tsc_is_plausible() {
        calibrate_now();
        if let Some(hz) = tsc_hz() {
            // Any CPU from the last 20 years: 0.5–10 GHz.
            assert!((5e8..1e10).contains(&hz), "tsc_hz = {hz}");
            let us = cycles_to_us(hz as u64).unwrap();
            assert!((us - 1_000_000.0).abs() < 1.0);
        }
    }

    #[test]
    fn thread_cycles_increase_with_work() {
        let a = thread_cycles();
        let mut x = 0u64;
        for i in 0..1_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        assert!(thread_cycles() > a);
        assert!(private_bytes() > 0);
        assert!(process_cycles() > 0);
    }
}
