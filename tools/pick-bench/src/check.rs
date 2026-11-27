//! `--check`: live correctness checks of picking, from the outside.
//!
//! Where the benchmark always cancels with Esc, these cases type a real
//! label and verify what happened in the target: the right button was
//! clicked, no key leaked into the page, the target kept focus, menus
//! stayed open, a second hotkey press closed the labels instead of
//! queueing another pick. Each case runs `--repeat` times (default 10)
//! and passes only if every repetition passes.
//!
//! How the harness knows what happened, without jumpkey's internals:
//! - The fixture page `bench/fixtures/check.html` writes its state into
//!   its own title (`chk|last=click:btn-07|n=3|keys=0|…`).
//! - jumpkey (telemetry builds) writes each pick's labels and their
//!   rectangles to `JUMPKEY_BENCH_LABELS` (bench contract item 7). The
//!   harness reads a button's rectangle through UI Automation by its id
//!   and types the label drawn for that rectangle.
//! - Show / hide events as in the benchmark (contract item 4).
//!
//! `--expect <file>` compares the results with a JSON map of
//! `case → "pass" | "fail"` (see `bench/check/`), so known failures of
//! today's build are documented and a change in either direction shows.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::cli::Options;
use crate::jumpkey::{self, Jumpkey, Sandbox};
use crate::output::BuildInfo;
use crate::run::{
    check_chords_free, check_stop, close_target, dismiss_leftover_overlay, find_repo,
    install_ctrl_c, open_target, refuse_if_jumpkey_running, sanitize, template_chords, Target,
};
use crate::scenario::{Placeholders, ScenarioFile};
use crate::win::checkwin::CheckWindow;
use crate::win::events::{self, pump_sleep, pump_until, pump_until_every, Kind};
use crate::win::input::{self, Chord};
use crate::win::uia::Uia;
use crate::win::window::{self, NeutralWindow};
use crate::win::{self as w};

/// Every case, in run order.
pub const CASES: &[&str] = &[
    "pick-click",
    "pick-modifiers",
    "prefix-backspace",
    "esc-cancel",
    "focus-kept",
    "toggle",
    "toggle-during-search",
    "rapid-x3",
    "wheel-cancel",
    "click-cancel",
    "foreground-cancel",
    "window-pick",
    "lone-alt",
    "menu-win32",
];

const SHOW_TIMEOUT: Duration = Duration::from_secs(5);
const HIDE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longer than jumpkey's default cache lifetime (500 ms), so a press
/// that should search really searches.
const CACHE_EXPIRY: Duration = Duration::from_millis(700);
/// Rectangles from the labels file and from UI Automation are the same
/// property read twice; allow a little for rounding.
const RECT_SLACK: i32 = 3;

struct Rig {
    jk: Jumpkey,
    target: Target,
    neutral: NeutralWindow,
    checkwin: Option<CheckWindow>,
    uia: Uia,
    labels: LabelsFile,
    element: Chord,
    window: Chord,
}

/// One repetition's result.
#[derive(Debug, Serialize)]
struct Outcome {
    case: String,
    rep: usize,
    pass: bool,
    /// What failed (or which variant ran), for the log. Never contains
    /// window titles other than the fixture's own state fields.
    detail: String,
}

