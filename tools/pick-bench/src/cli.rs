//! Command-line options (hand-rolled to keep dependencies minimal).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::scenario::Pattern;

pub const SCOPES: [&str; 3] = ["active_window", "active_monitor", "all_windows"];

#[derive(Debug, Clone)]
pub struct Options {
    /// `(label, exe)` pairs. One entry normally; two in A/B mode.
    pub builds: Vec<(String, PathBuf)>,
    pub scenarios: Vec<String>,
    pub all_scenarios: bool,
    /// `--preset baseline`: see [`Pattern::baseline_set`].
    pub baseline: bool,
    pub patterns: Vec<Pattern>,
    pub scopes: Vec<String>,
    pub iterations: usize,
    pub block_size: usize,
    pub hotkey: Option<String>,
    pub manual: bool,
    pub manual_delay_secs: u64,
    pub idle_secs: u64,
    pub hold_ms: u64,
    pub label: Option<String>,
    pub machine_label: Option<String>,
    pub out: Option<PathBuf>,
    pub scenario_file: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    pub keep_open: bool,
    pub press_during_startup: bool,
    pub monitor: Option<String>,
}

pub const HELP: &str = "\
pick-bench: black-box latency and resource benchmark for jumpkey's element picker

USAGE:
    pick-bench [OPTIONS]

WHAT TO RUN:
    --scenario <name>       Scenario from bench/scenarios.toml (repeatable)
    --all                   Every scenario x standard patterns x active_window + all_windows
                            (3-4 hours)
    --preset baseline       The baseline to compare refactors against (~30 min): every
                            scenario x switch-then-press:300/2000 + repeat-press:300 x
                            active_window, 30 presses each. --pattern / --scope /
                            --iterations still override.
    --pattern <p>           switch-then-press:<ms> | repeat-press:<ms> (repeatable;
                            default switch-then-press:2000)
    --scope <s>             active_window | active_monitor | all_windows (repeatable;
                            default active_window)
    --manual                Benchmark whatever window you focus (no launching)
    --manual-delay <secs>   Seconds to focus the target in --manual mode (default 5)

BUILDS:
    --exe <path>            jumpkey build to test (default: target/release/jumpkey.exe)
    --exe-a <path>          A/B mode: baseline build
    --exe-b <path>          A/B mode: candidate build (alternates with A in blocks)
    --block-size <n>        Iterations per block in A/B mode (default 5)

TUNING:
    --iterations <n>        Measured presses per scenario x pattern x scope x build (default 30)
    --hotkey <chord>        Element-picker chord (default: from bench/config/<scope>.toml)
    --idle-secs <n>         Idle measurement after jumpkey starts (default 10, 0 = off)
    --hold-ms <n>           Keep the overlay open this long before Esc (default 0)
    --monitor <n|primary>   Monitor to place target windows on (default primary). The
                            scenario rect is relative to that monitor's top-left corner.
                            Monitor numbers are printed at the start of each run.
    --keep-open             Don't close target windows afterwards
    --press-during-startup  Make the first press right after jumpkey starts (while the
                            splash is up) instead of waiting until startup is done, as
                            when pressing the hotkey right after launch at login

OUTPUT:
    --label <text>          Run label (used in the result folder name)
    --machine-label <text>  Free-form machine description, e.g. unraid-vm-4c-8g-nogpu
    --out <dir>             Results root (default: bench/results)
    --scenarios <file>      Scenario file (default: bench/scenarios.toml)
    --repo <dir>            Repository root (default: found from the current directory)

Quit any running jumpkey first: only one instance can run per session.
";

pub fn parse() -> Result<Option<Options>> {
    let mut o = Options {
        builds: Vec::new(),
        scenarios: Vec::new(),
        all_scenarios: false,
        baseline: false,
        patterns: Vec::new(),
        scopes: Vec::new(),
        iterations: 30,
        block_size: 5,
        hotkey: None,
        manual: false,
        manual_delay_secs: 5,
        idle_secs: 10,
        hold_ms: 0,
        label: None,
        machine_label: None,
        out: None,
        scenario_file: None,
        repo: None,
        keep_open: false,
        press_during_startup: false,
        monitor: None,
    };
    let mut exe = None;
    let mut exe_a = None;
    let mut exe_b = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--scenario" => o.scenarios.push(val()?),
            "--all" => o.all_scenarios = true,
            "--preset" => match val()?.as_str() {
                "baseline" => {
                    o.all_scenarios = true;
                    o.baseline = true;
                }
                other => bail!("unknown preset `{other}` (known: baseline)"),
            },
            "--pattern" => o.patterns.push(Pattern::parse(&val()?)?),
            "--scope" => {
                let s = val()?;
                if !SCOPES.contains(&s.as_str()) {
                    bail!(
                        "unknown scope `{s}` (expected one of {})",
                        SCOPES.join(", ")
                    );
                }
                o.scopes.push(s);
            }
            "--manual" => o.manual = true,
            "--manual-delay" => o.manual_delay_secs = val()?.parse()?,
            "--exe" => exe = Some(PathBuf::from(val()?)),
            "--exe-a" => exe_a = Some(PathBuf::from(val()?)),
            "--exe-b" => exe_b = Some(PathBuf::from(val()?)),
            "--block-size" => o.block_size = val()?.parse()?,
            "--iterations" => o.iterations = val()?.parse()?,
            "--hotkey" => o.hotkey = Some(val()?),
            "--idle-secs" => o.idle_secs = val()?.parse()?,
            "--hold-ms" => o.hold_ms = val()?.parse()?,
            "--keep-open" => o.keep_open = true,
            "--press-during-startup" => o.press_during_startup = true,
            "--monitor" => o.monitor = Some(val()?),
            "--label" => o.label = Some(val()?),
            "--machine-label" => o.machine_label = Some(val()?),
            "--out" => o.out = Some(PathBuf::from(val()?)),
            "--scenarios" => o.scenario_file = Some(PathBuf::from(val()?)),
            "--repo" => o.repo = Some(PathBuf::from(val()?)),
            other => bail!("unknown argument `{other}` (see --help)"),
        }
    }

    match (exe, exe_a, exe_b) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            bail!("use either --exe or --exe-a/--exe-b, not both")
        }
        (_, Some(a), Some(b)) => {
            o.builds = vec![("a".into(), a), ("b".into(), b)];
        }
        (_, Some(_), None) | (_, None, Some(_)) => bail!("A/B mode needs both --exe-a and --exe-b"),
        (e, None, None) => {
            o.builds = vec![("main".into(), e.unwrap_or_default())];
        }
    }
    if o.iterations == 0 || o.block_size == 0 {
        bail!("--iterations and --block-size must be at least 1");
    }
    if o.manual && (o.all_scenarios || !o.scenarios.is_empty()) {
        bail!("--manual can't be combined with --scenario / --all");
    }
    if !o.manual && !o.all_scenarios && o.scenarios.is_empty() {
        bail!("nothing to run: pass --scenario <name>, --all or --manual (see --help)");
    }
    if o.baseline && o.label.is_none() {
        o.label = Some("baseline".into());
    }
    if o.patterns.is_empty() {
        o.patterns = if o.baseline {
            Pattern::baseline_set()
        } else if o.all_scenarios {
            Pattern::standard_set()
        } else {
            vec![Pattern::SwitchThenPress { dwell_ms: 2000 }]
        };
    }
    if o.scopes.is_empty() {
        o.scopes = if o.all_scenarios && !o.baseline {
            vec!["active_window".into(), "all_windows".into()]
        } else {
            vec!["active_window".into()]
        };
    }
    Ok(Some(o))
}
