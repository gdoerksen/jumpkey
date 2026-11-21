//! The `session` record: build, machine and config facts, written once
//! at startup so picks from different builds, machines, power settings
//! or configs aren't compared by accident. Same fields as the benchmark
//! harness's run manifest (`tools/pick-bench/src/win/machine.rs`; kept
//! as a separate copy because the harness mustn't depend on the app).
//! The computer name is stored only as a hash.

use serde_json::{json, Value};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{BOOL, HLOCAL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
    MONITORINFOEXW,
};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, PowerReadFriendlyName, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use super::clock::{qpc_freq, tsc_hz};
use super::{SessionInfo, SCHEMA};

/// Build the `session` record.
pub fn session_record(session: &str, info: &SessionInfo) -> Value {
    // TSC calibration runs in the background for 50 ms after startup;
    // give it a moment so the session carries the rate.
    for _ in 0..20 {
        if tsc_hz().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let start_unix_nano = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    json!({
        "record": "session",
        "schema": SCHEMA,
        "session": session,
        "start_unix_nano": start_unix_nano,
        "app_version": env!("CARGO_PKG_VERSION"),
        "git_sha": env!("JUMPKEY_GIT_SHA"),
        "git_dirty": env!("JUMPKEY_GIT_DIRTY") == "1",
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "pid": std::process::id(),
        "qpc_hz": qpc_freq(),
        "tsc_hz": tsc_hz(),
        "machine": machine(),
        "config": {
            "scope": info.scope,
            "hint_order": info.hint_order,
            "alphabet_len": info.alphabet_len,
            "caching": info.caching,
            "cache_ttl_ms": info.cache_ttl_ms,
            "max_elements": info.max_elements,
        },
    })
}

fn machine() -> Value {
    const CPU: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
    const NT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    const BIOS: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
    let build = reg_str(NT, "CurrentBuild").map(|b| match reg_dword(NT, "UBR") {
        Some(u) => format!("{b}.{u}"),
        None => b,
    });
    let manufacturer = reg_str(BIOS, "SystemManufacturer");
    let product = reg_str(BIOS, "SystemProductName");
    json!({
        "host_hash": fnv1a(&std::env::var("COMPUTERNAME").unwrap_or_default()),
        "cpu_model": reg_str(CPU, "ProcessorNameString").map(|s| s.trim().to_string()),
        "logical_cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "ram_bytes": ram(),
        "os_build": build,
        "os_display_version": reg_str(NT, "DisplayVersion"),
        "is_vm": looks_like_vm(manufacturer.as_deref(), product.as_deref()),
        "display_adapter": display_adapter(),
        "monitors": monitors(),
        "power": power(),
    })
}

fn looks_like_vm(manufacturer: Option<&str>, product: Option<&str>) -> bool {
    let s = format!("{} {}", manufacturer.unwrap_or(""), product.unwrap_or("")).to_lowercase();
    [
        "qemu",
        "kvm",
        "vmware",
        "virtualbox",
        "virtual machine",
        "xen",
        "bochs",
        "parallels",
    ]
    .iter()
    .any(|k| s.contains(k))
}

/// FNV-1a of the lowercased string, as 16 hex digits (same as the
/// harness, so records from both can be grouped per machine).
fn fnv1a(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn reg_str(key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer and byte size are consistent.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]);
    (!s.is_empty()).then_some(s)
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    let (k, v) = (wide(key), wide(value));
    let mut out = 0u32;
    let mut size = 4u32;
    // SAFETY: 4-byte out buffer, size says so.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut out as *mut u32 as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(out)
}

fn ram() -> Option<u64> {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength set as required.
    unsafe { GlobalMemoryStatusEx(&mut m).ok()? };
    Some(m.ullTotalPhys)
}

fn display_adapter() -> Option<String> {
    let mut d = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: cb set as required. Device 0 normally drives the primary
    // display.
    unsafe {
        if !EnumDisplayDevicesW(PCWSTR::null(), 0, &mut d, 0).as_bool() {
            return None;
        }
    }
    let end = d
        .DeviceString
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(d.DeviceString.len());
    Some(String::from_utf16_lossy(&d.DeviceString[..end]))
}

fn monitors() -> Vec<Value> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<Value>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(m, &mut info.monitorInfo).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            out.push(json!({
                "x": r.left,
                "y": r.top,
                "width": r.right - r.left,
                "height": r.bottom - r.top,
                "dpi": dx,
                "primary": info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            }));
        }
        BOOL(1)
    }
    let mut out: Vec<Value> = Vec::new();
    // SAFETY: the callback only runs during this call and writes to `out`.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

fn power() -> Value {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: valid out-pointer.
    let ok = unsafe { GetSystemPowerStatus(&mut s).is_ok() };
    let source = match (ok, s.ACLineStatus) {
        (true, 1) => "ac",
        (true, 0) => "battery",
        _ => "unknown",
    };
    let battery_percent = (ok && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);

    // SAFETY: PowerGetActiveScheme allocates the GUID, freed with
    // LocalFree; the friendly-name buffer and size are consistent.
    let (plan_guid, plan_name) = unsafe {
        let mut guid_ptr: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut guid_ptr).is_ok() && !guid_ptr.is_null() {
            let guid = *guid_ptr;
            let _ = windows::Win32::Foundation::LocalFree(HLOCAL(guid_ptr as *mut _));
            let mut buf = [0u8; 512];
            let mut size = buf.len() as u32;
            let name = PowerReadFriendlyName(
                None,
                Some(&guid),
                None,
                None,
                Some(buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
            .then(|| {
                let w: Vec<u16> = buf[..(size as usize).min(buf.len())]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                String::from_utf16_lossy(&w)
            });
            (Some(format!("{guid:?}").to_lowercase()), name)
        } else {
            (None, None)
        }
    };
    json!({
        "source": source,
        "battery_percent": battery_percent,
        "plan_guid": plan_guid,
        "plan_name": plan_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_detection_and_host_hash() {
        assert!(looks_like_vm(Some("QEMU"), Some("Standard PC")));
        assert!(!looks_like_vm(Some("Micro-Star"), Some("MS-7D25")));
        assert_eq!(fnv1a("DESKTOP-ABC"), fnv1a("desktop-abc"));
    }

    #[test]
    fn session_record_has_no_computer_name() {
        let rec = session_record("abc", &SessionInfo::default());
        assert_eq!(rec["record"], "session");
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.is_empty() {
                assert!(!rec
                    .to_string()
                    .to_lowercase()
                    .contains(&name.to_lowercase()));
            }
        }
    }
}
