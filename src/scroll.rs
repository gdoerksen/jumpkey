//! Scroll mode's platform-free core (#54): what each key does, and how a
//! held key turns into smooth motion.
//!
//! Two pure pieces, both driven by the Windows shell
//! (`crate::windows::scroll`):
//!
//! - [`ScrollMode`] decides, for every key event the keyboard hook sees,
//!   whether the key is swallowed or passed to the app ([`Verdict`]) and
//!   what should happen ([`Effect`]). It runs inside the hook callback, so
//!   it never blocks and never calls the OS.
//! - [`Motion`] turns key presses and releases into "lines to scroll this
//!   tick": a tap scrolls exactly one step; a held key glides after a
//!   short delay, ramping up to full speed, and eases out on release.
//!   Time comes from the caller, so the OS auto-repeat rate never matters.
//!
//! Owner decisions (2026-11-22) encoded here:
//! - Unused plain keys (Shift counts as plain) are swallowed; three in a
//!   row end the mode.
//! - Chords with Ctrl, Alt or Meta pass through to the app, except
//!   jumpkey's own hotkeys and Ctrl+N / Ctrl+P.
//! - A lone Alt tap passes through but is masked so the app doesn't
//!   switch into menu-bar mode.

use std::collections::HashSet;

use crate::input::{Chord, Key, KeyEvent, Mods, NamedKey};
use crate::keyboard::{PhysKey, Row};
use crate::model::Bounds;

/// One scrollable region of the target window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    /// Screen rectangle, physical pixels.
    pub bounds: Bounds,
    /// Can scroll horizontally.
    pub can_h: bool,
    /// Can scroll vertically.
    pub can_v: bool,
}

/// A scroll direction (`h j k l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Dir {
    Left,
    Down,
    Up,
    Right,
}

impl Dir {
    const ALL: [Dir; 4] = [Dir::Left, Dir::Down, Dir::Up, Dir::Right];

    fn index(self) -> usize {
        self as usize
    }

    /// Unit vector: `+x` = scroll right, `+y` = scroll down (content moves
    /// up, like Page Down).
    fn unit(self) -> (f64, f64) {
        match self {
            Dir::Left => (-1.0, 0.0),
            Dir::Down => (0.0, 1.0),
            Dir::Up => (0.0, -1.0),
            Dir::Right => (1.0, 0.0),
        }
    }
}

/// A jump within the active area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Jump {
    Top,
    Bottom,
    HalfUp,
    HalfDown,
}

/// Which picker a pick hotkey pressed during scroll mode asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum PickKind {
    Element,
    Window,
}

/// Why scroll mode ended (telemetry `exit_by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Esc.
    Esc,
    /// The scroll hotkey again.
    Hotkey,
    /// A mouse button press.
    Click,
    /// Three unused keys in a row.
    UnusedKeys,
    /// Another window came to the foreground.
    Foreground,
    /// A pick hotkey: the pick runs next.
    Pick,
    /// The Settings hotkey: Settings opens next.
    Settings,
    /// Session locked or the desktop switched.
    Lock,
}

impl ExitReason {
    /// Whitelist-friendly telemetry string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Esc => "esc",
            Self::Hotkey => "hotkey",
            Self::Click => "click",
            Self::UnusedKeys => "unused_keys",
            Self::Foreground => "foreground",
            Self::Pick => "pick",
            Self::Settings => "settings",
            Self::Lock => "lock",
        }
    }
}

/// What the hook does with the event it is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Let it reach the app.
    Pass,
    /// Eat it.
    Swallow,
}

/// Something the shell should do in response to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// A direction key went down or up. `fast` = Shift held at the press.
    KeyMotion {
        /// Which direction.
        dir: Dir,
        /// Press or release.
        down: bool,
        /// Shift held.
        fast: bool,
    },
    /// Shift went down or up while scroll mode is on.
    Fast(bool),
    /// Jump within the active area.
    Jump(Jump),
    /// Make area `i` the active one.
    SelectArea(usize),
    /// End scroll mode.
    Exit(ExitReason),
    /// Run a picker (after an `Exit(Pick)`).
    RunPick(PickKind),
    /// Open Settings (after an `Exit(Settings)`).
    OpenSettings,
    /// A lone Alt tap is being released: inject a no-op key first so the
    /// app sees Alt+key, not an Alt tap (which opens the menu bar).
    MaskAlt,
}

/// jumpkey's own hotkeys, matched inside the hook while scroll mode runs
/// (passing them to `RegisterHotKey` would close menus, spike #34).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Chords {
    /// The scroll mode hotkey (toggles the mode off).
    pub scroll: Option<Chord>,
    /// Element picker.
    pub pick_element: Option<Chord>,
    /// Window picker.
    pub pick_window: Option<Chord>,
    /// Settings. Matched so Settings doesn't open while the session is
    /// still swallowing keys.
    pub open_settings: Option<Chord>,
}

/// Unused keys in a row that end the mode.
pub const UNUSED_KEYS_TO_EXIT: u8 = 3;

const fn phys(row: Row, col: i8) -> Key {
    Key::Phys(PhysKey::new(row, col))
}

const KEY_H: Key = phys(Row::Home, 5);
const KEY_J: Key = phys(Row::Home, 6);
const KEY_K: Key = phys(Row::Home, 7);
const KEY_L: Key = phys(Row::Home, 8);
const KEY_D: Key = phys(Row::Home, 2);
const KEY_G: Key = phys(Row::Home, 4);
const KEY_U: Key = phys(Row::Top, 6);
const KEY_N: Key = phys(Row::Bottom, 5);
const KEY_P: Key = phys(Row::Top, 9);

