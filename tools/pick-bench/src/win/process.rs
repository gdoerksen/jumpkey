//! Per-process CPU and memory counters, read from outside the process.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use windows::core::PWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetGuiResources, GetProcessHandleCount, OpenProcess, QueryFullProcessImageNameW, GR_GDIOBJECTS,
    GR_USEROBJECTS, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::System::Threading::{GetProcessTimes, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::System::WindowsProgramming::QueryProcessCycleTime;

/// An open process handle with query rights. Closed on drop.
pub struct Proc {
    handle: HANDLE,
    pub pid: u32,
}

impl Proc {
    pub fn open(pid: u32) -> Option<Self> {
        // SAFETY: OpenProcess with query rights; checked for failure.
        // VM_READ is only needed for memory counters on old Windows;
        // fall back to limited rights when it's refused.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )
            .or_else(|_| OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid))
            .ok()?
        };
        Some(Self { handle, pid })
    }

    /// CPU cycles consumed by all threads of the process so far.
    pub fn cycles(&self) -> Option<u64> {
        let mut c = 0u64;
        unsafe { QueryProcessCycleTime(self.handle, &mut c).ok()? };
        Some(c)
    }

    pub fn snapshot(&self) -> ProcSnapshot {
        let mut mem = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: valid handle + correctly-sized struct.
        let mem_ok = unsafe {
            GetProcessMemoryInfo(
                self.handle,
                &mut mem as *mut _ as *mut _,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            )
            .is_ok()
        };
        let mut handles = 0u32;
        let handles_ok = unsafe { GetProcessHandleCount(self.handle, &mut handles).is_ok() };
        let (gdi, user) = unsafe {
            (
                GetGuiResources(self.handle, GR_GDIOBJECTS),
                GetGuiResources(self.handle, GR_USEROBJECTS),
            )
        };
        ProcSnapshot {
            cycles: self.cycles(),
            private_bytes: mem_ok.then_some(mem.PrivateUsage as u64),
            working_set: mem_ok.then_some(mem.WorkingSetSize as u64),
            peak_working_set: mem_ok.then_some(mem.PeakWorkingSetSize as u64),
            handles: handles_ok.then_some(handles),
            gdi_objects: Some(gdi),
            user_objects: Some(user),
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcSnapshot {
    pub cycles: Option<u64>,
    pub private_bytes: Option<u64>,
    pub working_set: Option<u64>,
    pub peak_working_set: Option<u64>,
    pub handles: Option<u32>,
    pub gdi_objects: Option<u32>,
    pub user_objects: Option<u32>,
}

/// Basename of a process's image, e.g. `vivaldi.exe`.
pub fn exe_name(pid: u32) -> Option<String> {
    let p = Proc::open(pid)?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buffer + length are consistent.
    unsafe {
        QueryFullProcessImageNameW(
            p.handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_string)
}

/// `(pid, parent pid, exe basename)` for every running process.
pub fn all_processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    // SAFETY: Toolhelp snapshot iteration; handle closed below.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let end = e
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExeFile.len());
                out.push((
                    e.th32ProcessID,
                    e.th32ParentProcessID,
                    String::from_utf16_lossy(&e.szExeFile[..end]),
                ));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Pids of running processes whose image is `exe`.
pub fn pids_of_exe(exe: &str) -> Vec<u32> {
    all_processes()
        .into_iter()
        .filter(|(_, _, n)| n.eq_ignore_ascii_case(exe))
        .map(|(pid, _, _)| pid)
        .collect()
}

/// `root` plus all of its descendants.
///
/// Chromium and Electron apps (Vivaldi, VS Code) build the
/// accessibility tree in renderer / helper processes, not in the
/// process that owns the window, so the cost jumpkey imposes on them
/// only shows up when the whole tree is counted.
pub fn process_tree(root: u32) -> Vec<u32> {
    let procs = all_processes();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid, _) in &procs {
        if pid != ppid {
            children.entry(*ppid).or_default().push(*pid);
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if let Some(kids) = children.get(&p) {
            stack.extend(kids.iter().copied());
        }
    }
    let mut out: Vec<u32> = seen.into_iter().collect();
    out.sort_unstable();
    out
}

/// Open handles to a set of processes so cycle counts can be summed.
/// Processes that exit between two readings simply stop contributing.
pub struct ProcGroup {
    procs: Vec<Proc>,
}

impl ProcGroup {
    pub fn open(pids: &[u32]) -> Self {
        Self {
            procs: pids.iter().filter_map(|&p| Proc::open(p)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// Per-pid cycle readings.
    pub fn cycles(&self) -> HashMap<u32, u64> {
        self.procs
            .iter()
            .filter_map(|p| p.cycles().map(|c| (p.pid, c)))
            .collect()
    }
}

/// Sum of cycle increases between two per-pid readings (pids missing
/// from either side are ignored).
pub fn cycles_delta(before: &HashMap<u32, u64>, after: &HashMap<u32, u64>) -> u64 {
    after
        .iter()
        .filter_map(|(pid, a)| before.get(pid).map(|b| a.saturating_sub(*b)))
        .sum()
}

/// When the process was created, as Unix milliseconds.
pub fn created_unix_ms(pid: u32) -> Option<u64> {
    let p = Proc::open(pid)?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: valid handle and out-pointers.
    unsafe {
        GetProcessTimes(p.handle, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
    }
    let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    // FILETIME counts 100 ns ticks since 1601-01-01.
    const EPOCH_DIFF_MS: u64 = 11_644_473_600_000;
    (ticks / 10_000).checked_sub(EPOCH_DIFF_MS)
}

pub fn is_alive(pid: u32) -> bool {
    all_processes().iter().any(|(p, _, _)| *p == pid)
}

/// Terminate `root` and all its descendants. Only used for browser
/// instances the harness started with a throwaway profile.
pub fn kill_tree(root: u32) -> usize {
    let mut killed = 0;
    for pid in process_tree(root) {
        // SAFETY: open with terminate rights, terminate, close.
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                if TerminateProcess(h, 1).is_ok() {
                    killed += 1;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    killed
}
