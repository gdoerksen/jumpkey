//! Scroll mode's engine: a worker thread that turns [`Motion`] into
//! mouse-wheel input and runs jumps.
//!
//! - While anything moves it ticks about 120 times a second, samples
//!   [`Motion`] for the lines to scroll, converts them to wheel units
//!   through the Windows "lines per notch" setting (120 units = that many
//!   lines) and sends `MOUSEEVENTF_WHEEL` / `HWHEEL`, tagged with
//!   [`JUMPKEY_INPUT_TAG`]. Fractions carry over between ticks.
//! - Wheel input goes to the window under the pointer, so selecting an
//!   area moves the pointer to its centre first.
//! - Top / bottom and half-page jumps use the area's ScrollPattern
//!   (`SetScrollPercent`) when it has one, else a burst of wheel input.
//!
//! - Apps read Shift and Ctrl from the keyboard state when a wheel event
//!   arrives (Shift+wheel scrolls sideways, Ctrl+wheel zooms). Shift is
//!   scroll mode's "fast" key, and the hotkey's Ctrl may still be down, so
//!   while either is physically held each wheel event is wrapped in tagged
//!   key-ups and key-downs for the held sides, in one `SendInput` batch:
//!   the app sees a plain wheel. If the user lets go of a key around that
//!   moment, the engine releases any key it pressed again that is no
//!   longer really held (the hook keeps the real state in [`RealMods`]).
//!   When the session ends it releases every key it pressed again, held
//!   or not, so a release the hook missed can't leave a key stuck down.
//!   Alt is left alone: lifting it could open the app's menu bar.
//!
//! It has its own [`UIAutomation`] client, so a slow app never blocks the
//! main or hook thread.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use uiautomation::patterns::UIScrollPattern;
use uiautomation::UIAutomation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_LCONTROL, VK_LSHIFT, VK_RCONTROL, VK_RSHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetCursorPos, SystemParametersInfoW, SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::scroll::{Area, Dir, Jump, Motion, MotionConfig};
use crate::windows::scroll::areas::scrollable_at;
use crate::windows::JUMPKEY_INPUT_TAG;

/// One wheel notch.
const WHEEL_DELTA: f64 = 120.0;
/// Tick period while moving (~120 Hz).
const TICK: Duration = Duration::from_millis(8);
/// `SPI_GETWHEELSCROLLLINES` value meaning "one screen per notch".
const WHEEL_PAGESCROLL: u32 = u32::MAX;
/// Lines per notch assumed when the setting is "one screen" or unreadable.
const DEFAULT_LINES: f64 = 3.0;
/// Rough height of a line in physical pixels, for wheel-based half pages.
const LINE_PX_ESTIMATE: f64 = 24.0;
/// Lines sent for a wheel-based top / bottom jump (far enough for any
/// page; apps clamp).
const FAR_LINES: f64 = 20_000.0;
/// Duration and steps of an animated ScrollPattern half page.
const HALF_PAGE_STEPS: u32 = 6;
const HALF_PAGE_STEP: Duration = Duration::from_millis(16);

/// Which Shift / Ctrl keys are **physically** held, as seen by the hook
/// (bits: [`MOD_LSHIFT`] …). Shared by the hook thread (writer) and the
/// engine (reader).
pub type RealMods = Arc<AtomicU8>;

/// [`RealMods`] bit: left Shift.
pub const MOD_LSHIFT: u8 = 1;
/// [`RealMods`] bit: right Shift.
pub const MOD_RSHIFT: u8 = 2;
/// [`RealMods`] bit: left Ctrl.
pub const MOD_LCTRL: u8 = 4;
/// [`RealMods`] bit: right Ctrl.
pub const MOD_RCTRL: u8 = 8;

/// `(bit, virtual key, extended)` for each side of Shift and Ctrl.
const SIDES: [(u8, VIRTUAL_KEY, bool); 4] = [
    (MOD_LSHIFT, VK_LSHIFT, false),
    (MOD_RSHIFT, VK_RSHIFT, false),
    (MOD_LCTRL, VK_LCONTROL, false),
    (MOD_RCTRL, VK_RCONTROL, true),
];

enum Cmd {
    Key { dir: Dir, down: bool, fast: bool },
    Fast(bool),
    Config(MotionConfig),
    Jump(Jump, Area),
    MoveTo(i32, i32),
    Stop,
}

/// Handle to the engine thread. Dropping it stops all motion and joins.
pub struct Engine {
    tx: Option<Sender<Cmd>>,
    join: Option<JoinHandle<()>>,
}

impl Engine {
    /// Start the engine with the given tuning. `real` is the hook's view
    /// of the physically held Shift / Ctrl keys.
    pub fn start(cfg: MotionConfig, real: RealMods) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("jumpkey-scroll-engine".into())
            .spawn(move || run(cfg, rx, real))?;
        Ok(Self {
            tx: Some(tx),
            join: Some(join),
        })
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    /// A direction key went down or up.
    pub fn key(&self, dir: Dir, down: bool, fast: bool) {
        self.send(Cmd::Key { dir, down, fast });
    }