fn dir_of(key: Key) -> Option<Dir> {
    match key {
        KEY_H => Some(Dir::Left),
        KEY_J => Some(Dir::Down),
        KEY_K => Some(Dir::Up),
        KEY_L => Some(Dir::Right),
        _ => None,
    }
}

/// `1`–`9` on the number row → area index `0`–`8`.
fn digit_index(key: Key) -> Option<usize> {
    match key {
        Key::Phys(PhysKey {
            row: Row::Number,
            col,
        }) if (0..=8).contains(&col) => Some(col as usize),
        _ => None,
    }
}

/// Key decisions for one scroll mode session. Lives on the hook thread.
#[derive(Debug, Clone)]
pub struct ScrollMode {
    areas: Vec<Area>,
    active: usize,
    chords: Chords,
    /// Direction keys currently held (pressed in this session).
    held: [bool; 4],
    pending_g: bool,
    unused_streak: u8,
    /// Keys whose press was swallowed: their repeats and release are
    /// swallowed too.
    swallowed: HashSet<Key>,
    /// An Alt press seen in this session with nothing after it yet.
    alt_alone: bool,
    /// Ctrl / Alt / Meta held since before the session (the hotkey's own
    /// modifiers) and not released yet. They don't make a key a command
    /// chord: `Ctrl+Alt+J`, then `k` with Ctrl+Alt still down, scrolls.
    stale: Mods,
    fast: bool,
    exited: bool,
}

impl ScrollMode {
    /// A new session. `held` is the real modifier state when the mode
    /// started (the hotkey's own Ctrl+Alt are usually still down).
    pub fn new(chords: Chords, held: Mods) -> Self {
        Self {
            areas: Vec::new(),
            active: 0,
            chords,
            held: [false; 4],
            pending_g: false,
            unused_streak: 0,
            swallowed: HashSet::new(),
            alt_alone: false,
            stale: Mods {
                shift: false,
                ..held
            },
            fast: held.shift,
            exited: false,
        }
    }

    /// `key` was already held when the session started (the hotkey's own
    /// key): its auto-repeats and release are swallowed, so holding the
    /// hotkey a little long doesn't toggle the mode straight off. The
    /// caller also feeds it to its `KeyTracker` as a press, so repeats are
    /// flagged.
    pub fn seed_held_key(&mut self, key: Key) {
        self.swallowed.insert(key);
    }

    /// The known areas.
    pub fn areas(&self) -> &[Area] {
        &self.areas
    }

    /// Index of the active area.
    pub fn active(&self) -> usize {
        self.active
    }

    /// True once an [`Effect::Exit`] was produced. Every later event
    /// passes, except the repeats and release of a key whose press was
    /// swallowed (no orphan key-ups reach the app).
    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// Replace the area list (discovery finished) and set the active one.
    pub fn on_areas(&mut self, areas: Vec<Area>, active: usize) {
        self.active = active.min(areas.len().saturating_sub(1));
        self.areas = areas;
    }

    /// End the session from outside the keyboard (click, foreground
    /// change, lock). Returns the effects to run (empty if already ended).
    pub fn exit(&mut self, reason: ExitReason) -> Vec<Effect> {
        if self.exited {
            return Vec::new();
        }
        self.exited = true;
        vec![Effect::Exit(reason)]
    }

    /// Decide what to do with one key event.
    pub fn on_key(&mut self, ev: KeyEvent) -> (Verdict, Vec<Effect>) {
        if self.exited {
            let verdict = match (ev.down, ev.repeat) {
                (false, _) if self.swallowed.remove(&ev.key) => Verdict::Swallow,
                (true, true) if self.swallowed.contains(&ev.key) => Verdict::Swallow,
                _ => Verdict::Pass,
            };
            return (verdict, Vec::new());
        }
        if ev.key.is_modifier() {
            return (Verdict::Pass, self.on_modifier(ev));
        }
        self.alt_alone = false;

        if !ev.down {
            let mut fx = Vec::new();
            if let Some(dir) = dir_of(ev.key) {
                if std::mem::take(&mut self.held[dir.index()]) {
                    fx.push(Effect::KeyMotion {
                        dir,
                        down: false,
                        fast: self.fast,
                    });
                }
            }
            let verdict = if self.swallowed.remove(&ev.key) {
                Verdict::Swallow
            } else {
                Verdict::Pass
            };
            return (verdict, fx);
        }
        if ev.repeat {
            // Auto-repeat: Motion has its own clock, so repeats only need
            // the same verdict as the first press.
            let verdict = if self.swallowed.contains(&ev.key) {
                Verdict::Swallow
            } else {
                Verdict::Pass
            };
            return (verdict, Vec::new());
        }

        let (verdict, fx) = self.on_press(ev);
        if verdict == Verdict::Swallow {
            self.swallowed.insert(ev.key);
        }
        (verdict, fx)
    }

    fn on_modifier(&mut self, ev: KeyEvent) -> Vec<Effect> {
        let mut fx = Vec::new();
        let is_alt = ev.key == Key::Named(NamedKey::Alt);
        if ev.down {
            if !ev.repeat {
                // Only an Alt pressed in this session can be a lone tap;
                // the entry chord's Alt was pressed before the hook.
                self.alt_alone = is_alt;
            }
        } else {
            match ev.key {
                Key::Named(NamedKey::Ctrl) => self.stale.ctrl = false,
                Key::Named(NamedKey::Alt) => self.stale.alt = false,
                Key::Named(NamedKey::Meta) => self.stale.meta = false,
                _ => {}
            }
            if is_alt && std::mem::take(&mut self.alt_alone) {
                fx.push(Effect::MaskAlt);
            } else {
                self.alt_alone = false;
            }
        }
        if ev.key == Key::Named(NamedKey::Shift) && ev.mods.shift != self.fast {
            self.fast = ev.mods.shift;
            fx.push(Effect::Fast(self.fast));
        }
        fx
    }