pub fn run(opts: Options) -> Result<()> {
    w::init_dpi();
    install_ctrl_c();
    let repo = find_repo(opts.repo.as_deref())?;
    let cases: Vec<String> = if opts.check.is_empty() {
        CASES.iter().map(|s| s.to_string()).collect()
    } else {
        for c in &opts.check {
            if !CASES.contains(&c.as_str()) {
                bail!("unknown check `{c}` (known: {})", CASES.join(", "));
            }
        }
        opts.check.clone()
    };
    let expect: Option<BTreeMap<String, String>> = match &opts.expect {
        Some(p) => Some(
            serde_json::from_str(
                &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            )
            .with_context(|| format!("parsing {}", p.display()))?,
        ),
        None => None,
    };

    let (label, exe) = opts.builds.first().cloned().context("no build")?;
    let exe = if exe.as_os_str().is_empty() {
        repo.join("target").join("release").join("jumpkey.exe")
    } else {
        exe
    };
    let exe = PathBuf::from(
        exe.canonicalize()
            .with_context(|| format!("jumpkey build not found: {}", exe.display()))?
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    let build = BuildInfo {
        label,
        version: jumpkey::version(&exe),
        exe_bytes: None,
        exe_modified_unix: None,
        exe: exe.clone(),
    };
    refuse_if_jumpkey_running(std::slice::from_ref(&build))?;

    let template = repo.join("bench").join("config").join("check.toml");
    let chords = template_chords(&template)?;
    let chord = |name: &str| -> Result<Chord> {
        let text = chords
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.clone())
            .with_context(|| format!("{} has no hotkeys.{name}", template.display()))?;
        Chord::parse(&text)
    };
    let element = chord("pick_element")?;
    let window_chord = chord("pick_window")?;
    let tmpl = template.clone();
    check_chords_free(
        &["check".to_string()],
        &move |_| tmpl.clone(),
        std::slice::from_ref(&element.text),
    )?;

    let run_id = format!(
        "{}-check{}",
        w::local_timestamp(),
        opts.label
            .as_deref()
            .map(|l| format!("-{}", sanitize(l)))
            .unwrap_or_default()
    );
    let dir = opts
        .out
        .clone()
        .unwrap_or_else(|| repo.join("bench").join("results"))
        .join(&run_id);
    std::fs::create_dir_all(&dir)?;
    let mut log = std::io::BufWriter::new(std::fs::File::create(dir.join("check.jsonl"))?);
    println!("pick-bench check {run_id}");
    println!("results: {}", dir.display());
    println!("Hands off the keyboard and mouse while it runs. Ctrl+C to stop.\n");

    let scenarios = ScenarioFile::load(&repo.join("bench").join("scenarios.toml"))?;
    let ph = Placeholders {
        fixtures: repo.join("bench").join("fixtures"),
        repo: repo.clone(),
        profiles: Some(dir.join("profiles")),
    };
    let neutral = NeutralWindow::create([40, 40, 480, 320])?;
    let target = open_target(
        scenarios.get("check-page")?,
        &scenarios.defaults,
        &ph,
        (0, 0),
    )?
    .context("check-page needs Vivaldi installed")?;

    let mut sandbox = Sandbox::create(&dir.join("sandbox"), &template)?;
    let labels_path = sandbox.localappdata.join("labels.jsonl");
    sandbox
        .env
        .push(("JUMPKEY_BENCH_LABELS", labels_path.clone().into_os_string()));
    let mut jk = Jumpkey::spawn(&exe, &sandbox, &run_id)?;
    let startup = jk.wait_startup(Duration::from_secs(20));
    let mut rig = Rig {
        jk,
        target,
        neutral,
        checkwin: None,
        uia: Uia::new()?,
        labels: LabelsFile::new(labels_path),
        element,
        window: window_chord,
    };

    let mut results: BTreeMap<String, (usize, usize, Vec<String>)> = BTreeMap::new();
    let outcome = (|| -> Result<()> {
        startup?;
        // The first UI Automation request wakes Chromium's accessibility
        // tree, which takes a moment to build.
        let page = rig.target.hwnd;
        if !pump_until_every(Duration::from_secs(10), Duration::from_millis(200), || {
            rig.uia.rect_of_id(page, "btn-01").is_some()
        }) {
            bail!("UI Automation never found the fixture's buttons");
        }
        for case in &cases {
            for rep in 0..opts.repeat {
                check_stop()?;
                if !rig.jk.is_running() {
                    bail!("jumpkey exited during the checks");
                }
                let r = run_case(&mut rig, case, rep);
                // Leave nothing behind for the next case.
                cleanup(&mut rig)?;
                let (pass, detail) = match r {
                    Ok(Ok(note)) => (true, note),
                    Ok(Err(why)) => (false, why),
                    Err(e) => (false, format!("error: {e:#}")),
                };
                println!(
                    "   {:<22} #{rep:<2} {}  {detail}",
                    case,
                    if pass { "pass" } else { "FAIL" }
                );
                let o = Outcome {
                    case: case.clone(),
                    rep,
                    pass,
                    detail: detail.clone(),
                };
                serde_json::to_writer(&mut log, &o)?;
                log.write_all(b"\n")?;
                log.flush()?;
                let e = results.entry(case.clone()).or_default();
                e.1 += 1;
                if pass {
                    e.0 += 1;
                } else if !e.2.contains(&detail) {
                    e.2.push(detail);
                }
            }
        }
        Ok(())
    })();

    rig.jk.stop(&sandbox);
    drop(rig.checkwin.take());
    if !opts.keep_open {
        close_target(&rig.target);
    }
    let _ = std::fs::remove_dir_all(dir.join("profiles"));

    println!("\ncase                    passed  expected  status");
    let mut mismatches = 0;
    for case in &cases {
        let Some((pass, total, fails)) = results.get(case) else {
            continue;
        };
        let got = if pass == total {
            "pass"
        } else if *pass == 0 {
            "fail"
        } else {
            "flaky"
        };
        let want = expect
            .as_ref()
            .and_then(|m| m.get(case))
            .map(|s| s.as_str());
        let status = match want {
            None => "",
            Some(wnt) if wnt == got => "as expected",
            Some(_) => {
                mismatches += 1;
                "CHANGED"
            }
        };
        println!(
            "{case:<23} {pass:>3}/{total:<3} {:<9} {status}",
            want.unwrap_or("-")
        );
        for f in fails.iter().take(3) {
            println!("{:<23}   - {f}", "");
        }
    }
    outcome?;
    if mismatches > 0 {
        bail!(
            "{mismatches} check(s) differ from {}",
            opts.expect.as_ref().unwrap().display()
        );
    }
    Ok(())
}

