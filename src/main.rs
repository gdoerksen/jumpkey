//! `jumpkey` — system-wide keyboard navigation overlay.
//!
//! Two leader chords:
//!
//! - `Ctrl + Shift + Space` — pick an interactable element inside the
//!   currently-focused window and invoke it.
//! - `Ctrl + Alt + Space` — pick a top-level window across all monitors
//!   and bring it to the foreground.
//!
//! While the element overlay is open, modifier keys on the *completing*
//! hint keystroke change the action:
//!
//! - **Ctrl + letter** — double-click the element.
//! - **Shift + letter** — right-click (context menu) the element.
//! - **No modifier** — standard invoke (UIA pattern cascade / left-click
//!   fallback).
//!
//! Both actions are also exposed through a system tray icon (right-click
//! the yellow "K" badge in the notification area). `Esc` cancels either
//! overlay; the tray's `Quit` entry exits the message loop.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

// Everything below runs only in the Windows app; other targets build a
// stub `main` (see `run_windows`), so these imports are Windows-only too.
#[cfg(windows)]
use jumpkey::config::ScopeMode;
#[cfg(windows)]
use jumpkey::scroll::{Chords, ExitReason, PickKind};
#[cfg(windows)]
use jumpkey::telemetry::Pick;
#[cfg(windows)]
use jumpkey::{tel_event, tel_record, tel_span};
#[cfg(windows)]
use jumpkey::{Action, Backend, Config, Element, HintEngine, HintOrder};

#[cfg(windows)]
use jumpkey::windows::{
    config_watcher::{self, WM_USER_RELOAD_CONFIG},
    hotkey::{HotkeyAction, HotkeyConflict, Hotkeys},
    ipc, notification,
    overlay::{pick_hint, Hint, HintStyle, PickModifier},
    scroll::{
        indicator::Indicator,
        session::{FollowUp, ScrollSession, StartError},
    },
    settings_window,
    single_instance::InstanceGuard,
    splash_screen::SplashScreen,
    startup,
    tray::{Tray, TrayCommand},
    window_picker,
};

/// Parsed command-line flags. Hand-rolled so we don't pull in `clap` for
/// a handful of options.
#[derive(Debug, Default, Clone, Copy)]
struct Cli {
    no_tray: bool,
    /// Ask any running jumpkey in this user session to shut down cleanly,
    /// then exit. See [`ipc::send_close_signal`] for the wire-level
    /// mechanism. Mutually exclusive with the normal startup path —
    /// when set, we never even acquire the single-instance mutex.
    close: bool,
    /// Delete the on-disk log files in `%LOCALAPPDATA%\jumpkey\` and
    /// exit. Useful when triaging a recurring issue ("clear, repro,
    /// share log") or when the file has grown larger than the user
    /// is comfortable keeping around.
    clear_logs: bool,
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(Some(cli)) => cli,
        Ok(None) => return ExitCode::SUCCESS, // --help / --version handled
        Err(code) => return code,
    };

    // One-shot maintenance commands run before tracing init so the
    // closer / clear-logs paths don't themselves write a single line to
    // the log they're trying to either send-to or delete.
    #[cfg(windows)]
    if cli.close {
        return run_close_command();
    }
    #[cfg(windows)]
    if cli.clear_logs {
        return run_clear_logs_command();
    }

    // Held until exit: dropping it flushes telemetry (feature builds).
    let _tracing = init_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "jumpkey starting");

    let result: anyhow::Result<()>;
    #[cfg(windows)]
    {
        result = run_windows(cli);
    }
    #[cfg(not(windows))]
    {
        let _ = cli;
        result = Err(anyhow::anyhow!(
            "no backend available for this platform yet"
        ));
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = ?e, "jumpkey exited with error");
            ExitCode::from(1)
        }
    }
}

fn parse_args() -> Result<Option<Cli>, ExitCode> {
    let mut cli = Cli::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("jumpkey {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--no-tray" => cli.no_tray = true,
            "--close" | "--quit" => cli.close = true,
            "--clear-logs" => cli.clear_logs = true,
            other => {
                eprintln!("jumpkey: unknown argument: {other}");
                eprintln!("Try 'jumpkey --help' for a list of options.");
                return Err(ExitCode::from(2));
            }
        }
    }
    Ok(Some(cli))
}

