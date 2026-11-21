//! The benchmark loop.
//!
//! Order: for each scope → scenario (open the target app once) →
//! pattern → block → build. Each block starts the build fresh, does one
//! warm-up press (recorded separately: it includes first-use costs such
//! as creating the overlay), then the measured presses. With one build
//! there's one block per pattern; in A/B mode builds alternate per block
//! (AB, BA, AB, …) so drift affects both equally.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cli::Options;
use crate::jumpkey::{self, Jumpkey, Sandbox};
use crate::output::{BuildInfo, Iteration, Manifest, Record, RunWriter, SCHEMA};
use crate::scenario::{Pattern, Placeholders, Scenario, ScenarioFile};
use crate::stats::summarize;
use crate::win::clock::{cycles_to_ms, qpc, qpc_delta_us};
use crate::win::events::{self, pump_sleep, pump_until, pump_until_every};
use crate::win::input::{self, Chord};
use crate::win::process::{self, cycles_delta, pids_of_exe, process_tree, ProcGroup};
use crate::win::window::{self, NeutralWindow};
use crate::win::{self as w, machine};

/// Generous upper bounds; a timeout is recorded, not fatal.
const VISIBLE_TIMEOUT: Duration = Duration::from_secs(10);
const HIDE_TIMEOUT: Duration = Duration::from_secs(5);
/// After a visible timeout, how much longer to wait for a late overlay
/// so it can be closed before the next press.
const LATE_SHOW_GRACE: Duration = Duration::from_secs(10);
/// The first press after start pays one-time costs; allow more time.
const FIRST_PRESS_TIMEOUT: Duration = Duration::from_secs(20);
/// Tries for the first press before the run is abandoned.
const FIRST_PRESS_ATTEMPTS: u32 = 3;
/// Wait between first-press tries, for the target to finish starting.
const FIRST_PRESS_RETRY_WAIT: Duration = Duration::from_secs(3);
/// Upper bound for jumpkey's startup (splash screen etc.).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Time to look away (neutral window focused) in switch-then-press.
const AWAY_MS: u64 = 300;
/// How often to check slow external state (process exit, deleting a
/// profile folder) while waiting.
const SLOW_POLL: Duration = Duration::from_millis(100);

static STOP: AtomicBool = AtomicBool::new(false);