    /// Shift went down or up.
    pub fn fast(&self, fast: bool) {
        self.send(Cmd::Fast(fast));
    }

    /// New tuning (config hot-reload).
    pub fn config(&self, cfg: MotionConfig) {
        self.send(Cmd::Config(cfg));
    }

    /// Jump within `area`.
    pub fn jump(&self, jump: Jump, area: Area) {
        self.send(Cmd::Jump(jump, area));
    }

    /// Move the pointer (wheel input follows it). Queued behind earlier
    /// wheel input so a scroll never lands in the wrong area.
    pub fn move_to(&self, x: i32, y: i32) {
        self.send(Cmd::MoveTo(x, y));
    }

    /// Stop all motion now.
    pub fn stop(&self) {
        self.send(Cmd::Stop);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Closing the channel ends the thread.
        self.tx.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn spi_u32(action: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION) -> u32 {
    let mut v: u32 = 0;
    let ok = unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut v as *mut u32 as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_ok() {
        v
    } else {
        0
    }
}

/// Lines (or characters, horizontally) per wheel notch.
fn per_notch(v: u32) -> f64 {
    if v == 0 || v == WHEEL_PAGESCROLL {
        DEFAULT_LINES
    } else {
        v as f64
    }
}

fn tagged_key(vk: VIRTUAL_KEY, extended: bool, up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: JUMPKEY_INPUT_TAG,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Wheel output with fractional carry and Shift / Ctrl lifting.
struct Wheel {
    lines_v: f64,
    chars_h: f64,
    carry_v: f64,
    carry_h: f64,
    real: RealMods,
    /// Sides this engine pressed again after a wheel event. Released by
    /// [`Self::repair`] once the user is no longer really holding them.
    repressed: u8,
}

impl Wheel {
    fn new(real: RealMods) -> Self {
        Self {
            lines_v: per_notch(spi_u32(SPI_GETWHEELSCROLLLINES)),
            chars_h: per_notch(spi_u32(SPI_GETWHEELSCROLLCHARS)),
            carry_v: 0.0,
            carry_h: 0.0,
            real,
            repressed: 0,
        }
    }

    /// One wheel event, with held Shift / Ctrl lifted around it.
    fn wheel(&mut self, flags: MOUSE_EVENT_FLAGS, delta: i32) {
        let held = self.real.load(Ordering::Acquire);
        let mut batch = Vec::with_capacity(9);
        for (bit, vk, ext) in SIDES {
            if held & bit != 0 {
                batch.push(tagged_key(vk, ext, true));
            }
        }
        batch.push(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: JUMPKEY_INPUT_TAG,
                },
            },
        });
        for (bit, vk, ext) in SIDES {
            if held & bit != 0 {
                batch.push(tagged_key(vk, ext, false));
                self.repressed |= bit;
            }
        }
        send(&batch);
    }

    /// Release keys this engine pressed again that the user has let go of
    /// since (their real release may have come just before our batch).
    fn repair(&mut self) {
        let stale = self.repressed & !self.real.load(Ordering::Acquire);
        if stale == 0 {
            return;
        }
        let ups: Vec<INPUT> = SIDES
            .iter()
            .filter(|(bit, ..)| stale & bit != 0)
            .map(|&(_, vk, ext)| tagged_key(vk, ext, true))
            .collect();
        send(&ups);
        self.repressed &= !stale;
    }

    /// Scroll by `(dx, dy)` lines: `+x` right, `+y` down.
    fn send(&mut self, dx: f64, dy: f64) {
        // Wheel up (positive) scrolls content up, i.e. towards the top.
        self.carry_v += -dy * WHEEL_DELTA / self.lines_v;
        self.carry_h += dx * WHEEL_DELTA / self.chars_h;
        let v = self.carry_v.trunc();
        let h = self.carry_h.trunc();
        self.carry_v -= v;
        self.carry_h -= h;
        if v != 0.0 {
            self.wheel(MOUSEEVENTF_WHEEL, v as i32);
        }
        if h != 0.0 {
            self.wheel(MOUSEEVENTF_HWHEEL, h as i32);
        }
        self.repair();
    }

    fn reset(&mut self) {
        self.carry_v = 0.0;
        self.carry_h = 0.0;
    }
}

impl Drop for Wheel {
    /// Release every key this engine pressed again, whether or not the
    /// hook says the user still holds it. The hook's view can be stale:
    /// Windows skips (and may silently remove) a low-level hook that
    /// answers too slowly, so a real release can go unseen and the key
    /// would stay down after scroll mode ends. A stray key-up for a key
    /// still physically held costs little; a stuck Shift or Ctrl breaks
    /// every app. Runs on a normal exit and on a panic in the thread.
    fn drop(&mut self) {
        if self.repressed == 0 {
            return;
        }
        let ups: Vec<INPUT> = SIDES
            .iter()
            .filter(|(bit, ..)| self.repressed & bit != 0)
            .map(|&(_, vk, ext)| tagged_key(vk, ext, true))
            .collect();
        send(&ups);
        self.repressed = 0;
    }
}