/// `Ok(Ok(note))` = pass, `Ok(Err(why))` = fail, `Err` = harness error.
type CaseResult = Result<std::result::Result<String, String>>;

fn run_case(rig: &mut Rig, case: &str, rep: usize) -> CaseResult {
    match case {
        "pick-click" => pick_click(rig, rep),
        "pick-modifiers" => pick_modifiers(rig, rep),
        "prefix-backspace" => prefix_backspace(rig, rep),
        "esc-cancel" => esc_cancel(rig),
        "focus-kept" => focus_kept(rig),
        "toggle" => toggle(rig),
        "toggle-during-search" => toggle_during_search(rig),
        "rapid-x3" => rapid_x3(rig),
        "wheel-cancel" => wheel_cancel(rig),
        "click-cancel" => click_cancel(rig),
        "foreground-cancel" => foreground_cancel(rig),
        "window-pick" => window_pick(rig),
        "lone-alt" => lone_alt(rig),
        "menu-win32" => menu_win32(rig),
        other => bail!("no such case {other}"),
    }
}

macro_rules! fail {
    ($($t:tt)*) => { return Ok(Err(format!($($t)*))) };
}

// ---- shared steps ----------------------------------------------------------

/// The fixture page's state fields, from its title.
fn page_state(hwnd: isize) -> BTreeMap<String, String> {
    let t = window::title(hwnd);
    let Some(rest) = t.split("chk|").nth(1) else {
        return BTreeMap::new();
    };
    // Vivaldi appends " - Vivaldi" to the page title.
    let rest = rest.split(" - ").next().unwrap_or(rest);
    rest.split('|')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn num(s: &BTreeMap<String, String>, k: &str) -> i64 {
    s.get(k).and_then(|v| v.parse().ok()).unwrap_or(-1)
}

/// Focus the fixture page and let it settle; wait out the cache.
fn ready_page(rig: &mut Rig) -> Result<BTreeMap<String, String>> {
    window::focus(rig.target.hwnd)?;
    pump_sleep(CACHE_EXPIRY);
    let s = page_state(rig.target.hwnd);
    if s.is_empty() {
        bail!("can't read the fixture page's state (is check.html loaded?)");
    }
    Ok(s)
}

/// Press a chord and wait for a jumpkey window to show.
fn press_and_show(rig: &mut Rig, chord: &Chord) -> Result<Option<isize>> {
    events::clear();
    input::send_chord(chord)?;
    Ok(rig.jk.watcher.wait_show(SHOW_TIMEOUT).map(|e| e.hwnd))
}

fn wait_hidden(rig: &mut Rig, overlay: isize, timeout: Duration) -> bool {
    rig.jk.watcher.wait_hide(overlay, timeout).is_some()
        || !window::is_window(overlay)
        || window::visible_windows_of_pid(rig.jk.pid()) == 0
}

/// Wait until the page's action counter moves past `n0`.
fn wait_action(hwnd: isize, n0: i64, timeout: Duration) -> Option<BTreeMap<String, String>> {
    let mut got = None;
    pump_until_every(timeout, Duration::from_millis(10), || {
        let s = page_state(hwnd);
        if num(&s, "n") > n0 {
            got = Some(s);
            true
        } else {
            false
        }
    });
    got
}

/// The label drawn for `rect`, from the newest labels line.
fn label_for(labels: &[(String, [i32; 4])], rect: [i32; 4]) -> Option<String> {
    labels
        .iter()
        .find(|(_, r)| r.iter().zip(rect).all(|(a, b)| (a - b).abs() <= RECT_SLACK))
        .map(|(l, _)| l.clone())
}

fn button_id(rep: usize, salt: usize) -> String {
    format!("btn-{:02}", (rep * 7 + salt * 11) % 30 + 1)
}

/// Show labels and find the label for fixture element `id`.
fn show_and_find(rig: &mut Rig, id: &str) -> Result<std::result::Result<(isize, String), String>> {
    let Some(rect) = rig.uia.rect_of_id(rig.target.hwnd, id) else {
        bail!("UI Automation can't find #{id} on the fixture page");
    };
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        return Ok(Err("no overlay".into()));
    };
    let Some(labels) = rig.labels.next(Duration::from_secs(1)) else {
        return Ok(Err(
            "no labels line (telemetry build with JUMPKEY_BENCH_LABELS?)".into(),
        ));
    };
    match label_for(&labels, rect) {
        Some(l) => Ok(Ok((overlay, l))),
        None => Ok(Err(format!("no label over #{id}"))),
    }
}