    fn on_press(&mut self, ev: KeyEvent) -> (Verdict, Vec<Effect>) {
        let pending_g = std::mem::take(&mut self.pending_g);

        if let Some(c) = self.chords.scroll {
            if c.matches(&ev) {
                return (Verdict::Swallow, self.exit(ExitReason::Hotkey));
            }
        }
        for (chord, kind) in [
            (self.chords.pick_element, PickKind::Element),
            (self.chords.pick_window, PickKind::Window),
        ] {
            if chord.is_some_and(|c| c.matches(&ev)) {
                let mut fx = self.exit(ExitReason::Pick);
                fx.push(Effect::RunPick(kind));
                return (Verdict::Swallow, fx);
            }
        }
        if self.chords.open_settings.is_some_and(|c| c.matches(&ev)) {
            let mut fx = self.exit(ExitReason::Settings);
            fx.push(Effect::OpenSettings);
            return (Verdict::Swallow, fx);
        }

        // Modifiers held over from the hotkey don't count.
        let m = Mods {
            shift: ev.mods.shift,
            ctrl: ev.mods.ctrl && !self.stale.ctrl,
            alt: ev.mods.alt && !self.stale.alt,
            meta: ev.mods.meta && !self.stale.meta,
        };
        if m.ctrl || m.alt || m.meta {
            let ctrl_only = m.ctrl && !m.alt && !m.meta && !m.shift;
            if ctrl_only && (ev.key == KEY_N || ev.key == KEY_P) {
                self.unused_streak = 0;
                let fx = self.step_area(ev.key == KEY_N);
                return (Verdict::Swallow, fx);
            }
            return (Verdict::Pass, Vec::new());
        }

        let shift = ev.mods.shift;
        let fx = match ev.key {
            Key::Named(NamedKey::Esc) => self.exit(ExitReason::Esc),
            k if dir_of(k).is_some() => {
                let dir = dir_of(k).unwrap_or(Dir::Down);
                self.held[dir.index()] = true;
                vec![Effect::KeyMotion {
                    dir,
                    down: true,
                    fast: self.fast,
                }]
            }
            KEY_D => vec![Effect::Jump(Jump::HalfDown)],
            KEY_U => vec![Effect::Jump(Jump::HalfUp)],
            KEY_G if shift => vec![Effect::Jump(Jump::Bottom)],
            KEY_G if pending_g => vec![Effect::Jump(Jump::Top)],
            KEY_G => {
                self.pending_g = true;
                Vec::new()
            }
            Key::Named(NamedKey::Tab) => self.step_area(!shift),
            Key::Named(NamedKey::Down) => self.step_area(true),
            Key::Named(NamedKey::Up) => self.step_area(false),
            k if digit_index(k).is_some() => {
                let i = digit_index(k).unwrap_or(0);
                if i < self.areas.len() {
                    self.active = i;
                    vec![Effect::SelectArea(i)]
                } else {
                    Vec::new()
                }
            }
            _ => {
                self.unused_streak += 1;
                if self.unused_streak >= UNUSED_KEYS_TO_EXIT {
                    return (Verdict::Swallow, self.exit(ExitReason::UnusedKeys));
                }
                return (Verdict::Swallow, Vec::new());
            }
        };
        self.unused_streak = 0;
        (Verdict::Swallow, fx)
    }

    fn step_area(&mut self, forward: bool) -> Vec<Effect> {
        let n = self.areas.len();
        if n == 0 {
            return Vec::new();
        }
        self.active = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        vec![Effect::SelectArea(self.active)]
    }
}

// ---------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------

/// Tuning for [`Motion`]. Distances are in lines (the shell converts
/// through the OS "lines per wheel notch" setting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionConfig {
    /// Lines one tap scrolls.
    pub step: f64,
    /// Glide speed while held, lines per second.
    pub speed: f64,
    /// Speed and step multiplier while Shift is held.
    pub fast_multiplier: f64,
    /// A key held this long (ms) starts gliding.
    pub hold_ms: f64,
    /// Time (ms) to ramp from 0 to full glide speed.
    pub ramp_ms: f64,
    /// Time (ms) the tap step is spread over.
    pub pulse_ms: f64,
    /// Time (ms) to ease from full speed to 0 after release.
    pub ease_out_ms: f64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            step: 3.0,
            speed: 40.0,
            fast_multiplier: 3.0,
            hold_ms: 150.0,
            ramp_ms: 150.0,
            pulse_ms: 80.0,
            ease_out_ms: 80.0,
        }
    }
}