fn print_help() {
    println!(
        "jumpkey {} — system-wide keyboard navigation overlay",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("USAGE:");
    println!("    jumpkey [FLAGS]");
    println!();
    println!("FLAGS:");
    println!("    -h, --help        Print help and exit");
    println!("    -V, --version     Print version and exit");
    println!("        --no-tray     Run without the system tray icon (hotkeys-only mode)");
    println!("        --close       Cleanly shut down a running jumpkey instance and exit");
    println!("                      (alias: --quit)");
    println!("        --clear-logs  Delete the on-disk log files and exit");
    println!();
    println!("DEFAULT HOTKEYS:");
    println!("    Ctrl+Shift+Space  Pick element in foreground window");
    println!("    Ctrl+Alt+Space    Pick top-level window across all monitors");
    println!("    Esc               Cancel an open overlay");
    println!();
    println!("HINT MODIFIERS (element picker):");
    println!("    <letter>          Invoke (primary action / left-click fallback)");
    println!("    Ctrl+<letter>     Double-click the element");
    println!("    Shift+<letter>    Right-click the element (context menu)");
    println!();
    println!("LOGS (release builds):");
    println!("    Location:         %LOCALAPPDATA%\\jumpkey\\jumpkey.log[.YYYY-MM-DD]");
    println!("    Rotation:         daily, keeping the 7 most recent files");
    println!("    Manual purge:     jumpkey --clear-logs");
    println!();
    println!("ENVIRONMENT:");
    println!("    RUST_LOG          Tracing filter, e.g. `jumpkey=debug` (default: info)");
    println!();
    println!("PROJECT:");
    println!("    Repository:       {}", env!("CARGO_PKG_REPOSITORY"));
}

#[cfg(windows)]
fn run_windows(cli: Cli) -> anyhow::Result<()> {
    use ::windows::Win32::System::Threading::GetCurrentThreadId;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PostQuitMessage, TranslateMessage, MSG,
    };

    // Refuse to start a second copy in the same session — two instances
    // would race for the same global hotkeys and double-stack tray icons.
    let _instance = match InstanceGuard::acquire()? {
        Some(guard) => guard,
        None => {
            eprintln!("jumpkey is already running in this session.");
            eprintln!("Use the existing tray icon, the `--close` flag, or quit it first.");
            // Exit successfully so launchers / autostart shims don't show
            // an error dialog when the user double-clicks twice.
            return Ok(());
        }
    };

    // Show splash screen during initialization. Track when it appeared so
    // we can enforce a minimum display time below — on fast machines the
    // rest of init finishes in <100ms and the splash would otherwise
    // flash by too quickly for the user to register.
    let splash_shown_at = std::time::Instant::now();
    let splash = match SplashScreen::show() {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = ?e, "failed to show splash screen; continuing anyway");
            None
        }
    };

    // Hidden IPC window so a future `jumpkey --close` can find this
    // process and ask it to shut down. Held alive until the message
    // loop exits; dropping it destroys the window.
    let _ipc_window = match ipc::create() {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(error = ?e, "failed to create IPC window; --close will not work");
            None
        }
    };

    // A broken config.toml starts jumpkey on defaults but leaves the file
    // alone (nothing saves it unless the user does), and the user is told
    // just before hotkeys are registered (below).
    let (mut config, startup_config_error) = match Config::try_load() {
        Ok(Some(cfg)) => {
            tracing::info!("loaded config from disk");
            (cfg, None)
        }
        Ok(None) => {
            tracing::info!("no config file found, using defaults");
            (Config::default(), None)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "config file is invalid, using defaults");
            (Config::default(), Some(e))
        }
    };
    // Resolve the alphabet from the preset + flags every launch so users
    // who hand-edit `[hints]` to switch presets don't have to delete the
    // stale `alphabet` field by hand. Settings dialog Save also writes
    // a resolved value, so this is just defence-in-depth.
    let (hint_engine, engine_layout_id) = build_hint_engine(&config.hints, &config.keyboard);
    let element_style = HintStyle::elements_from_config(&config.colors.element);
    let window_style = HintStyle::windows_from_config(&config.colors.window);

    let mut backend = jumpkey::windows::WindowsBackend::with_config(
        config.performance.enable_caching,
        config.performance.cache_ttl_ms,
        config.scope.max_elements,
    )?;

    // Phase 4: spin up the foreground-tracking pre-warm worker so the
    // hotkey path can serve from a hot cache. Held in a binding whose
    // `Drop` unhooks `SetWinEventHook` on shutdown. Failure is
    // non-fatal: the synchronous hotkey path still works, just with
    // cold-cache UIA latency on each press.
    // `telemetry` feature: foreground-change tracking + the `session`
    // record (see src/telemetry/). Dropped (unhooked) on exit.
    #[cfg(feature = "telemetry")]
    let _telemetry = jumpkey::telemetry::start(jumpkey::telemetry::SessionInfo {
        scope: format!("{:?}", config.scope.mode),
        hint_order: format!("{:?}", config.hints.order),
        alphabet_len: hint_engine.alphabet().len(),
        caching: config.performance.enable_caching,
        cache_ttl_ms: config.performance.cache_ttl_ms,
        max_elements: config.scope.max_elements,
    });

    let _prewarm = match jumpkey::windows::prewarm::Prewarmer::start(
        backend.cache_handle(),
        config.scope.max_elements,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = ?e, "prewarm worker unavailable; hotkeys will run cold");
            None
        }
    };

    // Register hotkeys from config. Conflicts (e.g. another app already
    // owns the chord) are surfaced to the user but don't abort startup —
    // partial registration is better than nothing, and the user can fix
    // the broken chord in Settings.
    //
    // A broken config.toml is reported first: it explains why the chords
    // (and everything else) are the defaults, which may also conflict.
    if let Some(e) = startup_config_error {
        notify_config_error(&e, "Using default settings for now");
    }
    let outcome = Hotkeys::register_from_config(&config.hotkeys)?;
    let mut hotkeys = outcome.hotkeys;
    if !outcome.conflicts.is_empty() {
        notify_hotkey_conflicts(&outcome.conflicts);
    }

    // Hot-reload watcher. Best-effort: failure leaves jumpkey fully
    // functional but missing the live-edit-`config.toml` convenience.
    // Held in a binding so its `Drop` only fires when `run_windows`
    // returns. The Settings dialog also triggers reload (without
    // depending on the watcher), so users on locked-down profiles
    // where filesystem watching fails still get hot-reload via Save.
    let main_thread_id = unsafe { GetCurrentThreadId() };
    let _config_watcher = match config_watcher::spawn(main_thread_id) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = ?e, "config hot-reload watcher unavailable");
            None
        }
    };

    // The tray is opt-in via `--no-tray` and best-effort otherwise: if it
    // can't be created (e.g. headless CI, no Explorer shell) we still want
    // the hotkeys to work.
    let mut tray = if cli.no_tray {
        tracing::info!("--no-tray: running without system tray icon");
        None
    } else {
        match Tray::build(&config.hotkeys) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = ?e, "tray icon unavailable; continuing with hotkeys only");
                None
            }
        }
    };

    println!("jumpkey {} is running.", env!("CARGO_PKG_VERSION"));
    println!("  Pick element : {}", config.hotkeys.pick_element);
    println!("  Pick window  : {}", config.hotkeys.pick_window);
    println!("  Open settings: {}", config.hotkeys.open_settings);
    println!("  Scroll mode  : {}", config.hotkeys.scroll_mode);
    println!("  Cancel       : Esc (inside overlay)");
    #[cfg(debug_assertions)]
    {
        if tray.is_some() {
            println!(
                "  Quit         : Tray menu → Quit, `jumpkey --close`, or Ctrl + C in this terminal"
            );
        } else {
            println!("  Quit         : `jumpkey --close`, or Ctrl + C in this terminal");
        }
    }
    #[cfg(not(debug_assertions))]
    {
        if tray.is_some() {
            println!("  Quit         : Tray menu → Quit, or `jumpkey --close`");
        } else {
            println!("  Quit         : `jumpkey --close`");
        }
    }
    println!();
    println!("Switch focus to any app, then press a leader.");

    // Keep the splash visible for at least 2.5s so the user actually sees
    // the brand mark on fast machines where init completes in <100ms.
    // Pump messages while we wait so the splash stays painted.
    let mut reload_pending = false;
    if splash.is_some() {
        const MIN_SPLASH_MS: u128 = 2500;
        let elapsed = splash_shown_at.elapsed().as_millis();
        if elapsed < MIN_SPLASH_MS {
            let remaining = MIN_SPLASH_MS - elapsed;
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(remaining as u64);
            use ::windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW as DM, PeekMessageW as PM, TranslateMessage as TM, MSG as M,
                PM_REMOVE as PMR,
            };
            let mut m = M::default();
            while std::time::Instant::now() < deadline {
                unsafe {
                    while PM(&mut m, None, 0, 0, PMR).as_bool() {
                        // Dispatching a NULL-hwnd thread message drops it, so
                        // remember a config reload and replay it below.
                        if m.hwnd.0.is_null() && m.message == WM_USER_RELOAD_CONFIG {
                            reload_pending = true;
                            continue;
                        }
                        let _ = TM(&m);
                        DM(&m);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }
    drop(splash);

    // The splash loop above dispatched any WM_HOTKEY / tray messages that
    // arrived meanwhile, so their actions are already queued on the crates'
    // channels — but the main loop only polls those after `GetMessageW`
    // returns. Post a no-op thread message so it wakes immediately and
    // drains them, instead of waiting for some unrelated message. A config
    // reload swallowed by the splash loop is replayed first (its branch
    // `continue`s before polling, hence the WM_NULL after it).
    {
        use ::windows::Win32::Foundation::{LPARAM, WPARAM};
        use ::windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_NULL};
        let wake = |message: u32| {
            if let Err(e) =
                unsafe { PostThreadMessageW(main_thread_id, message, WPARAM(0), LPARAM(0)) }
            {
                tracing::debug!(error = ?e, message, "failed to post post-splash wake message");
            }
        };
        if reload_pending {
            wake(WM_USER_RELOAD_CONFIG);
        }
        wake(WM_NULL);
    }

    let mut runtime = Runtime {
        hint_engine,
        engine_layout_id,
        element_style,
        window_style,
        scope_mode: config.scope.mode,
        hint_order: config.hints.order,
    };

    // Scroll mode (#54): at most one session, run from this loop (never a
    // nested loop, so its hotkey toggles instead of queueing). The
    // indicator window is created on first use and kept.
    let mut scroll: Option<ScrollSession> = None;
    let mut scroll_indicator: Option<Indicator> = None;
    let mut scroll_motion = config.scroll;
    // Actions queued by an ending scroll session (a pick hotkey pressed
    // during scroll mode runs once the session is torn down).
    let mut pending: Vec<HotkeyAction> = Vec::new();

    // Win32 message loop. `GetMessageW` is required on this thread for the
    // global-hotkey crate's hidden message window to receive `WM_HOTKEY`,
    // and for tray-icon's notification window to receive shell callbacks.
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            // 0 = WM_QUIT (clean shutdown via PostQuitMessage), -1 = error.
            break;
        }
        // Telemetry: note when a WM_HOTKEY arrived (no-op without the
        // `telemetry` feature).
        jumpkey::telemetry::note_message(msg.message, msg.time);

        // Custom thread message from the config-file watcher: re-read
        // the file from disk and hot-apply. Skip Translate/Dispatch
        // for thread messages — they have a NULL hwnd, so DispatchMessage
        // would route them through DefWindowProc and waste cycles.
        if msg.hwnd.0.is_null() && msg.message == WM_USER_RELOAD_CONFIG {
            tracing::info!("config-file change detected, reloading");
            // A missing file means defaults (the user deleted it on
            // purpose). A file that doesn't parse keeps the current
            // settings: applying defaults would silently reset every
            // live setting on a typo or a half-written file (review #8).
            match Config::try_load() {
                Ok(loaded) => {
                    let new_config = loaded.unwrap_or_default();
                    if new_config != config {
                        apply_config(
                            &new_config,
                            &mut config,
                            &mut runtime,
                            &mut backend,
                            &mut hotkeys,
                            tray.as_mut(),
                            /* announce = */ true,
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "config file is invalid; keeping current settings");
                    notify_config_error(&e, "Keeping your previous settings");
                }
            }
            continue;
        }

        // Scroll mode's hook thread or discovery worker queued something.
        if msg.hwnd.0.is_null() && msg.message == WM_USER_SCROLL {
            if let Some(session) = scroll.as_mut() {
                if let Some((reason, follow)) = session.on_wake(scroll_indicator.as_ref()) {
                    if let Some(s) = scroll.take() {
                        s.end(reason, scroll_indicator.as_ref());
                    }
                    pending.extend(follow.map(|f| match f {
                        FollowUp::Pick(PickKind::Element) => HotkeyAction::PickElement,
                        FollowUp::Pick(PickKind::Window) => HotkeyAction::PickWindow,
                        FollowUp::Settings => HotkeyAction::OpenSettings,
                    }));
                }
            }
        } else {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let mut actions = std::mem::take(&mut pending);
        actions.extend(hotkeys.poll_actions());
        for action in actions {
            // Any hotkey that reaches here while scroll mode is on ends it
            // first. (The hook normally swallows jumpkey's chords and ends
            // the session itself; this covers a chord that slipped through
            // to RegisterHotKey.)
            if let Some(session) = scroll.take() {
                let reason = match action {
                    HotkeyAction::ScrollMode => ExitReason::Hotkey,
                    HotkeyAction::OpenSettings => ExitReason::Settings,
                    _ => ExitReason::Pick,
                };
                session.end(reason, scroll_indicator.as_ref());
                if action == HotkeyAction::ScrollMode {
                    continue;
                }
            } else if action == HotkeyAction::ScrollMode {
                scroll = start_scroll(&config, main_thread_id, &mut scroll_indicator);
                continue;
            }
            match action {
                HotkeyAction::OpenSettings => match open_settings(&config) {
                    Ok(Some(new_config)) => {
                        apply_config(
                            &new_config,
                            &mut config,
                            &mut runtime,
                            &mut backend,
                            &mut hotkeys,
                            tray.as_mut(),
                            /* announce = */ true,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(error = ?e, "settings window failed");
                        notification::error("Couldn't open Settings", &format!("{e}"));
                    }
                },
                _ => {
                    if let Err(e) = dispatch_hotkey(action, &mut backend, &mut runtime, &config) {
                        tracing::error!(?action, error = ?e, "hotkey handler failed");
                        notification::error(
                            "jumpkey: action failed",
                            &format!("{action:?} failed:\n{e}\n\nSee log for details."),
                        );
                    }
                }
            }
        }

        // `[scroll]` hot-reload reaches a running session too.
        if config.scroll != scroll_motion {
            scroll_motion = config.scroll;
            if let Some(s) = &scroll {
                s.set_motion(scroll_motion.motion());
            }
        }

        let tray_cmds: Vec<TrayCommand> =
            tray.as_ref().map(|t| t.poll_commands()).unwrap_or_default();
        for cmd in tray_cmds {
            match cmd {
                TrayCommand::OpenSettings => match open_settings(&config) {
                    Ok(Some(new_config)) => {
                        apply_config(
                            &new_config,
                            &mut config,
                            &mut runtime,
                            &mut backend,
                            &mut hotkeys,
                            tray.as_mut(),
                            /* announce = */ true,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(error = ?e, "settings window failed");
                        notification::error("Couldn't open Settings", &format!("{e}"));
                    }
                },
                TrayCommand::ViewLog => {
                    #[cfg(not(debug_assertions))]
                    {
                        if let Err(e) = open_log_file() {
                            tracing::error!(error = ?e, "failed to open log file");
                            notification::error("jumpkey: couldn't open log", &format!("{e}"));
                        }
                    }
                    #[cfg(debug_assertions)]
                    {
                        notification::info(
                            "View Log unavailable",
                            "Debug builds write logs to stderr, not to a file. \
                            Run the release build to use this option.",
                        );
                    }
                }
                TrayCommand::Quit => {
                    tracing::info!("quit requested from tray");
                    unsafe { PostQuitMessage(0) };
                }
            }
        }
    }

    Ok(())
}

/// Per-process configuration that the picker handlers need. Built once at
/// startup from [`Config`] so repeated invocations don't re-parse strings.
#[cfg(windows)]
struct Runtime {
    hint_engine: HintEngine,
    /// Windows layout the engine's alphabet was resolved against (see
    /// [`refresh_engine_for_layout`]).
    engine_layout_id: isize,
    element_style: HintStyle,
    window_style: HintStyle,
    /// Which set of windows the element picker enumerates. Driven by
    /// `[scope]` in `config.toml`; defaults to active-window for
    /// backwards-compatible v0.3.0 behaviour.
    scope_mode: ScopeMode,
    /// Which point the shortest element labels radiate out from.
    /// Driven by `hints.order` in `config.toml`.
    hint_order: HintOrder,
}

/// Hot-apply a freshly loaded [`Config`] to every live subsystem
/// without restarting the process. Called from both the Settings
/// dialog Save handler and the [`config_watcher`]'s file-change
/// thread message; both code paths converge here so the apply logic
/// stays in one place.
///
/// Steps, in dependency order:
///   1. Resolve the alphabet from the preset+modifiers and rebuild
///      the [`HintEngine`].
///   2. Rebuild the element / window [`HintStyle`]s (colors, opacity,
///      leader pref).
///   3. Push the new scope mode into [`Runtime`].
///   4. Push cache + max-elements into the [`WindowsBackend`] without
///      reconstructing it (the UIA client is expensive to recreate).
///   5. Tear down the old [`Hotkeys`] *before* registering the new
///      ones — the OS hotkey table is per-process so re-registering
///      the same chord on top of itself would otherwise fail.
///   6. Update the registry "launch at startup" entry to match.
///   7. Replace the cached `Config` so subsequent Settings opens see
///      the current state, not the stale one we loaded at startup.
///
/// `announce` controls whether a "Settings applied" toast fires;
/// pass `false` for silent reloads (e.g. on first paint).
#[cfg(windows)]
fn apply_config(
    new_config: &Config,
    cached: &mut Config,
    runtime: &mut Runtime,
    backend: &mut jumpkey::windows::WindowsBackend,
    hotkeys: &mut Hotkeys,
    tray: Option<&mut Tray>,
    announce: bool,
) {
    let (engine, layout_id) = build_hint_engine(&new_config.hints, &new_config.keyboard);
    runtime.hint_engine = engine;
    runtime.engine_layout_id = layout_id;
    runtime.element_style = HintStyle::elements_from_config(&new_config.colors.element);
    runtime.window_style = HintStyle::windows_from_config(&new_config.colors.window);
    runtime.scope_mode = new_config.scope.mode;
    runtime.hint_order = new_config.hints.order;

    backend.reconfigure_cache(
        new_config.performance.enable_caching,
        new_config.performance.cache_ttl_ms,
    );
    backend.set_max_elements_global(new_config.scope.max_elements);

    // Drop the old Hotkeys *before* trying to register the new ones.
    // Windows' RegisterHotKey is per-process, so replacing
    // Ctrl+Shift+Space (old) with Ctrl+Shift+Space (new) would fail
    // on the `register` call if the old chord was still alive at the
    // moment of attempt. The empty-Hotkeys placeholder either built
    // here or already in `*hotkeys` keeps the type intact between
    // the drop and the new assignment.
    if let Ok(empty) = Hotkeys::new() {
        let _drop_old = std::mem::replace(hotkeys, empty);
        // _drop_old goes out of scope at the end of this `if let`,
        // releasing every chord it was holding.
    }
    match Hotkeys::register_from_config(&new_config.hotkeys) {
        Ok(outcome) => {
            *hotkeys = outcome.hotkeys;
            if !outcome.conflicts.is_empty() {
                notify_hotkey_conflicts(&outcome.conflicts);
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "re-registering hotkeys after reload failed");
            notification::error(
                "jumpkey: hotkey reload failed",
                &format!(
                    "{e}\n\nThe previous chords are no longer active. Open Settings to retry."
                ),
            );
        }
    }

    // Best-effort: a registry write failure here is non-fatal;
    // everything else has already taken effect in-process.
    if let Err(e) = startup::set_enabled(new_config.startup.launch_at_startup) {
        tracing::warn!(error = ?e, "couldn't sync launch-at-startup registry entry");
    }

    if let Some(tray) = tray {
        tray.update_labels(&new_config.hotkeys);
    }

    *cached = new_config.clone();

    if announce {
        notification::info(
            "Settings applied",
            "Your changes are live now — no restart needed.",
        );
    }
}

/// Common path for "we just registered hotkeys and one or more
/// failed". Surfacing through a notification rather than the log
/// matters because the failure is otherwise silent — the user just
/// sees their chord doing nothing.
#[cfg(windows)]
fn notify_hotkey_conflicts(conflicts: &[HotkeyConflict]) {
    let body = conflicts
        .iter()
        .map(|c| format!("• {} ({}): {}", c.action, c.chord, c.reason))
        .collect::<Vec<_>>()
        .join("\n");
    notification::warn(
        "jumpkey: hotkey conflict",
        &format!(
            "Some hotkeys couldn't be registered:\n\n{body}\n\n\
            Open Settings to choose a different chord."
        ),
    );
}

/// Tell the user `config.toml` couldn't be read. `outcome` says what
/// jumpkey does instead ("Keeping your previous settings", …). The
/// error text comes from the TOML parser and names the line and column.
#[cfg(windows)]
fn notify_config_error(error: &anyhow::Error, outcome: &str) {
    notification::warn(
        "jumpkey: config.toml has an error",
        &format!("{error:#}\n\n{outcome}. Fix the file and save it again to apply it."),
    );
}

/// Open the Settings dialog, capturing the user's keyboard layout first
/// while their app is still in front (so the dialog's "Auto" entry and
/// the alphabet it saves reflect the layout they're actually using).
#[cfg(windows)]
fn open_settings(config: &Config) -> anyhow::Result<Option<Config>> {
    jumpkey::windows::keyboard_layout::capture_active_layout();
    settings_window::show(config)
}

/// Resolve the keyboard layout, build the alphabet through it, and
/// construct the [`HintEngine`]. Returns the engine plus the id of the
/// Windows layout it was built against.
#[cfg(windows)]
///
/// Uses the most recently captured layout rather than re-reading the
/// foreground window: this also runs after Settings → Save, when jumpkey's
/// own window is in front and Windows may report a stale layout. The
/// hotkey path captures fresh first (see [`refresh_engine_for_layout`]).
fn build_hint_engine(
    hints: &jumpkey::config::HintConfig,
    keyboard: &jumpkey::config::KeyboardConfig,
) -> (HintEngine, isize) {
    let layout_id = jumpkey::windows::keyboard_layout::last_layout_id();
    let detected = match keyboard.layout {
        jumpkey::LayoutChoice::Auto => jumpkey::windows::keyboard_layout::detect(),
        _ => None,
    };
    let layout = jumpkey::Layout::resolve(keyboard.layout, detected);
    let alphabet = jumpkey::alphabet_presets::build_alphabet_for(hints, &layout, keyboard.geometry);
    tracing::debug!(%alphabet, "hint alphabet resolved");
    // Finger per alphabet character, via the key the user actually presses.
    let fingers = alphabet
        .chars()
        .map(|c| {
            layout
                .typing_key(c)
                .map_or(&[][..], |k| keyboard.geometry.fingers(k))
        })
        .collect();
    let engine = HintEngine::with_strategy(&alphabet, hints.strategy)
        .with_min_singles(hints.min_singles)
        .with_key_rule(hints.consecutive_keys, fingers);
    (engine, layout_id)
}

/// Capture the current Windows layout (the overlay translates keystrokes
/// through it) and, in `auto` layout mode, rebuild the engine if the user
/// switched layouts since it was built, so the labels stay typeable.
#[cfg(windows)]
fn refresh_engine_for_layout(runtime: &mut Runtime, config: &Config) {
    let span = tel_span!("refresh_layout", rebuilt = false).entered();
    let current = jumpkey::windows::keyboard_layout::capture_active_layout();
    if config.keyboard.layout != jumpkey::LayoutChoice::Auto || current == runtime.engine_layout_id
    {
        return;
    }
    tel_record!(span, "rebuilt", true);
    let (engine, id) = build_hint_engine(&config.hints, &config.keyboard);
    tracing::info!("keyboard layout changed; rebuilt hint alphabet");
    runtime.hint_engine = engine;
    runtime.engine_layout_id = id;
}

#[cfg(windows)]
fn dispatch_hotkey(
    action: HotkeyAction,
    backend: &mut jumpkey::windows::WindowsBackend,
    runtime: &mut Runtime,
    config: &Config,
) -> anyhow::Result<()> {
    let kind = match action {
        HotkeyAction::PickElement => "element",
        HotkeyAction::PickWindow => "window",
        // Handled separately in the main loop.
        HotkeyAction::OpenSettings | HotkeyAction::ScrollMode => return Ok(()),
    };
    // Root of this press's telemetry trace (no-op without the feature);
    // written when it drops at the end of this function.
    let pick = Pick::begin(kind, &runtime.scope_mode);
    refresh_engine_for_layout(runtime, config);
    match action {
        HotkeyAction::PickElement => handle_pick_element(backend, runtime, &pick),
        HotkeyAction::PickWindow => handle_pick_window(runtime, &pick),
        HotkeyAction::OpenSettings | HotkeyAction::ScrollMode => Ok(()),
    }
}

/// Thread message from scroll mode's hook thread and discovery worker:
/// effects or areas are queued (`WM_USER + 1` is the config reload).
#[cfg(windows)]
const WM_USER_SCROLL: u32 = ::windows::Win32::UI::WindowsAndMessaging::WM_USER + 2;

/// Start a scroll mode session on the foreground window, or explain why
/// not.
#[cfg(windows)]
fn start_scroll(
    config: &Config,
    main_thread_id: u32,
    indicator: &mut Option<Indicator>,
) -> Option<ScrollSession> {
    if indicator.is_none() {
        match Indicator::new() {
            Ok(i) => *indicator = Some(i),
            Err(e) => tracing::warn!(error = ?e, "scroll indicator unavailable"),
        }
    }
    let chord = |s: &str| jumpkey::windows::hotkey::chord_from_str(s).ok();
    let chords = Chords {
        scroll: chord(&config.hotkeys.scroll_mode),
        pick_element: chord(&config.hotkeys.pick_element),
        pick_window: chord(&config.hotkeys.pick_window),
        open_settings: chord(&config.hotkeys.open_settings),
    };
    match ScrollSession::start(
        chords,
        config.scroll.motion(),
        main_thread_id,
        WM_USER_SCROLL,
    ) {
        Ok(s) => Some(s),
        Err(StartError::NoTarget) => None,
        Err(StartError::Elevated) => {
            notification::info(
                "Scroll mode",
                "Scroll mode doesn't work in windows running as administrator.",
            );
            None
        }
        Err(StartError::Failed(e)) => {
            tracing::error!(error = ?e, "scroll mode failed to start");
            notification::error("jumpkey: scroll mode failed", &format!("{e}"));
            None
        }
    }
}

#[cfg(windows)]
fn handle_pick_element(
    backend: &mut jumpkey::windows::WindowsBackend,
    runtime: &Runtime,
    pick: &Pick,
) -> anyhow::Result<()> {
    // Snapshot focus / cursor / active monitor once so enumeration and
    // label ordering agree even if focus moves during the UIA walk.
    let ctx = {
        let _s = tel_span!("capture_ctx").entered();
        jumpkey::windows::PickContext::capture()
    };
    tel_event!("enum_start");
    let mut elements: Vec<Element> = backend.enumerate_by_scope(runtime.scope_mode, &ctx)?;
    tel_event!("elements_ready");
    pick.count(elements.len());
    if elements.is_empty() {
        pick.outcome("empty");
        tracing::info!("no interactable elements in foreground window");
        return Ok(());
    }

    // Reorder so the shortest labels land nearest the configured anchor
    // (window center / cursor). Sorting `elements` itself keeps
    // `elements[result.index]` below pointing at the right element.
    {
        let _s = tel_span!("sort", n = elements.len()).entered();
        if let Some(anchor) =
            jumpkey::windows::hint_anchor(runtime.hint_order, runtime.scope_mode, &ctx, &elements)
        {
            jumpkey::layout_order::sort_by_proximity(&mut elements, anchor);
        }
    }

    let hints: Vec<Hint> = {
        let _s = tel_span!("labels", n = elements.len()).entered();
        let labels = runtime.hint_engine.generate(elements.len());
        elements
            .iter()
            .zip(labels.iter())
            .map(|(el, label)| Hint {
                bounds: el.bounds,
                label: label.clone(),
                extra: None,
            })
            .collect()
    };
    tel_event!("labels_ready");
    tracing::info!(count = hints.len(), "showing element overlay");

    match pick_hint(hints, runtime.element_style)? {
        Some(result) => {
            pick.outcome("picked");
            let chosen = &elements[result.index];
            // Modifier held at commit time picks the action variant.
            // Ctrl + letter → double-click, Shift + letter → right-click
            // (context menu), no modifier → the standard UIA invoke path.
            // See `PickModifier` for the precedence rule when both are
            // held.
            let action = match result.modifier {
                PickModifier::Ctrl => Action::DoubleClick,
                PickModifier::Shift => Action::RightClick,
                PickModifier::None => Action::Invoke,
            };
            pick.modifier(match result.modifier {
                PickModifier::Ctrl => "ctrl",
                PickModifier::Shift => "shift",
                PickModifier::None => "none",
            });
            tracing::info!(
                id = ?chosen.id,
                role = ?chosen.role,
                modifier = ?result.modifier,
                ?action,
                "element selected — performing"
            );
            let performed = {
                let span = tel_span!(
                    "perform",
                    action = ?action,
                    invoke_path = tracing::field::Empty,
                    ok = tracing::field::Empty
                )
                .entered();
                let performed = backend.perform(chosen.id, action);
                tel_record!(span, "ok", performed.is_ok());
                performed
            };
            tel_event!("action_done");
            if let Err(e) = performed {
                tracing::warn!(error = ?e, "perform failed");
            }
        }
        None => {
            pick.outcome("cancelled");
            tracing::info!("element overlay cancelled");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn handle_pick_window(runtime: &Runtime, pick: &Pick) -> anyhow::Result<()> {
    let windows = window_picker::enumerate_visible()?;
    pick.count(windows.len());
    if windows.is_empty() {
        pick.outcome("empty");
        tracing::info!("no visible top-level windows");
        notification::info("jumpkey", "No visible windows to pick from.");
        return Ok(());
    }
    tracing::info!(count = windows.len(), "showing window overlay");

    match window_picker::pick_with(windows, &runtime.hint_engine, runtime.window_style)? {
        Some(chosen) => {
            pick.outcome("picked");
            tracing::info!(title = %chosen.title, "window selected — focusing");
            window_picker::focus(chosen.hwnd)?;
            tel_event!("action_done");
        }
        None => {
            pick.outcome("cancelled");
            tracing::info!("window overlay cancelled");
        }
    }
    Ok(())
}

/// Keeps the telemetry writer alive until exit (feature builds only);
/// dropping it writes any open trace and flushes the file.
struct TracingGuard {
    #[cfg(all(feature = "telemetry", windows))]
    _telemetry: Option<jumpkey::telemetry::TelemetryGuard>,
}

/// Set up logging: the normal log (stderr in debug builds, daily files
/// in `%LOCALAPPDATA%\jumpkey\` in release builds) and, with the
/// `telemetry` feature, the telemetry layer. Each has its own filter,
/// so `RUST_LOG` only affects the log and telemetry spans never show up
/// in it.
fn init_tracing() -> TracingGuard {
    use tracing_subscriber::{
        fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    #[allow(unused_mut)]
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Telemetry spans and events go to their own layer, never the log.
    #[cfg(all(feature = "telemetry", windows))]
    if let Ok(off) = format!("{}=off", jumpkey::telemetry::TARGET).parse() {
        filter = filter.add_directive(off);
    }
    let log = fmt::layer()
        .with_target(false)
        // Colours only on the debug console, never in log files.
        .with_ansi(cfg!(debug_assertions))
        .with_writer(log_writer())
        .with_filter(filter);
    let registry = tracing_subscriber::registry().with(log);

    #[cfg(all(feature = "telemetry", windows))]
    {
        let (layer, guard) = match jumpkey::telemetry::layer() {
            Some((layer, guard)) => (Some(layer), Some(guard)),
            None => (None, None),
        };
        registry.with(layer).init();
        TracingGuard { _telemetry: guard }
    }
    #[cfg(not(all(feature = "telemetry", windows)))]
    {
        registry.init();
        TracingGuard {}
    }
}

/// Where the normal log goes: stderr in debug builds; in release builds
/// daily-rotated files with a 7-file ceiling (the old `rolling::never`
/// appended forever and produced multi-hundred-megabyte files on
/// long-running installs), or stderr if `%LOCALAPPDATA%` isn't writable.
fn log_writer() -> tracing_subscriber::fmt::writer::BoxMakeWriter {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    #[cfg(not(debug_assertions))]
    {
        use tracing_appender::rolling::{RollingFileAppender, Rotation};
        if let Some(log_dir) = get_log_dir() {
            if std::fs::create_dir_all(&log_dir).is_ok() {
                let appender = RollingFileAppender::builder()
                    .rotation(Rotation::DAILY)
                    .filename_prefix("jumpkey")
                    .filename_suffix("log")
                    .max_log_files(7)
                    .build(&log_dir);
                if let Ok(file_appender) = appender {
                    return BoxMakeWriter::new(file_appender);
                }
            }
        }
    }
    BoxMakeWriter::new(std::io::stderr)
}

/// Directory holding all jumpkey log files. Returns `None` when
/// `%LOCALAPPDATA%` is unset (very rare — typically only on heavily
/// restricted service accounts).
#[cfg(not(debug_assertions))]
fn get_log_dir() -> Option<std::path::PathBuf> {
    use std::env;
    let appdata = env::var("LOCALAPPDATA").ok()?;
    let mut path = std::path::PathBuf::from(appdata);
    path.push("jumpkey");
    Some(path)
}

/// Path to the *most recent* log file. With daily rotation this is
/// today's file, named `jumpkey.YYYY-MM-DD.log`. We pick "newest by
/// mtime" rather than computing today's name explicitly so the lookup
/// keeps working across timezone changes / clock skew / files written
/// by an earlier process.
#[cfg(not(debug_assertions))]
fn get_latest_log_file() -> Option<std::path::PathBuf> {
    use std::fs;
    let dir = get_log_dir()?;
    let entries = fs::read_dir(&dir).ok()?;
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("jumpkey"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let modified = meta.modified().ok()?;
            Some((e.path(), modified))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(path, _)| path)
}

#[cfg(not(debug_assertions))]
fn open_log_file() -> anyhow::Result<()> {
    if let Some(log_path) = get_latest_log_file() {
        std::process::Command::new("notepad.exe")
            .arg(&log_path)
            .spawn()?;
    }
    Ok(())
}

/// Implementation of `jumpkey --close`. Looks for the hidden IPC window
/// the running instance creates ([`ipc::create`]) and posts `WM_CLOSE`
/// to it. The running instance's message loop turns that into a
/// graceful shutdown — the same path the tray-menu "Quit" entry takes.
///
/// Always exits the closer process with code 0 even when no instance
/// was found, so wrapping `jumpkey --close` in startup / cleanup
/// scripts doesn't error out when jumpkey wasn't running.
#[cfg(windows)]
fn run_close_command() -> ExitCode {
    match ipc::send_close_signal() {
        Ok(true) => {
            println!("jumpkey: shutdown signal sent.");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("jumpkey: no running instance found.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("jumpkey: failed to signal running instance: {e}");
            ExitCode::from(1)
        }
    }
}

/// Implementation of `jumpkey --clear-logs`. Removes every file under
/// the log directory whose name starts with `jumpkey` (today's file plus
/// any rolled-over daily archives). Leaves the directory itself in
/// place so the next run can write straight back to it without an
/// extra `create_dir_all`.
///
/// In debug builds logs go to stderr only — there's nothing to delete,
/// so the command is a no-op that prints a hint instead of pretending
/// it did something.
#[cfg(windows)]
fn run_clear_logs_command() -> ExitCode {
    #[cfg(debug_assertions)]
    {
        eprintln!("jumpkey: --clear-logs has nothing to do in debug builds (logs go to stderr).");
        ExitCode::SUCCESS
    }
    #[cfg(not(debug_assertions))]
    {
        use std::fs;
        let Some(dir) = get_log_dir() else {
            eprintln!("jumpkey: %LOCALAPPDATA% is not set; nothing to clear.");
            return ExitCode::from(1);
        };
        if !dir.exists() {
            println!("jumpkey: log directory does not exist; nothing to clear.");
            return ExitCode::SUCCESS;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("jumpkey: cannot read log directory {}: {e}", dir.display());
                return ExitCode::from(1);
            }
        };
        let mut removed = 0usize;
        let mut errors = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("jumpkey") {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => {
                    // Common case: the running jumpkey has the current
                    // file open with FILE_SHARE_READ but not _DELETE.
                    // Worth surfacing so the user knows to run --close
                    // first or shut down via the tray.
                    eprintln!("jumpkey: could not delete {}: {e}", entry.path().display());
                    errors += 1;
                }
            }
        }
        println!(
            "jumpkey: removed {removed} log file(s) from {}.",
            dir.display()
        );
        if errors > 0 {
            eprintln!(
                "jumpkey: {errors} file(s) could not be deleted (is jumpkey still running? \
                Run `jumpkey --close` first)."
            );
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    }
}
