//! Win32 plumbing for the harness. Nothing here knows about jumpkey's
//! internals; see `bench/README.md` for the contract it relies on.

pub mod clock;
pub mod events;
pub mod input;
pub mod machine;
pub mod process;
pub mod window;

/// Make the harness per-monitor DPI aware so window rects and monitor
/// sizes are in physical pixels, matching what jumpkey sees.
pub fn init_dpi() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Local time as `YYYYMMDD-HHMMSS`, for run ids.
pub fn local_timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