fn install_ctrl_c() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(_: u32) -> BOOL {
        STOP.store(true, Ordering::SeqCst);
        BOOL(1)
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

fn check_stop() -> Result<()> {
    if STOP.load(Ordering::SeqCst) {
        bail!("interrupted (Ctrl+C)");
    }
    Ok(())
}

struct Ctx {
    opts: Options,
    run_id: String,
    neutral: NeutralWindow,
    writer: RunWriter,
    /// Builds that already had their idle measurement, per scope.
    idle_done: HashSet<(String, String)>,
    results: Vec<Iteration>,
    first_presses: Vec<(String, String, String, u64)>,
}

struct Target {
    name: String,
    hwnd: isize,
    opened_by_us: bool,
    root_pid: u32,
    close: bool,
    /// Throwaway browser profile to delete once the window is closed.
    profile: Option<PathBuf>,
    /// The browser instance started for that profile (its own process
    /// tree), safe to end if it doesn't exit after the window closes.
    owned_instance: Option<u32>,
}

pub fn run(opts: Options) -> Result<()> {
    w::init_dpi();
    install_ctrl_c();

    let repo = find_repo(opts.repo.as_deref())?;
    let scenario_path = opts
        .scenario_file
        .clone()
        .unwrap_or_else(|| repo.join("bench").join("scenarios.toml"));
    let scenarios = ScenarioFile::load(&scenario_path)?;
    let mut ph = Placeholders {
        fixtures: repo.join("bench").join("fixtures"),
        repo: repo.clone(),
        profiles: None,
    };
    ensure_fixture_folder(&ph.fixtures)?;

    // Builds.
    let mut builds = Vec::new();
    for (label, exe) in &opts.builds {
        let exe = if exe.as_os_str().is_empty() {
            repo.join("target").join("release").join("jumpkey.exe")
        } else {
            exe.clone()
        };
        let exe = exe
            .canonicalize()
            .with_context(|| format!("jumpkey build not found: {}", exe.display()))?;
        // canonicalize() returns `\\?\C:\…`; keep the familiar form.
        let exe = PathBuf::from(
            exe.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_string(),
        );
        let meta = std::fs::metadata(&exe).ok();
        builds.push(BuildInfo {
            label: label.clone(),
            version: jumpkey::version(&exe),
            exe_bytes: meta.as_ref().map(|m| m.len()),
            exe_modified_unix: meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            exe,
        });
    }
    refuse_if_jumpkey_running(&builds)?;

    // Hotkey: explicit flag, else the template's `hotkeys.pick_element`.
    let config_dir = repo.join("bench").join("config");
    let template = move |scope: &str| config_dir.join(format!("{scope}.toml"));
    for scope in &opts.scopes {
        if !template(scope).is_file() {
            bail!("missing config template {}", template(scope).display());
        }
    }
    // The chord to press, per scope (each scope's jumpkey reads its own
    // template).
    let mut chords: BTreeMap<String, Chord> = BTreeMap::new();
    for scope in &opts.scopes {
        let text = match &opts.hotkey {
            Some(h) => h.clone(),
            None => template_chords(&template(scope))?
                .into_iter()
                .find(|(name, _)| name == "pick_element")
                .map(|(_, c)| c)
                // jumpkey's documented default.
                .unwrap_or_else(|| "Ctrl+Shift+Space".into()),
        };
        chords.insert(scope.clone(), Chord::parse(&text)?);
    }
    let pressed: Vec<String> = chords.values().map(|c| c.text.clone()).collect();
    check_chords_free(&opts.scopes, &template, &pressed)?;
    // One chord in the usual case; otherwise `scope=chord; …`.
    let mut unique = pressed.clone();
    unique.sort();
    unique.dedup();
    let hotkey_text = if unique.len() == 1 {
        unique.remove(0)
    } else {
        chords
            .iter()
            .map(|(s, c)| format!("{s}={}", c.text))
            .collect::<Vec<_>>()
            .join("; ")
    };

    let scenario_names: Vec<String> = if opts.manual {
        vec!["manual".into()]
    } else if opts.all_scenarios {
        scenarios.scenarios.iter().map(|s| s.name.clone()).collect()
    } else {
        for n in &opts.scenarios {
            scenarios.get(n)?;
        }
        opts.scenarios.clone()
    };

    let machine = machine::collect(opts.machine_label.clone());
    for (i, m) in machine.monitors.iter().enumerate() {
        println!(
            "monitor {i}: {}x{} at ({}, {}), {} dpi{}",
            m.width,
            m.height,
            m.x,
            m.y,
            m.dpi,
            if m.primary { " (primary)" } else { "" }
        );
    }
    let origin = match opts.monitor.as_deref() {
        None | Some("primary") => machine
            .monitors
            .iter()
            .find(|m| m.primary)
            .map(|m| (m.x, m.y))
            .unwrap_or((0, 0)),
        Some(n) => {
            let i: usize = n
                .parse()
                .with_context(|| format!("--monitor expects a number or `primary`, got `{n}`"))?;
            let m = machine
                .monitors
                .get(i)
                .with_context(|| format!("no monitor {i} (have {})", machine.monitors.len()))?;
            (m.x, m.y)
        }
    };
    let warnings = machine.power.warnings();
    for wmsg in &warnings {
        eprintln!("warning: {wmsg}");
    }

    let run_id = match &opts.label {
        Some(l) => format!("{}-{}", w::local_timestamp(), sanitize(l)),
        None => w::local_timestamp(),
    };
    let out_root = opts
        .out
        .clone()
        .unwrap_or_else(|| repo.join("bench").join("results"));
    let writer = RunWriter::create(&out_root.join(&run_id))?;
    ph.profiles = Some(writer.dir.join("profiles"));
    let mut manifest = Manifest {
        schema: SCHEMA,
        tool: "pick-bench",
        tool_version: env!("CARGO_PKG_VERSION"),
        run_id: run_id.clone(),
        label: opts.label.clone(),
        started_local: w::local_timestamp(),
        builds: builds.clone(),
        scenarios: scenario_names.clone(),
        patterns: opts.patterns.iter().map(|p| p.to_string()).collect(),
        scopes: opts.scopes.clone(),
        iterations: opts.iterations,
        block_size: opts.block_size,
        hotkey: hotkey_text.clone(),
        hold_ms: opts.hold_ms,
        machine,
        warnings,
        finished_local: None,
        aborted: None,
    };
    writer.write_manifest(&manifest)?;
    println!("pick-bench run {run_id}");
    println!("results: {}", writer.dir.display());
    println!("Hands off the keyboard and mouse while it runs. Ctrl+C to stop.\n");

    let neutral = NeutralWindow::create([40, 40, 480, 320])?;
    let mut ctx = Ctx {
        opts,
        run_id,
        neutral,
        writer,
        idle_done: HashSet::new(),
        results: Vec::new(),
        first_presses: Vec::new(),
    };

    let outcome = run_matrix(
        &mut ctx,
        origin,
        &scenarios,
        &ph,
        &builds,
        &chords,
        &scenario_names,
        &template,
    );
    manifest.finished_local = Some(w::local_timestamp());
    if let Err(e) = &outcome {
        manifest.aborted = Some(format!("{e:#}"));
    }
    ctx.writer.write_manifest(&manifest)?;
    if let Some(dir) = &ph.profiles {
        let gone = !dir.exists()
            || pump_until_every(Duration::from_secs(30), SLOW_POLL, || {
                std::fs::remove_dir_all(dir).is_ok() || !dir.exists()
            });
        if !gone {
            eprintln!(
                "note: couldn't delete temporary browser profiles in {} (still in use?)",
                dir.display()
            );
        }
    }
    print_table(&ctx);
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_matrix(
    ctx: &mut Ctx,
    origin: (i32, i32),
    scenarios: &ScenarioFile,
    ph: &Placeholders,
    builds: &[BuildInfo],
    chords: &BTreeMap<String, Chord>,
    scenario_names: &[String],
    template: &dyn Fn(&str) -> PathBuf,
) -> Result<()> {
    let scopes = ctx.opts.scopes.clone();
    let patterns = ctx.opts.patterns.clone();
    for scope in &scopes {
        for name in scenario_names {
            check_stop()?;
            let target = if ctx.opts.manual {
                Some(manual_target(ctx.opts.manual_delay_secs)?)
            } else {
                open_target(scenarios.get(name)?, &scenarios.defaults, ph, origin)?
            };
            let Some(target) = target else { continue };
            println!("== {scope} / {name}");
            let chord = &chords[scope];
            let result = (|| -> Result<()> {
                for pattern in &patterns {
                    run_pattern(ctx, builds, chord, scope, &target, *pattern, template)?;
                }
                Ok(())
            })();
            if target.opened_by_us && target.close && !ctx.opts.keep_open {
                window::close(target.hwnd);
                pump_sleep(Duration::from_millis(500));
                if let Some(pid) = target.owned_instance {
                    // Fresh-profile Vivaldi asks "Confirm exit" when its
                    // last window closes, and would sit there forever.
                    // This instance only exists for the benchmark, so end
                    // it if it hasn't quit on its own.
                    if !pump_until_every(Duration::from_secs(5), SLOW_POLL, || {
                        !process::is_alive(pid)
                    }) {
                        process::kill_tree(pid);
                    }
                }
                if let Some(dir) = &target.profile {
                    // The browser holds files until all its processes exit,
                    // which can take a while; anything left is removed by
                    // the sweep at the end of the run.
                    pump_until_every(Duration::from_secs(15), SLOW_POLL, || {
                        std::fs::remove_dir_all(dir).is_ok() || !dir.exists()
                    });
                }
            }
            result?;
        }
    }
    Ok(())
}

fn run_pattern(
    ctx: &mut Ctx,
    builds: &[BuildInfo],
    chord: &Chord,
    scope: &str,
    target: &Target,
    pattern: Pattern,
    template: &dyn Fn(&str) -> PathBuf,
) -> Result<()> {
    let iterations = ctx.opts.iterations;
    let block_size = if builds.len() > 1 {
        ctx.opts.block_size
    } else {
        iterations
    };
    let blocks = iterations.div_ceil(block_size);
    let mut done: HashMap<String, usize> = HashMap::new();
    for block in 0..blocks {
        // AB, BA, AB, … cancels "second in the pair" effects.
        let order: Vec<&BuildInfo> = if block % 2 == 0 {
            builds.iter().collect()
        } else {
            builds.iter().rev().collect()
        };
        for build in order {
            check_stop()?;
            let n_done = done.entry(build.label.clone()).or_default();
            let n = block_size.min(iterations - *n_done);
            if n == 0 {
                continue;
            }
            run_block(
                ctx, build, chord, scope, target, pattern, block, *n_done, n, template,
            )?;
            *n_done += n;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_block(
    ctx: &mut Ctx,
    build: &BuildInfo,
    chord: &Chord,
    scope: &str,
    target: &Target,
    pattern: Pattern,
    block: usize,
    first_iter: usize,
    n: usize,
    template: &dyn Fn(&str) -> PathBuf,
) -> Result<()> {
    let sandbox_dir = ctx
        .writer
        .dir
        .join("sandbox")
        .join(format!("{}-{}", build.label, scope));
    let sandbox = Sandbox::create(&sandbox_dir, &template(scope))?;
    let mut jk = Jumpkey::spawn(&build.exe, &sandbox, &ctx.run_id)?;
    let result = (|| -> Result<()> {
        // Warm-up / readiness: the first press after start. Recorded on
        // its own because it pays one-time costs.
        let startup = if ctx.opts.press_during_startup {
            // First press lands while jumpkey is still starting up; the
            // timeout below then covers the whole startup.
            window::focus(target.hwnd)?;
            pump_sleep(Duration::from_millis(200));
            None
        } else {
            let s = jk.wait_startup(STARTUP_TIMEOUT)?;
            if s.ready_ms > 5_000.0 || !s.saw_window {
                println!(
                    "   note: jumpkey took {:.1} s to start{}",
                    s.ready_ms / 1000.0,
                    if s.saw_window {
                        ""
                    } else {
                        " (no splash seen)"
                    }
                );
            }
            // The splash may have taken the foreground: focus the target
            // again right before the first press.
            window::focus(target.hwnd)?;
            pump_sleep(Duration::from_millis(500));
            Some(s)
        };
        // A freshly started target (a browser building its accessibility
        // tree) can answer jumpkey's first search with nothing, and
        // jumpkey then shows no overlay. Retry a few times before giving
        // up on the run; the attempts are recorded.
        let mut first_attempts = 0;
        let (first, first_visible_qpc) = loop {
            first_attempts += 1;
            let press = press_and_dismiss(&mut jk, chord, FIRST_PRESS_TIMEOUT, ctx.opts.hold_ms)?;
            if let Some(v) = press.visible_qpc {
                break (press, v);
            }
            if first_attempts >= FIRST_PRESS_ATTEMPTS {
                bail!(
                    "jumpkey never showed an overlay after the hotkey ({}), {} tries. Check the \
                     hotkey matches the config template, and that the target window has \
                     clickable elements.",
                    chord.text,
                    first_attempts
                );
            }
            println!("   note: no overlay on first press (try {first_attempts}); retrying");
            pump_sleep(FIRST_PRESS_RETRY_WAIT);
            window::focus(target.hwnd)?;
            pump_sleep(Duration::from_millis(500));
        };
        let first_us = qpc_delta_us(first.sent_qpc, first_visible_qpc);
        ctx.first_presses.push((
            build.label.clone(),
            scope.into(),
            target.name.clone(),
            first_us,
        ));
        ctx.writer.record(&Record::ProcessStart {
            build: build.label.clone(),
            scope: scope.into(),
            scenario: target.name.clone(),
            pid: jk.pid(),
            startup_ms: startup.map(|s| s.ready_ms),
            startup_saw_window: startup.map(|s| s.saw_window),
            spawn_to_first_visible_ms: Some(jk.since_spawn_ms(first_visible_qpc)),
            first_press_visible_us: Some(first_us),
            first_press_attempts: Some(first_attempts),
            after: jk.proc.snapshot(),
        })?;

        let key = (build.label.clone(), scope.to_string());
        if ctx.opts.idle_secs > 0 && !ctx.idle_done.contains(&key) {
            measure_idle(ctx, &jk, build, scope, &target.name)?;
            ctx.idle_done.insert(key);
        }

        let group = ProcGroup::open(&process_tree(target.root_pid));
        let mut last_jk = jk.proc.cycles();
        let mut last_target = group.cycles();
        let mut last_qpc = qpc();
        for i in 0..n {
            check_stop()?;
            if !jk.is_running() {
                bail!(
                    "jumpkey exited during the run (see {})",
                    sandbox_dir.display()
                );
            }
            dismiss_leftover_overlay(&mut jk)?;
            // Behaviour before the press.
            let mut focus_retries = 0;
            match pattern {
                Pattern::SwitchThenPress { dwell_ms } => {
                    window::focus(ctx.neutral.hwnd())?;
                    pump_sleep(Duration::from_millis(AWAY_MS));
                    window::focus(target.hwnd)?;
                    pump_sleep(Duration::from_millis(dwell_ms));
                }
                Pattern::RepeatPress { gap_ms } => {
                    pump_sleep(Duration::from_millis(gap_ms));
                }
            }
            if window::foreground() != target.hwnd {
                focus_retries += 1;
                window::focus(target.hwnd)?;
            }
            let visible_app_windows = window::app_windows().len();

            let jk_before = jk.proc.cycles();
            let t_before = group.cycles();
            let press_qpc = qpc();
            let between_ms = qpc_delta_us(last_qpc, press_qpc) as f64 / 1000.0;
            let between_jk = match (last_jk, jk_before) {
                (Some(a), Some(b)) => Some(b.saturating_sub(a)),
                _ => None,
            };
            let between_t = cycles_delta(&last_target, &t_before);

            let p = press_and_dismiss(&mut jk, chord, VISIBLE_TIMEOUT, ctx.opts.hold_ms)?;

            let jk_after = jk.proc.cycles();
            let t_after = group.cycles();
            let pick_jk = match (jk_before, jk_after) {
                (Some(a), Some(b)) => Some(b.saturating_sub(a)),
                _ => None,
            };
            let pick_t = cycles_delta(&t_before, &t_after);
            last_jk = jk_after;
            last_target = t_after;
            last_qpc = qpc();

            let it = Iteration {
                build: build.label.clone(),
                scope: scope.into(),
                scenario: target.name.clone(),
                pattern: pattern.to_string(),
                block,
                iter: first_iter + i,
                hotkey_to_visible_us: p.visible_qpc.map(|v| qpc_delta_us(p.sent_qpc, v)),
                visible_to_foreground_us: p
                    .visible_qpc
                    .zip(p.foreground_qpc)
                    .map(|(v, f)| qpc_delta_us(v, f)),
                esc_to_hidden_us: p.esc_qpc.zip(p.hidden_qpc).map(|(e, h)| qpc_delta_us(e, h)),
                timed_out: p.visible_qpc.is_none() || p.hidden_qpc.is_none(),
                focus_retries,
                qpc_hotkey: p.sent_qpc,
                jumpkey_cycles: pick_jk,
                jumpkey_cpu_ms: pick_jk.and_then(cycles_to_ms),
                target_cycles: pick_t,
                target_cpu_ms: cycles_to_ms(pick_t),
                target_processes: group.len(),
                visible_app_windows,
                target_rect: window::rect(target.hwnd),
                target_dpi: window::dpi(target.hwnd),
                between_ms,
                between_jumpkey_cycles: between_jk,
                between_jumpkey_cpu_ms: between_jk.and_then(cycles_to_ms),
                between_target_cycles: between_t,
                between_target_cpu_ms: cycles_to_ms(between_t),
                after: jk.proc.snapshot(),
            };
            print_iteration(&it);
            ctx.writer
                .record(&Record::Iteration(Box::new(it.clone())))?;
            ctx.results.push(it);
        }
        Ok(())
    })();

    let before = jk.proc.snapshot();
    let clean = jk.stop(&sandbox);
    ctx.writer.record(&Record::ProcessStop {
        build: build.label.clone(),
        scope: scope.into(),
        scenario: target.name.clone(),
        pid: jk.pid(),
        clean_exit: clean,
        before,
    })?;
    result
}

struct Press {
    sent_qpc: i64,
    visible_qpc: Option<i64>,
    foreground_qpc: Option<i64>,
    esc_qpc: Option<i64>,
    hidden_qpc: Option<i64>,
}

/// Close any jumpkey window still showing (a late overlay, or one Esc
/// didn't close): Esc until none is visible. Errors if it won't go away,
/// because every later measurement would be wrong.
fn dismiss_leftover_overlay(jk: &mut Jumpkey) -> Result<()> {
    let pid = jk.pid();
    for _ in 0..3 {
        if window::visible_windows_of_pid(pid) == 0 {
            return Ok(());
        }
        input::tap_escape()?;
        pump_until_every(HIDE_TIMEOUT, Duration::from_millis(20), || {
            window::visible_windows_of_pid(pid) == 0
        });
    }
    if window::visible_windows_of_pid(pid) == 0 {
        return Ok(());
    }
    bail!("a jumpkey window stays visible after Esc; stopping so it can't skew later presses")
}

/// Press the hotkey, wait for a jumpkey window to appear, dismiss it
/// with Esc and wait for it to disappear.
fn press_and_dismiss(
    jk: &mut Jumpkey,
    chord: &Chord,
    timeout: Duration,
    hold_ms: u64,
) -> Result<Press> {
    events::clear();
    let sent_qpc = qpc();
    input::send_chord(chord)?;
    let Some(shown) = jk.watcher.wait_show(timeout) else {
        // The search may still finish: give it as long again, and close
        // the overlay if it appears, so it can't sit over the target and
        // swallow the next presses. The press stays a timeout.
        if jk.watcher.wait_show(LATE_SHOW_GRACE).is_some() {
            dismiss_leftover_overlay(jk)?;
        }
        return Ok(Press {
            sent_qpc,
            visible_qpc: None,
            foreground_qpc: None,
            esc_qpc: None,
            hidden_qpc: None,
        });
    };

    // Today's overlay takes keyboard focus right after it appears; Esc
    // must not be sent before that, or it would land in the target app.
    // A future overlay that never takes focus reads keys through a hook
    // instead, so after a short wait we send Esc regardless.
    let pid = jk.pid();
    let mut foreground_qpc = None;
    pump_until(Duration::from_millis(500), || {
        let fg = window::foreground();
        if fg != 0 && window::window_pid(window::hwnd(fg)) == pid {
            foreground_qpc = Some(qpc());
            true
        } else {
            false
        }
    });
    if hold_ms > 0 {
        pump_sleep(Duration::from_millis(hold_ms));
    }

    let mut esc_qpc = qpc();
    input::tap_escape()?;
    let mut hidden = jk.watcher.wait_hide(shown.hwnd, HIDE_TIMEOUT);
    if hidden.is_none() && window::is_window(shown.hwnd) {
        // One retry: the first Esc may have raced the focus change.
        esc_qpc = qpc();
        input::tap_escape()?;
        hidden = jk.watcher.wait_hide(shown.hwnd, HIDE_TIMEOUT);
    }
    Ok(Press {
        sent_qpc,
        visible_qpc: Some(shown.qpc),
        foreground_qpc,
        esc_qpc: Some(esc_qpc),
        hidden_qpc: hidden.map(|h| h.qpc),
    })
}

fn measure_idle(
    ctx: &mut Ctx,
    jk: &Jumpkey,
    build: &BuildInfo,
    scope: &str,
    scenario: &str,
) -> Result<()> {
    println!("   idle {} s (build {})…", ctx.opts.idle_secs, build.label);
    let c0 = jk.proc.cycles();
    let q0 = qpc();
    let deadline = Duration::from_secs(ctx.opts.idle_secs);
    pump_until_every(deadline, SLOW_POLL, || STOP.load(Ordering::SeqCst));
    check_stop()?;
    let secs = qpc_delta_us(q0, qpc()) as f64 / 1e6;
    let cycles = match (c0, jk.proc.cycles()) {
        (Some(a), Some(b)) => b.saturating_sub(a),
        _ => 0,
    };
    let cpu_ms = cycles_to_ms(cycles);
    ctx.writer.record(&Record::Idle {
        build: build.label.clone(),
        scope: scope.into(),
        scenario: scenario.into(),
        secs,
        jumpkey_cycles: cycles,
        jumpkey_cpu_ms: cpu_ms,
        jumpkey_cpu_percent_of_one_core: cpu_ms.map(|ms| ms / (secs * 10.0)),
        after: jk.proc.snapshot(),
    })
}

fn open_target(
    sc: &Scenario,
    defaults: &crate::scenario::Defaults,
    ph: &Placeholders,
    origin: (i32, i32),
) -> Result<Option<Target>> {
    let Some(exe) = sc.resolve_exe(ph) else {
        eprintln!(
            "skipping scenario `{}`: none of its executables exist here",
            sc.name
        );
        return Ok(None);
    };
    let launched_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let before: HashSet<isize> = window::app_windows_of_exe(&sc.window_exe)
        .into_iter()
        .collect();
    std::process::Command::new(&exe)
        .args(sc.args(ph))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("launching {}", exe.display()))?;
    let (hwnd, opened_by_us) =
        match window::wait_new_window(&sc.window_exe, &before, Duration::from_secs(20)) {
            Some(h) => (h, true),
            None => match window::app_windows_of_exe(&sc.window_exe).first() {
                Some(&h) => {
                    eprintln!(
                        "warning: `{}` didn't open a new window; using the existing {} window",
                        sc.name, sc.window_exe
                    );
                    (h, false)
                }
                None => bail!("`{}`: no {} window appeared", sc.name, sc.window_exe),
            },
        };
    let root_pid = window::window_pid(window::hwnd(hwnd));
    let profile = sc.profile_dir(ph);
    // Ours to end only if it's a throwaway-profile instance whose process
    // started after we launched it (never the user's own browser).
    let owned_instance = (profile.is_some()
        && opened_by_us
        && process::created_unix_ms(root_pid).is_some_and(|c| c + 1000 >= launched_unix_ms))
    .then_some(root_pid);
    window::focus(hwnd)?;
    let r = sc.rect.unwrap_or(defaults.rect);
    let want = [origin.0 + r[0], origin.1 + r[1], r[2], r[3]];
    let got = window::place(hwnd, want);
    println!(
        "target: {} at {:?}, {} dpi",
        sc.window_exe,
        got,
        window::dpi(hwnd)
    );
    if got.is_some_and(|g| g.iter().zip(want).any(|(a, b)| (a - b).abs() > 16)) {
        eprintln!("warning: window landed at {got:?}, not the requested {want:?}");
    }
    pump_sleep(Duration::from_millis(
        sc.settle_ms.unwrap_or(defaults.settle_ms),
    ));
    Ok(Some(Target {
        name: sc.name.clone(),
        hwnd,
        opened_by_us,
        root_pid,
        close: sc.close,
        profile,
        owned_instance,
    }))
}

fn manual_target(delay_secs: u64) -> Result<Target> {
    println!("Focus the window to benchmark; using the foreground window in {delay_secs} s…");
    pump_sleep(Duration::from_secs(delay_secs));
    let hwnd = window::foreground();
    if hwnd == 0 {
        bail!("no foreground window");
    }
    let pid = window::window_pid(window::hwnd(hwnd));
    if pid == std::process::id() {
        bail!("the foreground window is pick-bench's own; focus the target app instead");
    }
    println!(
        "target: {} ({:?})",
        crate::win::process::exe_name(pid).unwrap_or_default(),
        window::rect(hwnd)
    );
    Ok(Target {
        name: "manual".into(),
        hwnd,
        opened_by_us: false,
        root_pid: pid,
        close: false,
        profile: None,
        owned_instance: None,
    })
}

fn refuse_if_jumpkey_running(builds: &[BuildInfo]) -> Result<()> {
    let mut names: Vec<String> = vec!["jumpkey.exe".into()];
    for b in builds {
        if let Some(n) = b.exe.file_name() {
            names.push(n.to_string_lossy().into_owned());
        }
    }
    names.sort();
    names.dedup();
    for n in &names {
        let pids = pids_of_exe(n);
        if !pids.is_empty() {
            bail!(
                "{n} is already running (pid {pids:?}). Only one jumpkey can run per session: quit \
                 it first (tray → Quit, or `jumpkey --close`), then rerun pick-bench."
            );
        }
    }
    Ok(())
}

/// `(name, chord)` for every entry of the template's `[hotkeys]` table.
fn template_chords(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path)?;
    let v: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v.get("hotkeys")
        .and_then(|h| h.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default())
}

/// Refuse to start if another app already owns any chord the sandboxed
/// jumpkey will register: jumpkey would show a blocking message box.
fn check_chords_free(
    scopes: &[String],
    template: &dyn Fn(&str) -> PathBuf,
    pressed: &[String],
) -> Result<()> {
    let mut chords: Vec<String> = pressed.to_vec();
    for scope in scopes {
        chords.extend(
            template_chords(&template(scope))?
                .into_iter()
                .map(|(_, c)| c),
        );
    }
    chords.sort();
    chords.dedup();
    let taken: Vec<&String> = chords
        .iter()
        .filter(|c| Chord::parse(c).map(|ch| !ch.is_free()).unwrap_or(false))
        .collect();
    if !taken.is_empty() {
        bail!(
            "these hotkeys are already registered by another app: {}. Close that app, or change \
             the chords in bench/config/*.toml (and --hotkey if you pass it).",
            taken
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn find_repo(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.canonicalize()?);
    }
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("bench").join("scenarios.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    // Built from this repo: fall back to where the crate lives.
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    if p.join("bench").join("scenarios.toml").is_file() {
        return Ok(p.canonicalize()?);
    }
    bail!("couldn't find the repo (bench/scenarios.toml); pass --repo")
}

/// `bench/fixtures/folder/`: 200 small files for the Explorer scenario.
/// Generated rather than checked in; names are fixed so every machine
/// gets the same listing.
fn ensure_fixture_folder(fixtures: &Path) -> Result<()> {
    let dir = fixtures.join("folder");
    std::fs::create_dir_all(&dir)?;
    for i in 1..=200 {
        let f = dir.join(format!("file-{i:03}.txt"));
        if !f.exists() {
            std::fs::write(&f, format!("pick-bench fixture file {i}\n"))?;
        }
    }
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

fn print_iteration(it: &Iteration) {
    let vis = it
        .hotkey_to_visible_us
        .map(|u| format!("{:7.1} ms", ms(u)))
        .unwrap_or_else(|| "  timeout ".into());
    println!(
        "   [{}] {:<22} #{:<3} visible {vis}  jumpkey cpu {:>6}  target cpu {:>6}",
        it.build,
        it.pattern,
        it.iter,
        it.jumpkey_cpu_ms
            .map(|v| format!("{v:.1}"))
            .unwrap_or("-".into()),
        it.target_cpu_ms
            .map(|v| format!("{v:.1}"))
            .unwrap_or("-".into()),
    );
}

fn print_table(ctx: &Ctx) {
    if ctx.results.is_empty() {
        return;
    }
    type Key = (String, String, String, String);
    let mut groups: BTreeMap<Key, Vec<&Iteration>> = BTreeMap::new();
    for it in &ctx.results {
        groups
            .entry((
                it.scope.clone(),
                it.scenario.clone(),
                it.pattern.clone(),
                it.build.clone(),
            ))
            .or_default()
            .push(it);
    }
    println!("\nhotkey → visible (ms), CPU per press (ms), jumpkey private bytes (MB)");
    println!(
        "{:<14} {:<14} {:<22} {:<5} {:>4} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>8} {:>4}",
        "scope",
        "scenario",
        "pattern",
        "build",
        "n",
        "min",
        "p50",
        "p95",
        "max",
        "jk cpu",
        "tgt cpu",
        "priv MB",
        "t/o"
    );
    let mut p50s: HashMap<(String, String, String), Vec<(String, f64)>> = HashMap::new();
    for ((scope, scen, pat, build), its) in &groups {
        let vis: Vec<f64> = its
            .iter()
            .filter_map(|i| i.hotkey_to_visible_us.map(ms))
            .collect();
        let jk: Vec<f64> = its.iter().filter_map(|i| i.jumpkey_cpu_ms).collect();
        let tg: Vec<f64> = its.iter().filter_map(|i| i.target_cpu_ms).collect();
        let priv_mb = its
            .iter()
            .filter_map(|i| i.after.private_bytes)
            .max()
            .map(|b| b as f64 / 1_048_576.0);
        let timeouts = its.iter().filter(|i| i.timed_out).count();
        let s = summarize(&vis).unwrap_or_default();
        let jk50 = summarize(&jk).map(|s| s.p50);
        let tg50 = summarize(&tg).map(|s| s.p50);
        println!(
            "{:<14} {:<14} {:<22} {:<5} {:>4} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8} {:>9} {:>8} {:>4}",
            scope,
            scen,
            pat,
            build,
            s.n,
            s.min,
            s.p50,
            s.p95,
            s.max,
            jk50.map(|v| format!("{v:.1}")).unwrap_or("-".into()),
            tg50.map(|v| format!("{v:.1}")).unwrap_or("-".into()),
            priv_mb.map(|v| format!("{v:.1}")).unwrap_or("-".into()),
            timeouts
        );
        p50s.entry((scope.clone(), scen.clone(), pat.clone()))
            .or_default()
            .push((build.clone(), s.p50));
    }
    if ctx.opts.builds.len() > 1 {
        println!("\nA/B: p50 change, b vs a (negative = b faster). Use telemetry_summary.py --compare for confidence intervals.");
        let mut keys: Vec<_> = p50s.keys().cloned().collect();
        keys.sort();
        for k in keys {
            let v = &p50s[&k];
            let a = v.iter().find(|(b, _)| b == "a").map(|x| x.1);
            let b = v.iter().find(|(b, _)| b == "b").map(|x| x.1);
            if let (Some(a), Some(b)) = (a, b) {
                let pct = if a > 0.0 { (b - a) / a * 100.0 } else { 0.0 };
                println!(
                    "  {} / {} / {}: {:+.1} ms ({:+.1} %)",
                    k.0,
                    k.1,
                    k.2,
                    b - a,
                    pct
                );
            }
        }
    }
    if !ctx.first_presses.is_empty() {
        println!("\nfirst press after start (one-time costs; not in the table above):");
        for (build, scope, scen, us) in &ctx.first_presses {
            println!("  [{build}] {scope} / {scen}: {:.1} ms", ms(*us));
        }
    }
}