/// After every case: no overlay, no menu, no stuck modifier.
fn cleanup(rig: &mut Rig) -> Result<()> {
    for vk in [0x10u16, 0x11, 0x12] {
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) }
            as u16
            & 0x8000
            != 0
        {
            input::key(vk, true)?;
        }
    }
    if let Some(cw) = &rig.checkwin {
        for _ in 0..3 {
            if !cw.popup_open() && !cw.in_menu_mode() {
                break;
            }
            input::tap_escape()?;
            pump_sleep(Duration::from_millis(150));
        }
    }
    unstick(rig)?;
    dismiss_leftover_overlay(&mut rig.jk)?;
    // A queued pick may still open (today's behaviour): give it a moment
    // and close it too, so it can't leak into the next case.
    if rig
        .jk
        .watcher
        .wait_show(Duration::from_millis(400))
        .is_some()
    {
        pump_sleep(Duration::from_millis(100));
        dismiss_leftover_overlay(&mut rig.jk)?;
    }
    rig.labels.skip_to_end();
    events::clear();
    Ok(())
}

/// Today's overlay can show without getting keyboard focus (seen with a
/// Win32 popup menu open): Esc never reaches it and it stays up. The
/// wheel still closes it (its mouse hook doesn't need focus), so use
/// that before giving up.
fn unstick(rig: &mut Rig) -> Result<bool> {
    let pid = rig.jk.pid();
    if window::visible_windows_of_pid(pid) == 0 {
        return Ok(false);
    }
    input::tap_escape()?;
    if pump_until_every(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || window::visible_windows_of_pid(pid) == 0,
    ) {
        return Ok(false);
    }
    let saved = input::pointer();
    if let Some(r) = window::rect(rig.neutral.hwnd()) {
        input::wheel_at(r[0] + r[2] / 2, r[1] + r[3] / 2)?;
    }
    pump_until_every(HIDE_TIMEOUT, Duration::from_millis(20), || {
        window::visible_windows_of_pid(pid) == 0
    });
    input::move_pointer(saved.0, saved.1)?;
    Ok(true)
}

// ---- cases -----------------------------------------------------------------

