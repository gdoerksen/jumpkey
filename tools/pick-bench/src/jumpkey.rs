//! Starting and stopping the jumpkey build under test, in a sandbox.
//!
//! Everything here goes through the bench contract (`bench/README.md`):
//! the `--no-tray` / `--close` flags, `APPDATA` for the config,
//! `LOCALAPPDATA` for logs and telemetry, and window show/hide events.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::win::clock::{qpc, qpc_delta_us};
use crate::win::events::{pump_until, pump_until_every, Watcher};
use crate::win::process::Proc;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Sandbox {
    pub appdata: PathBuf,
    pub localappdata: PathBuf,
    /// Extra environment for jumpkey (e.g. `JUMPKEY_BENCH_LABELS`).
    pub env: Vec<(&'static str, std::ffi::OsString)>,
}

impl Sandbox {
    /// `<run>/sandbox/<build>-<scope>/{appdata,localappdata}` with the
    /// scope's config template copied in as `jumpkey\config.toml`.
    pub fn create(root: &Path, config_template: &Path) -> Result<Self> {
        let appdata = root.join("appdata");
        let localappdata = root.join("localappdata");
        std::fs::create_dir_all(appdata.join("jumpkey"))?;
        std::fs::create_dir_all(&localappdata)?;
        std::fs::copy(config_template, appdata.join("jumpkey").join("config.toml"))
            .with_context(|| format!("copying config template {}", config_template.display()))?;
        Ok(Self {
            appdata,
            localappdata,
            env: Vec::new(),
        })
    }

    fn apply(&self, cmd: &mut Command, run_label: &str) {
        cmd.env("APPDATA", &self.appdata)
            .env("LOCALAPPDATA", &self.localappdata)
            .env("JUMPKEY_BENCH_RUN_LABEL", run_label)
            .env("JUMPKEY_BENCH_DWMFLUSH", "1")
            .creation_flags(CREATE_NO_WINDOW);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
    }
}

/// How jumpkey's startup went (see [`Jumpkey::wait_startup`]).
#[derive(Debug, Clone, Copy)]
pub struct Startup {
    /// Spawn → ready to take a hotkey (splash closed and quiet).
    pub ready_ms: f64,
    /// Whether a window (the splash) was seen during startup.
    pub saw_window: bool,
}

pub struct Jumpkey {
    exe: PathBuf,
    child: Child,
    pub proc: Proc,
    pub watcher: Watcher,
    pub spawned_qpc: i64,
    stopped: bool,
}

impl Jumpkey {
    pub fn spawn(exe: &Path, sandbox: &Sandbox, run_label: &str) -> Result<Self> {
        let log_dir = sandbox.localappdata.join("bench-console");
        std::fs::create_dir_all(&log_dir)?;
        let mut cmd = Command::new(exe);
        cmd.arg("--no-tray")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(log_dir.join("stdout.txt"))?)
            .stderr(std::fs::File::create(log_dir.join("stderr.txt"))?);
        sandbox.apply(&mut cmd, run_label);
        let spawned_qpc = qpc();
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        let pid = child.id();
        let watcher = Watcher::new(pid)?;
        let Some(proc) = Proc::open(pid) else {
            let _ = child.kill();
            bail!("couldn't open jumpkey process {pid} for reading counters");
        };
        // A second instance exits immediately (single-instance guard).
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "jumpkey exited right after starting ({status}). Is another jumpkey already \
                 running? See {}",
                log_dir.display()
            );
        }
        Ok(Self {
            exe: exe.to_path_buf(),
            child,
            proc,
            watcher,
            spawned_qpc,
            stopped: false,
        })
    }

    /// Wait until jumpkey has finished starting. Returns what happened,
    /// for the `process_start` record.
    ///
    /// Today a splash screen is up for ~2.5 s, and a hotkey pressed
    /// while it shows is only handled once the splash closes. So: once a
    /// jumpkey window has been seen, wait until none has been visible
    /// for `QUIET`. If no window appears at all, assume a build without a
    /// splash and go ahead after `NO_WINDOW`. No particular window is
    /// assumed, so this survives UI changes.
    ///
    /// `NO_WINDOW` is long on purpose: the splash usually shows within
    /// milliseconds, but sometimes jumpkey takes 3–5 s before it does
    /// (seen while a freshly started browser was busy). A shorter wait
    /// pressed the hotkey before jumpkey had registered it: the press
    /// was lost and the splash appearing later was timed as the overlay.
    pub fn wait_startup(&mut self, timeout: Duration) -> Result<Startup> {
        const MIN_STARTUP: Duration = Duration::from_millis(1000);
        const NO_WINDOW: Duration = Duration::from_millis(15_000);
        const QUIET: Duration = Duration::from_millis(400);
        let pid = self.pid();
        let start = self.spawned_qpc;
        let mut seen_window = false;
        let mut quiet_since: Option<i64> = None;
        let ok = pump_until_every(timeout, Duration::from_millis(20), || {
            let now = qpc();
            if crate::win::window::visible_windows_of_pid(pid) > 0 {
                seen_window = true;
                quiet_since = None;
                return false;
            }
            let since_spawn = qpc_delta_us(start, now);
            if !seen_window {
                return since_spawn >= NO_WINDOW.as_micros() as u64;
            }
            let since = *quiet_since.get_or_insert(now);
            since_spawn >= MIN_STARTUP.as_micros() as u64
                && qpc_delta_us(since, now) >= QUIET.as_micros() as u64
        });
        if !self.is_running() {
            bail!("jumpkey exited during startup");
        }
        if !ok {
            bail!(
                "jumpkey still had a window open {} s after starting (a message box?)",
                timeout.as_secs()
            );
        }
        crate::win::events::clear();
        Ok(Startup {
            ready_ms: qpc_delta_us(start, qpc()) as f64 / 1000.0,
            saw_window: seen_window,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ask jumpkey to quit through `--close`, then wait; kill as a last
    /// resort. Returns whether it exited cleanly.
    pub fn stop(&mut self, sandbox: &Sandbox) -> bool {
        if self.stopped {
            return true;
        }
        self.stopped = true;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--close")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        sandbox.apply(&mut cmd, "");
        let _ = cmd.status();
        let exited = pump_until(Duration::from_secs(5), || {
            matches!(self.child.try_wait(), Ok(Some(_)))
        });
        if !exited {
            eprintln!(
                "warning: jumpkey (pid {}) didn't exit after --close; killing it",
                self.pid()
            );
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        exited
    }

    pub fn since_spawn_ms(&self, at_qpc: i64) -> f64 {
        qpc_delta_us(self.spawned_qpc, at_qpc) as f64 / 1000.0
    }
}

impl Drop for Jumpkey {
    fn drop(&mut self) {
        // Never leave a sandboxed jumpkey holding the user's hotkeys.
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `jumpkey --version`, run without touching any instance.
pub fn version(exe: &Path) -> Option<String> {
    let out = Command::new(exe)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