fn run(cfg: MotionConfig, rx: Receiver<Cmd>, real: RealMods) {
    let uia = UIAutomation::new().ok();
    let epoch = Instant::now();
    let now = || epoch.elapsed().as_secs_f64() * 1000.0;
    let mut motion = Motion::new(cfg);
    let mut wheel_out = Wheel::new(real);
    loop {
        // Keep ticking while a re-pressed key may need releasing.
        let cmd = if motion.is_idle() && wheel_out.repressed == 0 {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break,
            }
        } else {
            match rx.recv_timeout(TICK) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        match cmd {
            Some(Cmd::Key { dir, down, fast }) => {
                motion.set_fast(fast);
                if down {
                    motion.press(dir, now());
                } else {
                    motion.release(dir);
                }
            }
            Some(Cmd::Fast(f)) => motion.set_fast(f),
            Some(Cmd::Config(c)) => motion.set_config(c),
            Some(Cmd::MoveTo(x, y)) => {
                // Flush what's moving into the old area first.
                let (dx, dy) = motion.advance(now());
                wheel_out.send(dx, dy);
                unsafe {
                    let _ = SetCursorPos(x, y);
                }
            }
            Some(Cmd::Stop) => {
                motion.stop();
                wheel_out.reset();
            }
            Some(Cmd::Jump(jump, area)) => {
                let done = uia.as_ref().is_some_and(|u| pattern_jump(u, jump, &area));
                if !done {
                    let (dir, lines) = wheel_jump(jump, &area);
                    motion.impulse(dir, lines, now());
                }
            }
            None => {}
        }
        let (dx, dy) = motion.advance(now());
        wheel_out.send(dx, dy);
    }
    // Session over: dropping `wheel_out` releases every key it pressed
    // again (see `impl Drop for Wheel`).
}

/// Wheel fallback for a jump: direction and lines.
fn wheel_jump(jump: Jump, area: &Area) -> (Dir, f64) {
    let vertical = area.can_v || !area.can_h;
    let size = if vertical {
        area.bounds.height
    } else {
        area.bounds.width
    } as f64;
    let half = (size / 2.0 / LINE_PX_ESTIMATE).max(3.0);
    let (back, fwd) = if vertical {
        (Dir::Up, Dir::Down)
    } else {
        (Dir::Left, Dir::Right)
    };
    match jump {
        Jump::Top => (back, FAR_LINES),
        Jump::Bottom => (fwd, FAR_LINES),
        Jump::HalfUp => (back, half),
        Jump::HalfDown => (fwd, half),
    }
}

/// Jump through the area's ScrollPattern. `false` if it has none or the
/// call failed (the caller falls back to the wheel).
fn pattern_jump(uia: &UIAutomation, jump: Jump, area: &Area) -> bool {
    let (cx, cy) = area.bounds.center();
    let Some(el) = scrollable_at(uia, cx, cy) else {
        return false;
    };
    let Ok(p) = el.get_pattern::<UIScrollPattern>() else {
        return false;
    };
    // UIA's "don't change this axis" value.
    const NO_SCROLL: f64 = -1.0;
    let vertical = area.can_v || !area.can_h;
    let set = |pct: f64| {
        let pct = pct.clamp(0.0, 100.0);
        if vertical {
            p.set_scroll_percent(NO_SCROLL, pct)
        } else {
            p.set_scroll_percent(pct, NO_SCROLL)
        }
        .is_ok()
    };
    match jump {
        Jump::Top => set(0.0),
        Jump::Bottom => set(100.0),
        Jump::HalfUp | Jump::HalfDown => {
            let (cur, view) = if vertical {
                (p.get_vertical_scroll_percent(), p.get_vertical_view_size())
            } else {
                (
                    p.get_horizontal_scroll_percent(),
                    p.get_horizontal_view_size(),
                )
            };
            let (Ok(cur), Ok(view)) = (cur, view) else {
                return false;
            };
            let usable = (0.0..=100.0).contains(&cur) && view > 0.0 && view < 100.0;
            if !usable {
                return false;
            }
            // View size is the visible share of the content (%); half of
            // it as a scroll percent is half a page.
            let content_share = 100.0 - view;
            let delta = (view / 2.0) / content_share * 100.0;
            let target = if jump == Jump::HalfUp {
                cur - delta
            } else {
                cur + delta
            };
            for i in 1..=HALF_PAGE_STEPS {
                let t = i as f64 / HALF_PAGE_STEPS as f64;
                let eased = t * t * (3.0 - 2.0 * t);
                if !set(cur + (target - cur) * eased) {
                    return i > 1;
                }
                std::thread::sleep(HALF_PAGE_STEP);
            }
            true
        }
    }
}
