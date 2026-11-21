//! Result files: `manifest.json` (run + machine description) and
//! `iterations.jsonl` (one JSON object per line, written as the run
//! goes so a crash keeps everything measured so far).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::win::machine::Machine;
use crate::win::process::ProcSnapshot;

/// Bump when a field changes meaning or is removed.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub schema: u32,
    pub tool: &'static str,
    pub tool_version: &'static str,
    pub run_id: String,
    pub label: Option<String>,
    pub started_local: String,
    pub builds: Vec<BuildInfo>,
    pub scenarios: Vec<String>,
    pub patterns: Vec<String>,
    pub scopes: Vec<String>,
    pub iterations: usize,
    pub block_size: usize,
    pub hotkey: String,
    pub hold_ms: u64,
    pub machine: Machine,
    pub warnings: Vec<String>,
    /// Filled in at the end of the run.
    pub finished_local: Option<String>,
    pub aborted: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub label: String,
    pub exe: PathBuf,
    /// `jumpkey --version` output.
    pub version: Option<String>,
    pub exe_bytes: Option<u64>,
    pub exe_modified_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// jumpkey started and answered its first hotkey press.
    ProcessStart {
        build: String,
        scope: String,
        scenario: String,
        pid: u32,
        /// Spawn → ready for the first press (splash shown and closed).
        /// `None` with `--press-during-startup`.
        startup_ms: Option<f64>,
        /// Whether a startup window (the splash) was seen. `false` means
        /// pick-bench waited the full no-window timeout.
        startup_saw_window: Option<bool>,
        /// Spawn → first overlay visible (includes the splash screen).
        spawn_to_first_visible_ms: Option<f64>,
        /// The first press after start: cold overlay creation etc.
        first_press_visible_us: Option<u64>,
        /// Presses needed before the first overlay appeared (a target
        /// still starting up can make jumpkey find nothing).
        first_press_attempts: Option<u32>,
        after: ProcSnapshot,
    },
    /// Background cost with nobody pressing anything.
    Idle {
        build: String,
        scope: String,
        scenario: String,
        secs: f64,
        jumpkey_cycles: u64,
        jumpkey_cpu_ms: Option<f64>,
        jumpkey_cpu_percent_of_one_core: Option<f64>,
        after: ProcSnapshot,
    },
    Iteration(Box<Iteration>),
    ProcessStop {
        build: String,
        scope: String,
        scenario: String,
        pid: u32,
        clean_exit: bool,
        before: ProcSnapshot,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Iteration {
    pub build: String,
    pub scope: String,
    pub scenario: String,
    pub pattern: String,
    pub block: usize,
    pub iter: usize,
    /// Hotkey sent → a jumpkey top-level window became visible.
    pub hotkey_to_visible_us: Option<u64>,
    /// Visible → jumpkey owns the foreground (current overlay takes
    /// focus; a future no-activate overlay never will, so `None`).
    pub visible_to_foreground_us: Option<u64>,
    pub esc_to_hidden_us: Option<u64>,
    pub timed_out: bool,
    pub focus_retries: u32,
    /// Raw QPC of the hotkey send (system-wide clock; join key for the
    /// in-app telemetry).
    pub qpc_hotkey: i64,
    /// jumpkey CPU from hotkey to overlay hidden.
    pub jumpkey_cycles: Option<u64>,
    pub jumpkey_cpu_ms: Option<f64>,
    /// Target app CPU (whole process tree) over the same interval.
    pub target_cycles: u64,
    pub target_cpu_ms: Option<f64>,
    pub target_processes: usize,
    /// Target window rect `[x, y, w, h]` (physical px) and its monitor's DPI.
    pub target_rect: Option<[i32; 4]>,
    pub target_dpi: u32,
    /// Visible app windows on the desktop (matters for multi-window scopes).
    pub visible_app_windows: usize,
    /// Since the previous overlay closed: window switching, dwell time,
    /// and whatever jumpkey did in the background meanwhile (warming).
    pub between_ms: f64,
    pub between_jumpkey_cycles: Option<u64>,
    pub between_jumpkey_cpu_ms: Option<f64>,
    pub between_target_cycles: u64,
    pub between_target_cpu_ms: Option<f64>,
    pub after: ProcSnapshot,
}

pub struct RunWriter {
    pub dir: PathBuf,
    lines: BufWriter<File>,
}

impl RunWriter {
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let f = File::create(dir.join("iterations.jsonl"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lines: BufWriter::new(f),
        })
    }

    pub fn record(&mut self, r: &Record) -> Result<()> {
        serde_json::to_writer(&mut self.lines, r)?;
        self.lines.write_all(b"\n")?;
        self.lines.flush()?;
        Ok(())
    }

    pub fn write_manifest(&self, m: &Manifest) -> Result<()> {
        let f = File::create(self.dir.join("manifest.json"))?;
        serde_json::to_writer_pretty(BufWriter::new(f), m)?;
        Ok(())
    }
}
