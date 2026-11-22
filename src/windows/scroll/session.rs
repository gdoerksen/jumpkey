//! One scroll mode session, driven from jumpkey's main loop (not a nested
//! loop, so the hotkey toggles the mode instead of queueing, #47).
//!
//! [`ScrollSession::start`] installs the hook thread, starts the engine
//! and a discovery worker. Both post `wake_message` to the main thread;
//! the main loop then calls [`ScrollSession::on_wake`], which runs the
//! queued effects and says when the session is over. [`ScrollSession::end`]
//! tears everything down, puts the pointer back and writes telemetry.

use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

use anyhow::Result;
use uiautomation::UIAutomation;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, PostThreadMessageW, SetCursorPos,
};

use crate::model::Bounds;
use crate::scroll::{
    arrange_areas, Area, Chords, Effect, ExitReason, FirstFrom, MotionConfig, PickKind,
};
use crate::windows::scroll::areas::{
    all_areas, client_bounds, cursor_over, first_area, window_bounds,
};
use crate::windows::scroll::engine::Engine;
use crate::windows::scroll::hook::{HookConfig, ScrollHook};
use crate::windows::scroll::indicator::Indicator;

#[cfg(feature = "telemetry")]
type TelSpan = tracing::Span;
#[cfg(not(feature = "telemetry"))]
type TelSpan = crate::telemetry::NoSpan;

/// What to do once a session has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// Run a picker.
    Pick(PickKind),
    /// Open Settings.
    Settings,
}

/// Why a session didn't start.
#[derive(Debug)]
pub enum StartError {
    /// No foreground window, or it's one of jumpkey's own.
    NoTarget,
    /// The foreground window runs elevated and jumpkey doesn't: the hook
    /// wouldn't see keys typed into it.
    Elevated,
    /// Something failed (hook, engine, thread).
    Failed(anyhow::Error),
}

enum Discovery {
    First(Area, FirstFrom),
    All(Vec<Area>, u64),
}

/// A running scroll mode session.
pub struct ScrollSession {
    hook: ScrollHook,
    engine: Engine,
    discovered: Receiver<Discovery>,
    target: HWND,
    frame: Bounds,
    client: Bounds,
    areas: Vec<Area>,
    active: usize,
    first: Option<Area>,
    saved_cursor: POINT,
    started: Instant,
    keys: u32,
    span: TelSpan,
}

fn process_elevated(process: HANDLE) -> Option<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut info = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let r = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut info as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        r.ok()?;
        Some(info.TokenIsElevated != 0)
    }
}

/// True when `hwnd`'s process is elevated and jumpkey's isn't. A process
/// jumpkey can't even query is treated as elevated (that's the usual
/// reason it can't).
fn target_is_elevated(hwnd: HWND) -> bool {
    unsafe {
        if process_elevated(GetCurrentProcess()) == Some(true) {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return false;
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return true;
        };
        let elevated = process_elevated(h).unwrap_or(true);
        let _ = CloseHandle(h);
        elevated
    }
}

fn is_own_window(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid == std::process::id()
}