/// Type a real label: the right button is clicked and no key reaches the page.
fn pick_click(rig: &mut Rig, rep: usize) -> CaseResult {
    let s0 = ready_page(rig)?;
    let id = button_id(rep, 0);
    let (overlay, label) = match show_and_find(rig, &id)? {
        Ok(x) => x,
        Err(why) => fail!("{why}"),
    };
    input::type_text(&label)?;
    if !wait_hidden(rig, overlay, HIDE_TIMEOUT) {
        fail!("overlay still showing after the label");
    }
    let Some(s1) = wait_action(rig.target.hwnd, num(&s0, "n"), HIDE_TIMEOUT) else {
        fail!("no action reached the page");
    };
    let want = format!("click:{id}");
    if s1.get("last") != Some(&want) {
        fail!("wrong action: {:?}, wanted {want}", s1.get("last"));
    }
    if num(&s1, "keys") != num(&s0, "keys") {
        fail!(
            "{} key(s) leaked into the page",
            num(&s1, "keys") - num(&s0, "keys")
        );
    }
    Ok(Ok(format!("{} chars", label.chars().count())))
}

/// Shift on the last key right-clicks, Ctrl (and Ctrl+Shift) double-clicks.
fn pick_modifiers(rig: &mut Rig, rep: usize) -> CaseResult {
    const SHIFT: u16 = 0x10;
    const CTRL: u16 = 0x11;
    let (mods, want_kind, name): (&[u16], &str, &str) = match rep % 3 {
        0 => (&[SHIFT], "ctx", "shift"),
        1 => (&[CTRL], "dbl", "ctrl"),
        _ => (&[CTRL, SHIFT], "dbl", "ctrl+shift"),
    };
    let s0 = ready_page(rig)?;
    let id = button_id(rep, 1);
    let (overlay, label) = match show_and_find(rig, &id)? {
        Ok(x) => x,
        Err(why) => fail!("{why}"),
    };
    let mut chars: Vec<char> = label.chars().collect();
    let last = chars.pop().expect("labels aren't empty");
    input::type_text(&chars.iter().collect::<String>())?;
    for &m in mods {
        input::key(m, false)?;
    }
    let vk = input::vk_for_char(last).context("label key")?;
    input::tap(vk)?;
    pump_sleep(Duration::from_millis(30));
    for &m in mods.iter().rev() {
        input::key(m, true)?;
    }
    if !wait_hidden(rig, overlay, HIDE_TIMEOUT) {
        fail!("{name}: overlay still showing");
    }
    // A double-click is reported after its two clicks: wait for it.
    let want = format!("{want_kind}:{id}");
    let mut last_seen = None;
    let ok = pump_until_every(HIDE_TIMEOUT, Duration::from_millis(10), || {
        let s = page_state(rig.target.hwnd);
        last_seen = s.get("last").cloned();
        num(&s, "n") > num(&s0, "n") && s.get("last") == Some(&want)
    });
    if !ok {
        fail!("{name}: got {last_seen:?}, wanted {want}");
    }
    Ok(Ok(name.into()))
}

/// Backspace undoes a typed prefix; then another label still works.
fn prefix_backspace(rig: &mut Rig, rep: usize) -> CaseResult {
    let s0 = ready_page(rig)?;
    let id = button_id(rep, 2);
    let (overlay, label) = match show_and_find(rig, &id)? {
        Ok(x) => x,
        Err(why) => fail!("{why}"),
    };
    // A multi-char label of some other element, if there is one: type its
    // first char, then Backspace. Otherwise Backspace on an empty prefix.
    let labels = rig.labels.last.clone();
    let other = labels
        .iter()
        .map(|(l, _)| l)
        .find(|l| l.chars().count() >= 2 && **l != label)
        .cloned();
    let variant = match &other {
        Some(o) => {
            input::type_text(&o.chars().next().unwrap().to_string())?;
            "prefix then backspace"
        }
        None => "backspace on empty",
    };
    pump_sleep(Duration::from_millis(50));
    input::tap(0x08)?;
    pump_sleep(Duration::from_millis(50));
    input::type_text(&label)?;
    if !wait_hidden(rig, overlay, HIDE_TIMEOUT) {
        fail!("{variant}: overlay still showing");
    }
    let Some(s1) = wait_action(rig.target.hwnd, num(&s0, "n"), HIDE_TIMEOUT) else {
        fail!("{variant}: no action reached the page");
    };
    let want = format!("click:{id}");
    if s1.get("last") != Some(&want) {
        fail!("{variant}: got {:?}, wanted {want}", s1.get("last"));
    }
    if num(&s1, "keys") != num(&s0, "keys") {
        fail!("{variant}: keys leaked into the page");
    }
    Ok(Ok(variant.into()))
}

