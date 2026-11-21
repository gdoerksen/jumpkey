//! What machine the run happened on, so results from different
//! machines, VMs and power settings aren't compared by accident.

use serde::Serialize;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
    MONITORINFOEXW,
};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, PowerReadFriendlyName, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

use super::clock::{qpc_freq, tsc_hz};

#[derive(Debug, Clone, Serialize)]
pub struct Machine {
    pub label: Option<String>,
    /// FNV-1a hash of the computer name: groups runs per machine without
    /// storing the name.
    pub host_hash: String,
    pub cpu_model: Option<String>,
    pub logical_cores: usize,
    pub ram_bytes: Option<u64>,
    pub os_build: Option<String>,
    pub os_display_version: Option<String>,
    pub bios_manufacturer: Option<String>,
    pub bios_product: Option<String>,
    /// Best guess from the BIOS strings. (The CPUID "hypervisor" bit
    /// isn't used: it's also set on bare metal with VBS / Hyper-V on.)
    pub is_vm: bool,
    pub display_adapter: Option<String>,
    pub monitors: Vec<Monitor>,
    pub power: Power,
    pub qpc_hz: i64,
    pub tsc_hz: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub dpi: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Power {
    /// `ac`, `battery` or `unknown`.
    pub source: String,
    pub battery_percent: Option<u8>,
    pub plan_guid: Option<String>,
    pub plan_name: Option<String>,
}

impl Power {
    /// Reasons this power state may skew timings, for a warning.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.source == "battery" {
            w.push("running on battery: CPU clocks may be throttled".into());
        }
        if let Some(name) = &self.plan_name {
            let n = name.to_lowercase();
            if n.contains("saver") || n.contains("power saving") {
                w.push(format!("power plan is `{name}`"));
            }
        }
        // Windows' built-in Power saver plan.
        if self.plan_guid.as_deref() == Some("a1841308-3541-4fab-bc81-f71556f20b4a") {
            w.push("power plan is Power saver".into());
        }
        w
    }
}

pub fn collect(label: Option<String>) -> Machine {
    let bios_manufacturer = reg_str(r"HARDWARE\DESCRIPTION\System\BIOS", "SystemManufacturer");
    let bios_product = reg_str(r"HARDWARE\DESCRIPTION\System\BIOS", "SystemProductName");
    let is_vm = looks_like_vm(bios_manufacturer.as_deref(), bios_product.as_deref());
    let build = reg_str(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuild",
    );
    let ubr = reg_dword(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "UBR");
    Machine {
        label,
        host_hash: fnv1a(&std::env::var("COMPUTERNAME").unwrap_or_default()),
        cpu_model: reg_str(
            r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "ProcessorNameString",
        )
        .map(|s| s.trim().to_string()),
        logical_cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        ram_bytes: ram(),
        os_build: build.map(|b| match ubr {
            Some(u) => format!("{b}.{u}"),
            None => b,
        }),
        os_display_version: reg_str(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "DisplayVersion",
        ),
        bios_manufacturer,
        bios_product,
        is_vm,
        display_adapter: display_adapter(),
        monitors: monitors(),
        power: power(),
        qpc_hz: qpc_freq(),
        tsc_hz: tsc_hz(),
    }
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
    // SAFETY: buffer + byte size are consistent.
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
    use windows::Win32::System::Registry::RRF_RT_REG_DWORD;
    let (k, v) = (wide(key), wide(value));
    let mut out = 0u32;
    let mut size = 4u32;
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
    unsafe { GlobalMemoryStatusEx(&mut m).ok()? };
    Some(m.ullTotalPhys)
}

fn display_adapter() -> Option<String> {
    let mut d = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // Device 0 is normally the adapter driving the primary display.
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

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<Monitor>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(m, &mut info.monitorInfo).as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            out.push(Monitor {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                dpi: dx,
                primary: info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            });
        }
        BOOL(1)
    }
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

fn power() -> Power {
    let mut s = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut s).is_ok() };
    let source = match (ok, s.ACLineStatus) {
        (true, 1) => "ac",
        (true, 0) => "battery",
        _ => "unknown",
    }
    .to_string();
    let battery_percent = (ok && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);

    let (plan_guid, plan_name) = unsafe {
        let mut guid_ptr: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut guid_ptr).is_ok() && !guid_ptr.is_null() {
            let guid = *guid_ptr;
            let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
                guid_ptr as *mut _,
            ));
            let mut buf = [0u8; 512];
            let mut size = buf.len() as u32;
            let name = if PowerReadFriendlyName(
                None,
                Some(&guid),
                None,
                None,
                Some(buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
            {
                let wide: Vec<u16> = buf[..size as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                Some(String::from_utf16_lossy(&wide))
            } else {
                None
            };
            (Some(format!("{guid:?}").to_lowercase()), name)
        } else {
            (None, None)
        }
    };

    Power {
        source,
        battery_percent,
        plan_guid,
        plan_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_detection_from_bios_strings() {
        assert!(looks_like_vm(
            Some("QEMU"),
            Some("Standard PC (Q35 + ICH9, 2009)")
        ));
        assert!(looks_like_vm(
            Some("Microsoft Corporation"),
            Some("Virtual Machine")
        ));
        assert!(!looks_like_vm(
            Some("Micro-Star International Co., Ltd."),
            Some("MS-7D25")
        ));
        assert!(!looks_like_vm(None, None));
    }

    #[test]
    fn host_hash_is_stable_and_case_insensitive() {
        assert_eq!(fnv1a("DESKTOP-ABC"), fnv1a("desktop-abc"));
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }

    #[test]
    fn power_saver_warns() {
        let p = Power {
            source: "battery".into(),
            battery_percent: Some(50),
            plan_guid: Some("a1841308-3541-4fab-bc81-f71556f20b4a".into()),
            plan_name: Some("Power saver".into()),
        };
        assert_eq!(p.warnings().len(), 3);
    }
}