impl ScrollSession {
    /// Start a session on the foreground window.
    pub fn start(
        chords: Chords,
        motion: MotionConfig,
        main_thread_id: u32,
        wake_message: u32,
    ) -> Result<Self, StartError> {
        let target = unsafe { GetForegroundWindow() };
        if target.0.is_null() || is_own_window(target) {
            return Err(StartError::NoTarget);
        }
        if target_is_elevated(target) {
            return Err(StartError::Elevated);
        }
        let mut saved_cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut saved_cursor);
        }
        let client = client_bounds(target);
        let frame = window_bounds(target);
        // Wheel input goes to the window under the pointer: make sure
        // that's the target before anything scrolls.
        let mut cursor = saved_cursor;
        if !cursor_over(target, cursor) {
            let (cx, cy) = client.center();
            unsafe {
                let _ = SetCursorPos(cx, cy);
            }
            cursor = POINT { x: cx, y: cy };
        }

        let hook = ScrollHook::start(HookConfig {
            chords,
            main_thread_id,
            wake_message,
            foreground: target,
        })
        .map_err(StartError::Failed)?;
        let engine = Engine::start(motion, hook.real_mods()).map_err(StartError::Failed)?;

        let (tx, discovered) = mpsc::channel();
        let raw = target.0 as isize;
        std::thread::Builder::new()
            .name("jumpkey-scroll-discovery".into())
            .spawn(move || {
                let wake = || unsafe {
                    let _ = PostThreadMessageW(main_thread_id, wake_message, WPARAM(0), LPARAM(0));
                };
                let Ok(uia) = UIAutomation::new() else {
                    return;
                };
                let hwnd = HWND(raw as *mut core::ffi::c_void);
                let (area, from) = first_area(&uia, hwnd, cursor);
                if tx.send(Discovery::First(area, from)).is_err() {
                    return;
                }
                wake();
                let t = Instant::now();
                let found = all_areas(&uia, hwnd);
                let ms = t.elapsed().as_millis() as u64;
                if tx.send(Discovery::All(found, ms)).is_ok() {
                    wake();
                }
            })
            .map_err(|e| StartError::Failed(e.into()))?;

        let span = crate::tel_span!(
            "scroll",
            areas = tracing::field::Empty,
            discover_ms = tracing::field::Empty,
            first_from = tracing::field::Empty,
            keys = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            exit_by = tracing::field::Empty
        );
        tracing::debug!("scroll mode on");
        Ok(Self {
            hook,
            engine,
            discovered,
            target,
            frame,
            client,
            areas: Vec::new(),
            active: 0,
            first: None,
            saved_cursor,
            started: Instant::now(),
            keys: 0,
            span,
        })
    }

    /// The window being scrolled.
    pub fn target(&self) -> HWND {
        self.target
    }

    /// New tuning from a config reload.
    pub fn set_motion(&self, cfg: MotionConfig) {
        self.engine.config(cfg);
    }

    /// End the session from the main thread (e.g. the hotkey arrived
    /// through `RegisterHotKey`). The exit comes back through
    /// [`Self::on_wake`].
    pub fn request_exit(&self, reason: ExitReason) {
        self.hook.exit(reason);
    }

    fn active_area(&self) -> Area {
        self.areas.get(self.active).copied().unwrap_or(Area {
            bounds: self.client,
            can_h: false,
            can_v: true,
        })
    }

    fn redraw(&self, indicator: Option<&Indicator>) {
        if let Some(ind) = indicator {
            ind.show(self.target, self.frame, &self.areas, self.active);
        }
    }

    /// Run whatever the hook and discovery queued. Returns the exit reason
    /// (and what to do next) once the session should end.
    pub fn on_wake(
        &mut self,
        indicator: Option<&Indicator>,
    ) -> Option<(ExitReason, Option<FollowUp>)> {
        while let Ok(d) = self.discovered.try_recv() {
            match d {
                Discovery::First(area, from) => {
                    crate::tel_record!(self.span, "first_from", from.as_str());
                    self.first = Some(area);
                    self.areas = vec![area];
                    self.active = 0;
                    self.hook.set_areas(self.areas.clone(), 0);
                    if from != FirstFrom::Point {
                        let (x, y) = area.bounds.center();
                        self.engine.move_to(x, y);
                    }
                    self.redraw(indicator);
                }
                Discovery::All(found, ms) => {
                    crate::tel_record!(self.span, "discover_ms", ms);
                    let (areas, active) = arrange_areas(found, self.client, self.first);
                    crate::tel_record!(self.span, "areas", areas.len() as u64);
                    tracing::debug!(areas = areas.len(), discover_ms = ms, "scroll areas found");
                    self.areas = areas;
                    self.active = active;
                    self.hook.set_areas(self.areas.clone(), active);
                    self.redraw(indicator);
                }
            }
        }

        let mut exit = None;
        let mut follow = None;
        for fx in self.hook.drain() {
            tracing::trace!(?fx, "scroll effect");
            match fx {
                Effect::KeyMotion { dir, down, fast } => {
                    if down {
                        self.keys += 1;
                    }
                    self.engine.key(dir, down, fast);
                }
                Effect::Fast(f) => self.engine.fast(f),
                Effect::Jump(j) => {
                    self.keys += 1;
                    self.engine.jump(j, self.active_area());
                }
                Effect::SelectArea(i) => {
                    self.keys += 1;
                    if i < self.areas.len() {
                        self.active = i;
                        let (x, y) = self.areas[i].bounds.center();
                        self.engine.move_to(x, y);
                        self.redraw(indicator);
                    }
                }
                Effect::Exit(r) => exit = exit.or(Some(r)),
                Effect::RunPick(kind) => follow = Some(FollowUp::Pick(kind)),
                Effect::OpenSettings => follow = Some(FollowUp::Settings),
                Effect::MaskAlt => {} // handled on the hook thread
            }
        }
        exit.map(|r| (r, follow))
    }

    /// Tear the session down: stop scrolling, remove the hooks, hide the
    /// indicator, put the pointer back (unless the user moved it) and
    /// record telemetry.
    pub fn end(self, reason: ExitReason, indicator: Option<&Indicator>) {
        self.engine.stop();
        let moved = self.hook.user_moved_pointer();
        let Self {
            hook,
            engine,
            saved_cursor,
            started,
            keys,
            span,
            target,
            ..
        } = self;
        // Join the engine first so no wheel event lands after the pointer
        // moves back.
        drop(engine);
        drop(hook);
        if let Some(ind) = indicator {
            ind.hide();
        }
        if !moved {
            unsafe {
                let _ = SetCursorPos(saved_cursor.x, saved_cursor.y);
            }
        }
        // The window's content moved: its cached elements are stale.
        crate::windows::prewarm::invalidate(target);
        crate::tel_record!(span, "keys", keys as u64);
        crate::tel_record!(span, "duration_ms", started.elapsed().as_millis() as u64);
        crate::tel_record!(span, "exit_by", reason.as_str());
        drop(span);
        tracing::debug!(
            exit_by = reason.as_str(),
            keys,
            duration_ms = started.elapsed().as_millis() as u64,
            "scroll mode off"
        );
    }
}