/// Esc closes the labels, does nothing else, and doesn't reach the page.
fn esc_cancel(rig: &mut Rig) -> CaseResult {
    let s0 = ready_page(rig)?;
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay");
    };
    pump_sleep(Duration::from_millis(150));
    input::tap_escape()?;
    if !wait_hidden(rig, overlay, HIDE_TIMEOUT) {
        fail!("overlay still showing after Esc");
    }
    pump_sleep(Duration::from_millis(300));
    let s1 = page_state(rig.target.hwnd);
    if num(&s1, "n") != num(&s0, "n") {
        fail!("an action happened: {:?}", s1.get("last"));
    }
    if num(&s1, "esc") != num(&s0, "esc") {
        fail!("Esc reached the page");
    }
    Ok(Ok(String::new()))
}

/// The target keeps the foreground while labels show.
fn focus_kept(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay");
    };
    let mut lost = None;
    pump_until(Duration::from_millis(300), || {
        let fg = window::foreground();
        if fg != rig.target.hwnd {
            lost = Some(fg);
            true
        } else {
            false
        }
    });
    input::tap_escape()?;
    wait_hidden(rig, overlay, HIDE_TIMEOUT);
    if let Some(fg) = lost {
        let who = if window::window_pid(window::hwnd(fg)) == rig.jk.pid() {
            "jumpkey"
        } else {
            "another window"
        };
        fail!("foreground moved to {who} while labels showed");
    }
    Ok(Ok(String::new()))
}

/// A second press of the hotkey closes the labels and opens nothing.
fn toggle(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let chord = rig.element.clone();
    let Some(overlay) = press_and_show(rig, &chord)? else {
        fail!("no overlay");
    };
    pump_sleep(Duration::from_millis(200));
    events::clear();
    input::send_chord(&chord)?;
    let closed = wait_hidden(rig, overlay, Duration::from_millis(500));
    if !closed {
        input::tap_escape()?;
        wait_hidden(rig, overlay, HIDE_TIMEOUT);
    }
    let reopened = rig
        .jk
        .watcher
        .wait_show(Duration::from_millis(1500))
        .is_some();
    match (closed, reopened) {
        (true, false) => Ok(Ok(String::new())),
        (false, true) => fail!("second press didn't close; a queued pick opened after Esc"),
        (false, false) => fail!("second press didn't close"),
        (true, true) => fail!("closed, then another pick opened"),
    }
}

/// A second press while the search runs cancels it: nothing stays open.
fn toggle_during_search(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let chord = rig.element.clone();
    events::clear();
    input::send_chord(&chord)?;
    pump_sleep(Duration::from_millis(10));
    input::send_chord(&chord)?;
    let mut shows = 0;
    pump_until(Duration::from_secs(2), || {
        shows += events::take(Kind::Show);
        false
    });
    let open = window::visible_windows_of_pid(rig.jk.pid()) > 0;
    match (shows, open) {
        (0, false) => Ok(Ok("cancelled before showing".into())),
        (1, false) => Ok(Ok("shown, then closed".into())),
        (_, true) => fail!("labels still open 2 s after the second press ({shows} show(s))"),
        (n, false) => fail!("{n} shows"),
    }
}