impl MotionConfig {
    /// Clamp every value to a sane range (config input is untrusted).
    pub fn clamped(self) -> Self {
        let c = |v: f64, lo: f64, hi: f64, def: f64| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                def
            }
        };
        let d = Self::default();
        Self {
            step: c(self.step, 0.25, 100.0, d.step),
            speed: c(self.speed, 1.0, 2000.0, d.speed),
            fast_multiplier: c(self.fast_multiplier, 1.0, 20.0, d.fast_multiplier),
            hold_ms: c(self.hold_ms, 0.0, 2000.0, d.hold_ms),
            ramp_ms: c(self.ramp_ms, 1.0, 2000.0, d.ramp_ms),
            pulse_ms: c(self.pulse_ms, 1.0, 1000.0, d.pulse_ms),
            ease_out_ms: c(self.ease_out_ms, 1.0, 2000.0, d.ease_out_ms),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct DirState {
    held: bool,
    pressed_at: f64,
    /// Total lines of the current tap pulse, and how much was delivered.
    pulse_total: f64,
    pulse_done: f64,
    pulse_start: f64,
    /// Current glide velocity, lines/s.
    v: f64,
    /// Deceleration after release, lines/s².
    decel: f64,
}

/// Smooth scrolling state for all four directions.
///
/// Call [`Motion::press`] / [`Motion::release`] on key edges and
/// [`Motion::advance`] every tick; `advance` returns the lines to scroll
/// since the previous call as `(dx, dy)`.
#[derive(Debug, Clone)]
pub struct Motion {
    cfg: MotionConfig,
    dirs: [DirState; 4],
    fast: bool,
    last: Option<f64>,
}

/// Ease-in-out (smoothstep) on `0..=1`.
fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Motion {
    /// Idle motion with the given tuning.
    pub fn new(cfg: MotionConfig) -> Self {
        Self {
            cfg: cfg.clamped(),
            dirs: [DirState::default(); 4],
            fast: false,
            last: None,
        }
    }

    /// Replace the tuning (config hot-reload). Takes effect from the next
    /// tick.
    pub fn set_config(&mut self, cfg: MotionConfig) {
        self.cfg = cfg.clamped();
    }

    /// Shift went down or up.
    pub fn set_fast(&mut self, fast: bool) {
        self.fast = fast;
    }

    fn mult(&self) -> f64 {
        if self.fast {
            self.cfg.fast_multiplier
        } else {
            1.0
        }
    }

    /// A direction key went down at `now_ms`.
    pub fn press(&mut self, dir: Dir, now_ms: f64) {
        if self.last.is_none() {
            self.last = Some(now_ms);
        }
        let step = self.cfg.step * self.mult();
        let d = &mut self.dirs[dir.index()];
        if d.held {
            return;
        }
        d.held = true;
        d.decel = 0.0;
        if d.v > 0.0 {
            // Pressed again while still easing out: carry on gliding from
            // the current speed (ramping back up now) instead of coasting
            // for `hold_ms` or adding a step on top.
            d.pressed_at = now_ms - self.cfg.hold_ms;
            return;
        }
        // Whatever was left of an earlier pulse is still delivered.
        let leftover = d.pulse_total - d.pulse_done;
        d.pressed_at = now_ms;
        d.pulse_total = step + leftover.max(0.0);
        d.pulse_done = 0.0;
        d.pulse_start = now_ms;
    }

    /// A direction key went up.
    pub fn release(&mut self, dir: Dir) {
        let ease_out = self.cfg.ease_out_ms / 1000.0;
        let d = &mut self.dirs[dir.index()];
        if !d.held {
            return;
        }
        d.held = false;
        d.decel = d.v / ease_out;
    }

    /// Stop everything at once (mode exit).
    pub fn stop(&mut self) {
        self.dirs = [DirState::default(); 4];
        self.last = None;
    }

    /// True when nothing is moving or pending, so the engine can stop
    /// ticking.
    pub fn is_idle(&self) -> bool {
        self.dirs
            .iter()
            .all(|d| !d.held && d.v == 0.0 && d.pulse_done >= d.pulse_total)
    }

    /// Lines to scroll since the previous call, `(dx, dy)`.
    pub fn advance(&mut self, now_ms: f64) -> (f64, f64) {
        let last = self.last.unwrap_or(now_ms);
        let dt = ((now_ms - last) / 1000.0).max(0.0);
        self.last = Some(now_ms);
        let cfg = self.cfg;
        let target_v = cfg.speed * self.mult();
        let ramp_s = cfg.ramp_ms / 1000.0;
        let (mut dx, mut dy) = (0.0, 0.0);
        for dir in Dir::ALL {
            let d = &mut self.dirs[dir.index()];
            let mut lines = 0.0;

            // Tap pulse.
            if d.pulse_done < d.pulse_total {
                let t = (now_ms - d.pulse_start) / cfg.pulse_ms;
                let pos = d.pulse_total * ease(t);
                lines += pos - d.pulse_done;
                d.pulse_done = pos;
                if t >= 1.0 {
                    d.pulse_done = d.pulse_total;
                }
            }

            // Glide.
            let v0 = d.v;
            if d.held {
                if now_ms - d.pressed_at >= cfg.hold_ms {
                    // Move toward the target so that a full change takes
                    // `ramp_ms`: 0 → speed on a hold, speed ⇄ fast when
                    // Shift changes mid-hold.
                    let rate = target_v.max(d.v) / ramp_s;
                    d.v = if d.v < target_v {
                        (d.v + rate * dt).min(target_v)
                    } else {
                        (d.v - rate * dt).max(target_v)
                    };
                }
            } else if d.v > 0.0 {
                d.v = (d.v - d.decel * dt).max(0.0);
            }
            lines += (v0 + d.v) / 2.0 * dt;

            let (ux, uy) = dir.unit();
            dx += ux * lines;
            dy += uy * lines;
        }
        if self.is_idle() {
            self.last = None;
        }
        (dx, dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyTracker, Mods};

    const ESC: Key = Key::Named(NamedKey::Esc);
    const TAB: Key = Key::Named(NamedKey::Tab);
    const SHIFT: Key = Key::Named(NamedKey::Shift);
    const CTRL: Key = Key::Named(NamedKey::Ctrl);
    const ALT: Key = Key::Named(NamedKey::Alt);
    const KEY_X: Key = phys(Row::Bottom, 1);
    const KEY_C: Key = phys(Row::Bottom, 2);
    const KEY_1: Key = phys(Row::Number, 0);
    const KEY_3: Key = phys(Row::Number, 2);
    const SPACE: Key = Key::Named(NamedKey::Space);

    fn area(i: i32) -> Area {
        Area {
            bounds: Bounds {
                x: i * 100,
                y: 0,
                width: 100,
                height: 100,
            },
            can_h: false,
            can_v: true,
        }
    }

    fn chords() -> Chords {
        let ca = Mods {
            ctrl: true,
            alt: true,
            ..Mods::NONE
        };
        Chords {
            scroll: Some(Chord {
                mods: ca,
                key: KEY_J,
            }),
            pick_element: Some(Chord {
                mods: Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                },
                key: SPACE,
            }),
            pick_window: Some(Chord {
                mods: ca,
                key: SPACE,
            }),
            open_settings: Some(Chord {
                mods: Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                },
                key: phys(Row::Bottom, 7), // ","
            }),
        }
    }

    /// A session plus a key tracker, as the hook thread holds them.
    struct Rig {
        mode: ScrollMode,
        t: KeyTracker,
    }

    impl Rig {
        fn new() -> Self {
            let mut mode = ScrollMode::new(chords(), Mods::NONE);
            mode.on_areas(vec![area(0), area(1), area(2)], 0);
            Self {
                mode,
                t: KeyTracker::new(),
            }
        }
        fn ev(&mut self, key: Key, down: bool) -> (Verdict, Vec<Effect>) {
            let e = self.t.track(key, down);
            self.mode.on_key(e)
        }
        fn tap(&mut self, key: Key) -> (Verdict, Vec<Effect>, Verdict) {
            let (v, fx) = self.ev(key, true);
            let (up, _) = self.ev(key, false);
            (v, fx, up)
        }
    }

    #[test]
    fn hjkl_press_and_release_drive_motion_and_are_swallowed() {
        let mut r = Rig::new();
        for (key, dir) in [
            (KEY_H, Dir::Left),
            (KEY_J, Dir::Down),
            (KEY_K, Dir::Up),
            (KEY_L, Dir::Right),
        ] {
            let (v, fx) = r.ev(key, true);
            assert_eq!(v, Verdict::Swallow);
            assert_eq!(
                fx,
                vec![Effect::KeyMotion {
                    dir,
                    down: true,
                    fast: false
                }]
            );
            let (v, fx) = r.ev(key, true);
            assert_eq!((v, fx.len()), (Verdict::Swallow, 0), "repeat");
            let (v, fx) = r.ev(key, false);
            assert_eq!(v, Verdict::Swallow);
            assert_eq!(
                fx,
                vec![Effect::KeyMotion {
                    dir,
                    down: false,
                    fast: false
                }]
            );
        }
    }

    #[test]
    fn held_directions_combine() {
        let mut r = Rig::new();
        r.ev(KEY_J, true);
        let (_, fx) = r.ev(KEY_L, true);
        assert!(matches!(
            fx[0],
            Effect::KeyMotion {
                dir: Dir::Right,
                down: true,
                ..
            }
        ));
        let (_, fx) = r.ev(KEY_J, false);
        assert!(matches!(
            fx[0],
            Effect::KeyMotion {
                dir: Dir::Down,
                down: false,
                ..
            }
        ));
    }

    #[test]
    fn shift_reports_fast_and_passes() {
        let mut r = Rig::new();
        let (v, fx) = r.ev(SHIFT, true);
        assert_eq!((v, fx), (Verdict::Pass, vec![Effect::Fast(true)]));
        let (_, fx) = r.ev(KEY_J, true);
        assert_eq!(
            fx,
            vec![Effect::KeyMotion {
                dir: Dir::Down,
                down: true,
                fast: true
            }]
        );
        let (_, fx) = r.ev(SHIFT, false);
        assert_eq!(fx, vec![Effect::Fast(false)]);
    }

    #[test]
    fn jumps() {
        let mut r = Rig::new();
        assert_eq!(r.tap(KEY_D).1, vec![Effect::Jump(Jump::HalfDown)]);
        assert_eq!(r.tap(KEY_U).1, vec![Effect::Jump(Jump::HalfUp)]);
        assert_eq!(r.tap(KEY_G).1, vec![]);
        assert_eq!(r.tap(KEY_G).1, vec![Effect::Jump(Jump::Top)]);
        r.ev(SHIFT, true);
        assert_eq!(r.tap(KEY_G).1, vec![Effect::Jump(Jump::Bottom)]);
    }

    #[test]
    fn another_key_cancels_pending_g() {
        let mut r = Rig::new();
        r.tap(KEY_G);
        r.tap(KEY_D);
        assert_eq!(r.tap(KEY_G).1, vec![], "first g again");
    }

    #[test]
    fn area_switching() {
        let mut r = Rig::new();
        assert_eq!(r.tap(TAB).1, vec![Effect::SelectArea(1)]);
        assert_eq!(
            r.tap(Key::Named(NamedKey::Down)).1,
            vec![Effect::SelectArea(2)]
        );
        assert_eq!(r.tap(TAB).1, vec![Effect::SelectArea(0)], "wraps");
        assert_eq!(
            r.tap(Key::Named(NamedKey::Up)).1,
            vec![Effect::SelectArea(2)]
        );
        r.ev(SHIFT, true);
        assert_eq!(r.tap(TAB).1, vec![Effect::SelectArea(1)], "Shift+Tab");
        r.ev(SHIFT, false);
        assert_eq!(r.tap(KEY_1).1, vec![Effect::SelectArea(0)]);
        assert_eq!(r.tap(KEY_3).1, vec![Effect::SelectArea(2)]);
        assert_eq!(r.mode.active(), 2);
    }

    #[test]
    fn ctrl_n_and_p_switch_areas_and_are_swallowed() {
        let mut r = Rig::new();
        r.ev(CTRL, true);
        let (v, fx, up) = r.tap(KEY_N);
        assert_eq!(
            (v, fx, up),
            (
                Verdict::Swallow,
                vec![Effect::SelectArea(1)],
                Verdict::Swallow
            )
        );
        assert_eq!(r.tap(KEY_P).1, vec![Effect::SelectArea(0)]);
    }

    #[test]
    fn digit_beyond_areas_or_before_discovery_does_nothing() {
        let mut r = Rig::new();
        r.mode.on_areas(Vec::new(), 0);
        let (v, fx, _) = r.tap(KEY_3);
        assert_eq!((v, fx), (Verdict::Swallow, vec![]));
        assert_eq!(r.tap(TAB).1, vec![]);
    }

    #[test]
    fn esc_exits_and_then_everything_passes() {
        let mut r = Rig::new();
        let (v, fx) = r.ev(ESC, true);
        assert_eq!(
            (v, fx),
            (Verdict::Swallow, vec![Effect::Exit(ExitReason::Esc)])
        );
        assert!(r.mode.has_exited());
        assert_eq!(r.ev(KEY_J, true).0, Verdict::Pass);
        assert!(r.mode.exit(ExitReason::Click).is_empty(), "exits once");
    }

    #[test]
    fn three_unused_keys_in_a_row_exit() {
        let mut r = Rig::new();
        assert_eq!(r.tap(KEY_X), (Verdict::Swallow, vec![], Verdict::Swallow));
        assert_eq!(r.tap(KEY_C).1, vec![]);
        assert_eq!(r.tap(KEY_X).1, vec![Effect::Exit(ExitReason::UnusedKeys)]);
    }

    #[test]
    fn bound_key_resets_the_unused_streak() {
        let mut r = Rig::new();
        r.tap(KEY_X);
        r.tap(KEY_X);
        r.tap(KEY_D);
        r.tap(KEY_X);
        assert_eq!(r.tap(KEY_X).1, vec![], "streak restarted after d");
        assert_eq!(r.tap(KEY_X).1, vec![Effect::Exit(ExitReason::UnusedKeys)]);
    }

    #[test]
    fn shift_plus_unused_letter_counts_as_unused() {
        let mut r = Rig::new();
        r.ev(SHIFT, true);
        r.tap(KEY_X);
        r.tap(KEY_X);
        assert_eq!(r.tap(KEY_X).1, vec![Effect::Exit(ExitReason::UnusedKeys)]);
    }

    #[test]
    fn command_chords_pass_and_leave_the_streak_alone() {
        let mut r = Rig::new();
        r.tap(KEY_X);
        r.tap(KEY_X);
        r.ev(CTRL, true);
        assert_eq!(
            r.tap(KEY_C),
            (Verdict::Pass, vec![], Verdict::Pass),
            "Ctrl+C"
        );
        r.ev(CTRL, false);
        r.ev(ALT, true);
        assert_eq!(r.tap(TAB).0, Verdict::Pass, "Alt+Tab");
        r.ev(ALT, false);
        assert_eq!(r.tap(KEY_X).1, vec![Effect::Exit(ExitReason::UnusedKeys)]);
    }

    #[test]
    fn scroll_hotkey_exits() {
        let mut r = Rig::new();
        r.ev(CTRL, true);
        r.ev(ALT, true);
        let (v, fx) = r.ev(KEY_J, true);
        assert_eq!(
            (v, fx),
            (Verdict::Swallow, vec![Effect::Exit(ExitReason::Hotkey)])
        );
    }

    #[test]
    fn pick_hotkeys_exit_then_run_the_pick() {
        for (mods, kind) in [
            ([CTRL, SHIFT], PickKind::Element),
            ([CTRL, ALT], PickKind::Window),
        ] {
            let mut r = Rig::new();
            for m in mods {
                r.ev(m, true);
            }
            let (v, fx) = r.ev(SPACE, true);
            assert_eq!(v, Verdict::Swallow);
            assert_eq!(
                fx,
                vec![Effect::Exit(ExitReason::Pick), Effect::RunPick(kind)]
            );
        }
    }

    #[test]
    fn lone_alt_tap_is_masked_but_passes() {
        let mut r = Rig::new();
        assert_eq!(r.ev(ALT, true), (Verdict::Pass, vec![]));
        assert_eq!(r.ev(ALT, false), (Verdict::Pass, vec![Effect::MaskAlt]));
    }

    #[test]
    fn alt_with_another_key_is_not_masked() {
        let mut r = Rig::new();
        r.ev(ALT, true);
        r.tap(TAB);
        assert_eq!(r.ev(ALT, false), (Verdict::Pass, vec![]));
        r.ev(CTRL, true);
        r.ev(ALT, true);
        r.ev(CTRL, false);
        assert_eq!(r.ev(ALT, false).1, vec![], "Ctrl came after Alt");
    }

    #[test]
    fn releasing_the_entry_chord_is_not_a_lone_alt() {
        // The hotkey's Ctrl+Alt were pressed before the hook existed.
        let (mut mode, mut t) = entry_rig(false);
        assert_eq!(mode.on_key(t.track(KEY_J, false)).0, Verdict::Pass);
        assert_eq!(mode.on_key(t.track(CTRL, false)), (Verdict::Pass, vec![]));
        assert_eq!(mode.on_key(t.track(ALT, false)), (Verdict::Pass, vec![]));
    }

    #[test]
    fn release_of_a_key_pressed_before_the_mode_passes() {
        let mut r = Rig::new();
        assert_eq!(r.ev(KEY_X, false), (Verdict::Pass, vec![]));
        assert_eq!(r.ev(KEY_J, false), (Verdict::Pass, vec![]));
    }

    #[test]
    fn external_exits() {
        let mut r = Rig::new();
        assert_eq!(
            r.mode.exit(ExitReason::Click),
            vec![Effect::Exit(ExitReason::Click)]
        );
        assert!(r.mode.exit(ExitReason::Foreground).is_empty());
    }

    #[test]
    fn exit_reasons_are_whitelist_friendly() {
        for r in [
            ExitReason::Esc,
            ExitReason::Hotkey,
            ExitReason::Click,
            ExitReason::UnusedKeys,
            ExitReason::Foreground,
            ExitReason::Pick,
            ExitReason::Settings,
            ExitReason::Lock,
        ] {
            let s = r.as_str();
            assert!(!s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    // ---- session start: the hotkey's own keys are still held ----

    /// Seed a session the way the hook does after `Ctrl+Alt+J`: Ctrl, Alt
    /// and J still down.
    fn entry_rig(j_held: bool) -> (ScrollMode, KeyTracker) {
        let held = Mods {
            ctrl: true,
            alt: true,
            ..Mods::NONE
        };
        let mut mode = ScrollMode::new(chords(), held);
        let mut t = KeyTracker::new();
        t.track(CTRL, true);
        t.track(ALT, true);
        if j_held {
            t.track(KEY_J, true);
            mode.seed_held_key(KEY_J);
        }
        (mode, t)
    }

    #[test]
    fn holding_the_hotkey_long_does_not_toggle_the_mode_off() {
        let (mut m, mut t) = entry_rig(true);
        for _ in 0..5 {
            let (v, fx) = m.on_key(t.track(KEY_J, true)); // auto-repeat
            assert_eq!((v, fx), (Verdict::Swallow, vec![]));
        }
        assert_eq!(m.on_key(t.track(KEY_J, false)), (Verdict::Swallow, vec![]));
        assert!(!m.has_exited());
        // Pressing the hotkey again after letting go of J does toggle off.
        let (_, fx) = m.on_key(t.track(KEY_J, true));
        assert_eq!(fx, vec![Effect::Exit(ExitReason::Hotkey)]);
    }

    #[test]
    fn scroll_keys_work_while_the_hotkey_mods_are_still_held() {
        let (mut m, mut t) = entry_rig(true);
        m.on_key(t.track(KEY_J, false));
        let (v, fx) = m.on_key(t.track(KEY_K, true));
        assert_eq!(v, Verdict::Swallow);
        assert!(matches!(
            fx[..],
            [Effect::KeyMotion {
                dir: Dir::Up,
                down: true,
                ..
            }]
        ));
    }

    #[test]
    fn hotkey_mods_count_again_once_released_and_pressed() {
        let (mut m, mut t) = entry_rig(false);
        m.on_key(t.track(ALT, false));
        m.on_key(t.track(CTRL, false));
        m.on_key(t.track(CTRL, true));
        assert_eq!(
            m.on_key(t.track(KEY_C, true)).0,
            Verdict::Pass,
            "fresh Ctrl+C"
        );
    }

    #[test]
    fn released_hotkey_mod_stops_being_stale_even_if_the_other_stays() {
        let (mut m, mut t) = entry_rig(false);
        m.on_key(t.track(CTRL, false));
        m.on_key(t.track(CTRL, true));
        // Ctrl is fresh, Alt still stale: Ctrl+N switches areas.
        m.on_areas(vec![area(0), area(1)], 0);
        let (v, fx) = m.on_key(t.track(KEY_N, true));
        assert_eq!((v, fx), (Verdict::Swallow, vec![Effect::SelectArea(1)]));
    }

    #[test]
    fn settings_hotkey_exits_then_opens_settings() {
        let mut r = Rig::new();
        r.ev(CTRL, true);
        r.ev(SHIFT, true);
        let (v, fx) = r.ev(phys(Row::Bottom, 7), true);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(
            fx,
            vec![Effect::Exit(ExitReason::Settings), Effect::OpenSettings]
        );
    }

    #[test]
    fn swallowed_key_up_after_exit_is_swallowed() {
        let mut r = Rig::new();
        r.ev(KEY_J, true);
        r.ev(ESC, true);
        assert_eq!(r.ev(KEY_J, true).0, Verdict::Swallow, "repeat of held j");
        assert_eq!(r.ev(ESC, false).0, Verdict::Swallow);
        assert_eq!(r.ev(KEY_J, false).0, Verdict::Swallow);
        assert_eq!(r.ev(KEY_X, true).0, Verdict::Pass, "new keys pass");
        assert_eq!(r.ev(KEY_X, false).0, Verdict::Pass);
    }

    // ---- Motion ----

    /// Run `m` at 120 Hz from `from` to `to` ms, summing the motion.
    fn run(m: &mut Motion, from: f64, to: f64) -> (f64, f64) {
        let (mut x, mut y) = (0.0, 0.0);
        let mut t = from;
        while t < to {
            t = (t + 1000.0 / 120.0).min(to);
            let (dx, dy) = m.advance(t);
            x += dx;
            y += dy;
        }
        (x, y)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn tap_moves_exactly_one_step() {
        let mut m = Motion::new(MotionConfig::default());
        m.press(Dir::Down, 0.0);
        let mut y = run(&mut m, 0.0, 40.0).1;
        m.release(Dir::Down);
        y += run(&mut m, 40.0, 500.0).1;
        assert!(close(y, 3.0), "moved {y}");
        assert!(m.is_idle());
    }

    #[test]
    fn tap_directions_have_the_right_sign() {
        let mut m = Motion::new(MotionConfig::default());
        m.press(Dir::Up, 0.0);
        m.release(Dir::Up);
        m.press(Dir::Left, 0.0);
        m.release(Dir::Left);
        let (x, y) = run(&mut m, 0.0, 300.0);
        assert!(close(x, -3.0) && close(y, -3.0), "{x} {y}");
    }

    #[test]
    fn fast_tap_is_multiplied() {
        let mut m = Motion::new(MotionConfig::default());
        m.set_fast(true);
        m.press(Dir::Down, 0.0);
        m.release(Dir::Down);
        assert!(close(run(&mut m, 0.0, 300.0).1, 9.0));
    }

    #[test]
    fn hold_ramps_up_to_full_speed() {
        let cfg = MotionConfig::default();
        let mut m = Motion::new(cfg);
        m.press(Dir::Down, 0.0);
        let before_hold = run(&mut m, 0.0, cfg.hold_ms - 1.0).1;
        assert!(before_hold <= cfg.step + 1e-9, "no glide before hold_ms");
        // After hold + ramp, one second of gliding is about `speed` lines.
        run(&mut m, cfg.hold_ms - 1.0, cfg.hold_ms + cfg.ramp_ms + 20.0);
        let t0 = cfg.hold_ms + cfg.ramp_ms + 20.0;
        let y = run(&mut m, t0, t0 + 1000.0).1;
        assert!((y - cfg.speed).abs() < 0.5, "one second at speed moved {y}");
    }

    #[test]
    fn release_during_ramp_stops_cleanly() {
        let cfg = MotionConfig::default();
        let mut m = Motion::new(cfg);
        m.press(Dir::Down, 0.0);
        run(&mut m, 0.0, cfg.hold_ms + cfg.ramp_ms / 2.0);
        m.release(Dir::Down);
        let t = cfg.hold_ms + cfg.ramp_ms / 2.0;
        let tail = run(&mut m, t, t + cfg.ease_out_ms + 20.0).1;
        assert!(tail > 0.0 && tail < cfg.speed, "eased out over {tail}");
        assert!(m.is_idle());
        assert_eq!(m.advance(t + 1000.0), (0.0, 0.0));
    }

    #[test]
    fn shift_mid_hold_changes_speed_smoothly() {
        let cfg = MotionConfig::default();
        let mut m = Motion::new(cfg);
        m.press(Dir::Down, 0.0);
        let t0 = cfg.hold_ms + cfg.ramp_ms + 50.0;
        run(&mut m, 0.0, t0);
        m.set_fast(true);
        // The very next tick moves less than a full-fast tick would.
        let tick = 1000.0 / 120.0;
        let first = m.advance(t0 + tick).1;
        let full_fast_tick = cfg.speed * cfg.fast_multiplier * tick / 1000.0;
        assert!(first < full_fast_tick * 0.9, "jumped to {first}");
        let t1 = t0 + tick + 1000.0;
        run(&mut m, t0 + tick, t1);
        let y = run(&mut m, t1, t1 + 1000.0).1;
        let target = cfg.speed * cfg.fast_multiplier;
        assert!((y - target).abs() < 1.0, "fast speed {y} vs {target}");
    }

    #[test]
    fn opposite_directions_cancel() {
        let mut m = Motion::new(MotionConfig::default());
        m.press(Dir::Down, 0.0);
        m.press(Dir::Up, 0.0);
        let (_, y) = run(&mut m, 0.0, 1000.0);
        assert!(close(y, 0.0), "{y}");
    }

    #[test]
    fn repress_during_ease_out_glides_on_without_coasting() {
        let cfg = MotionConfig::default();
        let mut m = Motion::new(cfg);
        m.press(Dir::Down, 0.0);
        run(&mut m, 0.0, 600.0);
        m.release(Dir::Down);
        run(&mut m, 600.0, 620.0);
        m.press(Dir::Down, 620.0);
        let a = run(&mut m, 620.0, 640.0).1;
        let b = run(&mut m, 640.0, 660.0).1;
        assert!(b > a, "ramping back up, not coasting: {a} then {b}");
        let c = run(&mut m, 660.0, 1660.0).1;
        assert!((c - cfg.speed).abs() < 1.0, "back at full speed: {c}");
    }

    #[test]
    fn stop_clears_everything() {
        let mut m = Motion::new(MotionConfig::default());
        m.press(Dir::Down, 0.0);
        run(&mut m, 0.0, 500.0);
        m.stop();
        assert!(m.is_idle());
        assert_eq!(m.advance(600.0), (0.0, 0.0));
    }

    #[test]
    fn config_is_clamped() {
        let c = MotionConfig {
            step: -5.0,
            speed: f64::NAN,
            fast_multiplier: 0.0,
            ramp_ms: 0.0,
            ..MotionConfig::default()
        }
        .clamped();
        assert!(c.step > 0.0);
        assert_eq!(c.speed, MotionConfig::default().speed);
        assert!(c.fast_multiplier >= 1.0 && c.ramp_ms >= 1.0);
    }
}