/// Three quick presses: open, close, open. One overlay, nothing queued.
fn rapid_x3(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let chord = rig.element.clone();
    events::clear();
    for i in 0..3 {
        if i > 0 {
            pump_sleep(Duration::from_millis(40));
        }
        input::send_chord(&chord)?;
    }
    let mut shows = 0;
    pump_until(Duration::from_millis(2500), || {
        shows += events::take(Kind::Show);
        false
    });
    let open = window::visible_windows_of_pid(rig.jk.pid()) > 0;
    // Close what's open; nothing else may follow.
    input::tap_escape()?;
    pump_until_every(HIDE_TIMEOUT, Duration::from_millis(20), || {
        window::visible_windows_of_pid(rig.jk.pid()) == 0
    });
    events::clear();
    let later = rig
        .jk
        .watcher
        .wait_show(Duration::from_millis(1500))
        .is_some();
    if later {
        fail!("another pick opened after Esc (queued presses)");
    }
    match (shows, open) {
        (1, true) => Ok(Ok(String::new())),
        (n, o) => fail!("{n} show(s), open at the end: {o}"),
    }
}

/// The mouse wheel closes the labels.
fn wheel_cancel(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let saved = input::pointer();
    let r = window::rect(rig.target.hwnd).context("target rect")?;
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay");
    };
    pump_sleep(Duration::from_millis(150));
    input::wheel_at(r[0] + r[2] - 120, r[1] + r[3] / 2)?;
    let closed = wait_hidden(rig, overlay, Duration::from_secs(1));
    input::move_pointer(saved.0, saved.1)?;
    if !closed {
        fail!("overlay still showing after the wheel");
    }
    Ok(Ok(String::new()))
}

/// A click closes the labels and reaches the app.
fn click_cancel(rig: &mut Rig) -> CaseResult {
    let s0 = ready_page(rig)?;
    let saved = input::pointer();
    let pad = rig
        .uia
        .rect_of_id(rig.target.hwnd, "pad")
        .context("UI Automation can't find #pad")?;
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay");
    };
    pump_sleep(Duration::from_millis(150));
    input::click_at(pad[0] + pad[2] / 2, pad[1] + pad[3] - 10)?;
    let closed = wait_hidden(rig, overlay, Duration::from_secs(1));
    let s1 = wait_action(rig.target.hwnd, num(&s0, "n"), Duration::from_secs(1));
    input::move_pointer(saved.0, saved.1)?;
    if !closed {
        fail!("overlay still showing after the click");
    }
    match s1.as_ref().and_then(|s| s.get("last")) {
        Some(l) if l == "click:pad" => Ok(Ok(String::new())),
        other => fail!("the click didn't reach the page ({other:?})"),
    }
}

/// Switching to another window closes the labels.
fn foreground_cancel(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay");
    };
    pump_sleep(Duration::from_millis(150));
    window::focus(rig.neutral.hwnd())?;
    let closed = wait_hidden(rig, overlay, Duration::from_secs(1));
    if !closed {
        fail!("overlay still showing after a window switch");
    }
    Ok(Ok(String::new()))
}

/// The window picker focuses the chosen window.
fn window_pick(rig: &mut Rig) -> CaseResult {
    ready_page(rig)?;
    let want = rig.neutral.hwnd();
    let wr = window::rect(want).context("neutral window rect")?;
    let Some(overlay) = press_and_show(rig, &rig.window.clone())? else {
        fail!("no overlay");
    };
    let Some(labels) = rig.labels.next(Duration::from_secs(1)) else {
        fail!("no labels line");
    };
    // Window labels are drawn for the visible frame, a few pixels inside
    // the window rect: take the closest.
    let dist = |r: &[i32; 4]| r.iter().zip(wr).map(|(a, b)| (a - b).abs()).sum::<i32>();
    let Some((label, r)) = labels.iter().min_by_key(|(_, r)| dist(r)) else {
        fail!("empty labels line");
    };
    if dist(r) > 64 {
        fail!("no label for the neutral window");
    }
    input::type_text(label)?;
    if !wait_hidden(rig, overlay, HIDE_TIMEOUT) {
        fail!("overlay still showing");
    }
    if !pump_until(Duration::from_secs(1), || window::foreground() == want) {
        fail!("chosen window didn't get the foreground");
    }
    Ok(Ok(String::new()))
}

fn checkwin(rig: &mut Rig) -> Result<isize> {
    if rig.checkwin.is_none() {
        rig.checkwin = Some(CheckWindow::create([140, 140, 700, 360])?);
        pump_sleep(Duration::from_millis(300));
    }
    Ok(rig.checkwin.as_ref().unwrap().hwnd())
}

/// A lone Alt tap while labels show doesn't put the app in menu-bar mode.
fn lone_alt(rig: &mut Rig) -> CaseResult {
    let h = checkwin(rig)?;
    window::focus(h)?;
    pump_sleep(CACHE_EXPIRY);
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay over the check window");
    };
    pump_sleep(Duration::from_millis(150));
    input::tap(0x12)?;
    pump_sleep(Duration::from_millis(250));
    let during = rig.checkwin.as_ref().unwrap().in_menu_mode();
    input::tap_escape()?;
    wait_hidden(rig, overlay, HIDE_TIMEOUT);
    pump_sleep(Duration::from_millis(200));
    let after = rig.checkwin.as_ref().unwrap().in_menu_mode();
    if during || after {
        fail!("menu-bar mode after a lone Alt (during labels: {during}, after: {after})");
    }
    Ok(Ok(String::new()))
}

/// An open popup menu stays open while labels show, and after Esc.
fn menu_win32(rig: &mut Rig) -> CaseResult {
    let h = checkwin(rig)?;
    window::focus(h)?;
    pump_sleep(CACHE_EXPIRY);
    let cw = rig.checkwin.as_ref().unwrap();
    cw.open_popup();
    if !pump_until(Duration::from_secs(2), || {
        rig.checkwin.as_ref().unwrap().popup_open()
    }) {
        bail!("the check window's popup menu didn't open");
    }
    pump_sleep(Duration::from_millis(200));
    let Some(overlay) = press_and_show(rig, &rig.element.clone())? else {
        fail!("no overlay while the menu was open");
    };
    pump_sleep(Duration::from_millis(250));
    let during = rig.checkwin.as_ref().unwrap().popup_open();
    input::tap_escape()?;
    let hidden = wait_hidden(rig, overlay, HIDE_TIMEOUT);
    pump_sleep(Duration::from_millis(250));
    let after = rig.checkwin.as_ref().unwrap().popup_open();
    match (during, hidden, after) {
        (true, true, true) => Ok(Ok(String::new())),
        (false, _, _) => fail!("the menu closed when the labels appeared"),
        (true, false, _) => {
            fail!("labels stuck: Esc went to the menu, not jumpkey (menu open after: {after})")
        }
        (true, true, false) => fail!("Esc closed the menu as well as the labels"),
    }
}

// ---- labels file -------------------------------------------------------

/// Reads the labels file jumpkey appends to, one pick per line.
struct LabelsFile {
    path: PathBuf,
    /// Lines consumed so far.
    seen: usize,
    /// The newest line read.
    last: Vec<(String, [i32; 4])>,
}

impl LabelsFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            seen: 0,
            last: Vec::new(),
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::File::open(path)
            .map(|f| {
                std::io::BufReader::new(f)
                    .lines()
                    .map_while(|l| l.ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The next unread line, waiting up to `timeout` for it.
    fn next(&mut self, timeout: Duration) -> Option<Vec<(String, [i32; 4])>> {
        let mut got = None;
        let path = self.path.clone();
        let seen = self.seen;
        pump_until_every(timeout, Duration::from_millis(10), || {
            let lines = Self::lines(&path);
            if lines.len() > seen {
                got = Some((lines.len(), lines.last().cloned().unwrap_or_default()));
                true
            } else {
                false
            }
        });
        let (count, line) = got?;
        self.seen = count;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        let labels: Vec<(String, [i32; 4])> = v["labels"]
            .as_array()?
            .iter()
            .filter_map(|e| {
                let l = e["label"].as_str()?.to_string();
                let r = e["rect"].as_array()?;
                let n = |i: usize| r.get(i).and_then(|x| x.as_i64()).map(|x| x as i32);
                Some((l, [n(0)?, n(1)?, n(2)?, n(3)?]))
            })
            .collect();
        self.last = labels.clone();
        Some(labels)
    }

    fn skip_to_end(&mut self) {
        self.seen = Self::lines(&self.path).len();
    }
}
