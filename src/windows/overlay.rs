//! Win32-native hint overlay.
//!
//! [`pick_hint`] creates a transparent, always-on-top, layered popup window
//! that covers the virtual desktop, draws a styled label per hint, and runs
//! its own message loop until the user types a complete label or presses
//! `Esc` (or focus is stolen).
//!
//! The function is generic over what the hints represent: callers supply a
//! `Vec<Hint>` (each with a screen-space rect, a label, and optional
//! "extra" disambiguation text) plus a [`HintStyle`] preset, and get back
//! the *index* of the chosen hint. The caller maps that index back to
//! whatever it cares about — an [`crate::ElementId`] for the element
//! picker, an HWND for the window picker, etc. This keeps the rendering
//! primitive completely free of knowledge about the domain it's pointing
//! at.
//!
//! Layout pass: the renderer sizes each badge up-front from the measured
//! advance of the (monospaced) label font, with no measure-then-place
//! dance, and runs a small collision-resolution pass that stacks
//! colliding hints vertically. This is what keeps two maximized windows
//! on the same monitor (e.g. Edge + Steam) from drawing their badges on
//! top of each other.
//!
//! Transparency is implemented with the modern, fast path:
//! `UpdateLayeredWindow(ULW_ALPHA)` driven by a pre-composed 32-bit ARGB
//! DIB. Pixels with `alpha == 0` are fully transparent (no color-key
//! tax, no per-paint magenta fill); badge pixels carry per-pixel alpha
//! that DWM blends directly. Opacity is baked into those pixels: badge
//! fills, borders and leaders are translucent, while label glyphs
//! (rasterised once per pick by [`super::label_text`]) stay opaque.
//! The HWND, off-screen DIB, and memory DC are all created once on the
//! first hotkey press and reused for the life of the process —
//! subsequent picks just re-render and re-show.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;

use anyhow::{anyhow, Context, Result};
use smallvec::SmallVec;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromRect, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_ESCAPE, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassInfoExW,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, LoadCursorW, PostMessageW, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW, ShowWindow,
    TranslateMessage, UnhookWindowsHookEx, UpdateLayeredWindow, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HHOOK, IDC_ARROW, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WH_MOUSE_LL, WM_KEYDOWN, WM_KILLFOCUS, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::label_text::{Coverage, Font, LabelText, Role, TextItem};
use crate::Bounds;

const CLASS_NAME: PCWSTR = w!("JumpkeyOverlayClass");
const WINDOW_TITLE: PCWSTR = w!("Jumpkey Overlay");

/// Pixel gap between the matchable badge and the optional "extra" pill,
/// and the vertical gap between stacked badges after collision resolution.
const PILL_GAP: i32 = 4;

/// Hard cap on how many characters we lay out for the extra text. Past
/// this, the title is trimmed with an ellipsis. Keeps very long window
/// titles from making one badge wider than a monitor.
const MAX_EXTRA_CHARS: usize = 50;

/// Custom message posted by [`scroll_hook_proc`] to dismiss the overlay
/// when a mouse-wheel scroll is detected. `0x8000` is `WM_APP`, the
/// start of the private application message range.
const WM_SCROLL_DISMISS: u32 = 0x8000;

/// `WM_MOUSEHWHEEL` (horizontal mouse wheel). Not always re-exported by
/// the `windows` crate, so we define it from the SDK value.
const WM_MOUSEHWHEEL_RAW: u32 = 0x020E;

/// One pickable thing on screen. The renderer treats `bounds` as a
/// screen-space anchor and places the label badge at the top-left of that
/// rect (subject to collision-resolution shifting).
#[derive(Debug, Clone)]
pub struct Hint {
    /// Anchor rectangle in screen coordinates (the badge appears at its
    /// top-left). For elements this is the element's bounding rect; for
    /// windows it's the window's frame rect.
    pub bounds: Bounds,
    /// Lowercase home-row label produced by [`crate::HintEngine`]. This is
    /// what the user types to select.
    pub label: String,
    /// Optional non-matchable text drawn in a secondary pill beside the
    /// badge. Used by the window picker to show window titles so the user
    /// can disambiguate two windows whose badges would otherwise be at
    /// the same pixel.
    pub extra: Option<String>,
}

/// Modifier key held at the moment the user pressed the final
/// hint-completing letter. Routed to the caller so it can pick the
/// right [`crate::Action`] variant (Invoke / RightClick / DoubleClick)
/// without the overlay itself needing to know what each modifier maps
/// to.
///
/// Only one modifier is reported per pick — Ctrl wins over Shift if
/// both happen to be held when the matching key fires, which keeps the
/// caller's match arm trivial and matches the typical "I held Ctrl
/// because I wanted a double-click" intent. Modifier state is sampled
/// *only* on the keystroke that completes the label; intermediate
/// prefix characters don't have to be modified, so the user can press
/// Ctrl just before the last letter and still get a double-click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickModifier {
    /// No modifier was held when the hint completed — perform the
    /// element's primary action via UIA invoke / mouse fallback.
    None,
    /// Control was held — caller should synthesize a double-click.
    Ctrl,
    /// Shift was held — caller should synthesize a right-click (context
    /// menu).
    Shift,
}

/// What [`pick_hint`] returns on a successful pick: which hint was
/// chosen, plus the modifier (if any) the user held to influence what
/// happens next. Callers that don't care about modifiers can ignore
/// [`PickResult::modifier`] entirely.
#[derive(Debug, Clone, Copy)]
pub struct PickResult {
    /// Index into the input `hints` vec the user picked.
    pub index: usize,
    /// Modifier key held at the moment the pick completed. See
    /// [`PickModifier`] for the precedence rule when multiple
    /// modifiers are held.
    pub modifier: PickModifier,
}

/// Outline of a hint badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeShape {
    /// Flat rectangle anchored in a corner of the element (the classic
    /// vimium look, and the window picker's badge + title pill).
    Rect,
    /// Rounded label just below the element (or above / beside it when
    /// that collides) with a small tail pointing at the element.
    Callout,
}

impl BadgeShape {
    /// Parse the config spelling (`"rect"` / `"callout"`, any case).
    /// Empty or unknown → `None` (caller keeps the preset default).
    pub fn from_config(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rect" => Some(Self::Rect),
            "callout" => Some(Self::Callout),
            _ => None,
        }
    }
}

/// Visual presets for the overlay. The element and window pickers use
/// different sizing/colors so the user can tell at a glance which mode
/// they're in.
#[derive(Debug, Clone, Copy)]
pub struct HintStyle {
    /// Pixel height of the bold label font.
    pub font_height: i32,
    /// Background fill of the matchable badge.
    pub badge_bg: COLORREF,
    /// Text color for the matchable badge.
    pub badge_fg: COLORREF,
    /// Border color for both pills.
    pub border: COLORREF,
    /// Background fill of the optional extra/title pill. Ignored when no
    /// hint has an `extra`.
    pub extra_bg: COLORREF,
    /// Text color for the optional extra/title pill.
    pub extra_fg: COLORREF,
    /// Horizontal padding inside each pill.
    pub padding_x: i32,
    /// Vertical padding inside each pill.
    pub padding_y: i32,
    /// Window-wide opacity for badges (0..=255). 255 = fully opaque,
    /// values around 200-230 let the underlying UI bleed through enough
    /// to see what the badge is sitting on. The transparent color-key
    /// pixels remain fully see-through regardless of this value.
    pub badge_opacity: u8,
    /// Draw a 1px text shadow behind labels for extra contrast on
    /// busy/light backgrounds. Off by default — most styles already
    /// have enough contrast between badge_bg and badge_fg.
    pub text_shadow: bool,
    /// Draw a thin connector line + arrowhead from each badge to the
    /// element it represents. Helps disambiguate badges that are placed
    /// near (but not on) their target — e.g. when collision resolution
    /// pushed the badge into [`BadgePosition::OutsideTop`] above the
    /// element. We always render the arrow when the geometry is long
    /// enough to be readable; degenerate cases (badge fully overlapping
    /// the element) skip it automatically.
    pub show_leader: bool,
    /// Pen color for the leader line + arrowhead. Independent of
    /// `badge_bg` so the leader can stay legible on backgrounds that
    /// would otherwise wash out a translucent badge.
    pub leader_color: COLORREF,
    /// When `true`, the layout pass tries to anchor badges *inside* their
    /// element (top-left first). Use this for the window picker, where
    /// targets are typically full-screen windows and an "above the
    /// element" badge would render above the monitor and be invisible.
    ///
    /// When `false` (default), `OutsideTop` is tried first so that small
    /// element badges sit just above the control they label, leaving the
    /// underlying UI fully visible.
    pub prefer_inside_anchor: bool,
    /// When `true`, `run_pick` scales `font_height` and padding by the
    /// target monitor's DPI factor. Set automatically when the user has
    /// not specified an explicit `font_size` in config (i.e. font_size=0).
    /// When `false`, the `font_height` is used as-is (user chose a
    /// concrete size).
    pub auto_scale_dpi: bool,
    /// Badge outline. See [`BadgeShape`].
    pub shape: BadgeShape,
}

impl HintStyle {
    /// Compact "vimium-style" corner badges meant to sit *inside* the
    /// top-left corner of small UI controls without obscuring the rest
    /// of the control. Tuned for dense screens (multi-monitor + busy
    /// app like Cursor / VS Code) where the previous "above the
    /// control" placement produced a wall of overlapping badges.
    ///
    /// Three deliberate departures from the v0.4.0 preset:
    ///
    /// 1. **Smaller font and padding** — the badge has to fit inside
    ///    the click target's corner, so it must shrink to ~13px and use
    ///    1-2px of padding.
    /// 2. **Lower opacity (~76%)** — the user needs to read the
    ///    underlying icon/label *through* the badge before committing.
    ///    230/255 was almost fully opaque; 195 keeps text legible while
    ///    the control's own glyph still bleeds through.
    /// 3. **Inside-anchor preferred** — `TopLeft` is tried first so the
    ///    badge sits in the corner of its element. The leader-line
    ///    renderer auto-skips the connector when the badge is inside
    ///    the target, so the visual is just `[a]` in the corner with
    ///    the element outlined.
    pub fn elements() -> Self {
        Self {
            font_height: 13,
            badge_bg: COLORREF(0x0000E5FF), // BGR yellow
            badge_fg: COLORREF(0x00000000),
            border: COLORREF(0x00202020),
            extra_bg: COLORREF(0x00202020),
            extra_fg: COLORREF(0x00FFFFFF),
            padding_x: 3,
            padding_y: 1,
            // ~76% opaque: small enough to see through, large enough to
            // stay readable on busy backgrounds. Trades a little
            // legibility for a dramatically calmer overlay.
            badge_opacity: 195,
            // Off: the 1px offset copy smeared the glyphs and made the
            // labels look soft. `examples/label_compare.rs` renders the
            // side-by-side comparison this was decided from.
            text_shadow: false,
            show_leader: true,
            // Same dark grey as the badge border — visually subordinate to
            // the badge itself but contrasts well against typical app UIs.
            leader_color: COLORREF(0x00202020),
            // Anchor inside the top-left corner of the control. Combined
            // with the smaller font this leaves the rest of the control
            // visible so the user can confirm what they're about to click.
            prefer_inside_anchor: true,
            auto_scale_dpi: true,
            shape: BadgeShape::Callout,
        }
    }

    /// Larger badges with a distinct accent color and a dark "title pill"
    /// for the window name. Used by the window picker so the user
    /// instantly sees what each badge maps to even when two windows are
    /// maximized on the same monitor.
    pub fn windows() -> Self {
        Self {
            font_height: 30,
            badge_bg: COLORREF(0x00FFAA33), // BGR teal-blue accent
            badge_fg: COLORREF(0x00FFFFFF),
            border: COLORREF(0x00101010),
            extra_bg: COLORREF(0x00202020), // dark gray
            extra_fg: COLORREF(0x00FFFFFF),
            padding_x: 12,
            padding_y: 6,
            // Slightly more opaque than the element picker — the window
            // badges are large and we'd rather lose a hair of see-through
            // than risk the title pill text smudging into a desktop image.
            badge_opacity: 240,
            text_shadow: false,
            // Window badges already include a title pill spelling out
            // exactly which window each one maps to, so the leader line
            // is just visual noise here — turn it off by default.
            show_leader: false,
            leader_color: COLORREF(0x00101010),
            // Window targets are typically maximized — anchoring the
            // badge to the top-left of the window puts it inside the
            // title bar area where it's clearly visible. `OutsideTop`
            // would push it above the monitor for any window at y == 0.
            prefer_inside_anchor: true,
            auto_scale_dpi: true,
            shape: BadgeShape::Rect,
        }
    }

    /// Scale font and padding by a monitor DPI factor (1.0 = 96 DPI).
    /// Factors at or below 1.0 leave the style unchanged.
    pub fn scaled_for_dpi(mut self, scale: f32) -> Self {
        if scale > 1.0 {
            self.font_height = ((self.font_height as f32) * scale).round() as i32;
            self.padding_x = ((self.padding_x as f32) * scale).ceil() as i32;
            self.padding_y = ((self.padding_y as f32) * scale).ceil() as i32;
        }
        self
    }

    /// Build the element-picker style, overriding any non-empty colors
    /// from the user config. Empty strings keep the hardcoded defaults so
    /// users can override one swatch at a time without filling out every
    /// field in `config.toml`.
    pub fn elements_from_config(c: &crate::config::BadgeColors) -> Self {
        let mut style = Self::elements();
        apply_color_override(&mut style.badge_bg, &c.badge_bg);
        apply_color_override(&mut style.badge_fg, &c.badge_fg);
        apply_color_override(&mut style.border, &c.border);
        apply_color_override(&mut style.leader_color, &c.leader_color);
        apply_opacity_override(&mut style.badge_opacity, c.opacity);
        if let Some(show) = c.show_leader {
            style.show_leader = show;
        }
        if c.font_size > 0 {
            style.font_height = c.font_size as i32;
            style.auto_scale_dpi = false;
        }
        apply_shape_override(&mut style.shape, &c.shape);
        style
    }

    /// Like [`Self::elements_from_config`] but for the window picker.
    pub fn windows_from_config(c: &crate::config::BadgeColors) -> Self {
        let mut style = Self::windows();
        apply_color_override(&mut style.badge_bg, &c.badge_bg);
        apply_color_override(&mut style.badge_fg, &c.badge_fg);
        apply_color_override(&mut style.border, &c.border);
        apply_color_override(&mut style.leader_color, &c.leader_color);
        apply_opacity_override(&mut style.badge_opacity, c.opacity);
        if let Some(show) = c.show_leader {
            style.show_leader = show;
        }
        if c.font_size > 0 {
            style.font_height = c.font_size as i32;
            style.auto_scale_dpi = false;
        }
        apply_shape_override(&mut style.shape, &c.shape);
        style
    }
}

/// Convert a 0..=100 percent value from config into a 0..=255 alpha byte.
/// `0` is treated as "use the preset default" so users who never touch the
/// field don't end up with invisible badges.
fn apply_opacity_override(target: &mut u8, percent: u8) {
    if percent == 0 {
        return;
    }
    let clamped = percent.min(100) as u32;
    *target = (clamped * 255 / 100) as u8;
}

fn apply_shape_override(target: &mut BadgeShape, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    match BadgeShape::from_config(value) {
        Some(shape) => *target = shape,
        None => tracing::warn!(value = %value, "unknown badge shape in config; keeping default"),
    }
}

fn apply_color_override(target: &mut COLORREF, hex: &str) {
    if hex.trim().is_empty() {
        return;
    }
    match parse_hex_color(hex) {
        Ok(c) => *target = c,
        Err(e) => {
            tracing::warn!(value = %hex, error = ?e, "invalid color in config; keeping default");
        }
    }
}

/// Parse `"#RRGGBB"` or `"#RGB"` (with or without the `#`) into a
/// Win32 [`COLORREF`]. Win32 stores colors as `0x00BBGGRR`, so we swap
/// the byte order from web-style RGB.
pub fn parse_hex_color(s: &str) -> anyhow::Result<COLORREF> {
    let trimmed = s.trim().trim_start_matches('#');
    let (r, g, b) = match trimmed.len() {
        6 => {
            let r = u8::from_str_radix(&trimmed[0..2], 16)?;
            let g = u8::from_str_radix(&trimmed[2..4], 16)?;
            let b = u8::from_str_radix(&trimmed[4..6], 16)?;
            (r, g, b)
        }
        3 => {
            let r = u8::from_str_radix(&trimmed[0..1], 16)? * 0x11;
            let g = u8::from_str_radix(&trimmed[1..2], 16)? * 0x11;
            let b = u8::from_str_radix(&trimmed[2..3], 16)? * 0x11;
            (r, g, b)
        }
        _ => anyhow::bail!("hex color must be #RGB or #RRGGBB, got '{s}'"),
    };
    let bgr = ((b as u32) << 16) | ((g as u32) << 8) | (r as u32);
    Ok(COLORREF(bgr))
}

/// One hint after layout resolution, in client coordinates. We compute
/// these once when the overlay is constructed; subsequent paints just
/// blit them.
struct LaidHint {
    label: String,
    extra: Option<String>,
    badge_rect: RECT,
    extra_rect: Option<RECT>,
    /// Element bounds in client coordinates. Kept around so the painter
    /// can draw a leader line / arrowhead from the badge to the actual
    /// click target — useful when smart positioning shoved the badge off
    /// the element to avoid a collision.
    target_rect: RECT,
    /// Drawn as a callout sprite (rounded body, with or without a tail).
    /// Decided once in [`lay_out`]: callout shape and no extra pill.
    callout: bool,
    /// Callout tail pointing at the target. `Some` only when a callout
    /// position won layout; `badge_rect` is then the callout body.
    tail: Option<Tail>,
}

/// Direction a callout's tail points, i.e. which body edge it sits on.
/// A callout *below* its element has an `Up` tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TailDir {
    Up,
    Down,
    Left,
    Right,
}

/// Callout tail: direction plus where the tip sits along its edge,
/// measured from `badge_rect.left` (Up/Down) or `badge_rect.top`
/// (Left/Right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Tail {
    dir: TailDir,
    offset: i32,
}

/// Callout dimensions derived from the (already DPI-scaled) font size.
#[derive(Debug, Clone, Copy)]
struct CalloutGeom {
    /// Tail length, from body edge to tip.
    tail_h: i32,
    /// Half the tail's base width.
    tail_half_w: i32,
    /// Body corner radius.
    radius: i32,
    /// Body height.
    body_h: i32,
    /// Horizontal padding between body edge and label text. Never less
    /// than `radius`, so text stays clear of the rounded corners.
    pad_x: i32,
    /// How far the tail tip reaches *into* the element past its edge,
    /// so the callout visibly overlaps what it labels. Capped per
    /// element at a third of its size (see [`callout_candidates`]).
    cut_in: i32,
}

impl CalloutGeom {
    /// Closest the tail's centre may sit to a body corner without its
    /// base running into the rounded corner.
    fn inner(&self) -> i32 {
        self.radius + self.tail_half_w
    }

    /// Body width for a label `label_w` pixels wide. At least wide
    /// enough to fit a centred tail clear of both corners.
    fn body_width(&self, label_w: i32) -> i32 {
        (label_w + 2 * self.pad_x).max(2 * self.inner() + 2)
    }
}

impl HintStyle {
    /// Callout geometry when this style draws callouts, else `None`.
    fn callout_geom_if_enabled(&self) -> Option<CalloutGeom> {
        (self.shape == BadgeShape::Callout).then(|| self.callout_geom())
    }

    fn callout_geom(&self) -> CalloutGeom {
        let f = self.font_height as f32;
        let radius = ((f * 0.3).round() as i32).max(2);
        let body_h = self.font_height + 2 * self.padding_y.max(2);
        let tail_h = ((f * 0.45).round() as i32).max(4);
        // Cap the base so a sideways tail still fits between the
        // corners of the (short) body.
        let tail_half_w = (((tail_h as f32) * 0.85).round() as i32)
            .min((body_h - 2 * radius) / 2)
            .max(2);
        CalloutGeom {
            tail_h,
            tail_half_w,
            radius,
            body_h,
            pad_x: self.padding_x.max(radius),
            cut_in: (tail_h + 1) / 2,
        }
    }
}

/// Candidate callout placements around `target`, in preference order:
/// below centred, above centred, below slid left/right, above slid
/// left/right, then to the right and left of the element. Each entry is
/// `(body, tail, bbox)` where `bbox` is the body plus its tail, the
/// area that must be free of other badges.
///
/// Up/down tails aim at the target's horizontal centre, sideways tails
/// at its vertical centre. The tip lands `cut_in` pixels inside the
/// element's near edge (at most a third of the element's height or
/// width, so tiny controls aren't covered). Sliding moves the
/// body so the tip sits at the body's inner left or right edge, which
/// lets a callout dodge a neighbour while still pointing at its element.
fn callout_candidates(
    target: &RECT,
    body_w: i32,
    g: CalloutGeom,
) -> SmallVec<[(RECT, Tail, RECT); 8]> {
    let cx = (target.left + target.right) / 2;
    let cy = (target.top + target.bottom) / 2;
    let inner = g.inner();
    let cut_v = g.cut_in.min((target.bottom - target.top) / 3).max(0);
    let cut_h = g.cut_in.min((target.right - target.left) / 3).max(0);
    let mut out = SmallVec::new();

    let vertical = |dir: TailDir, offset: i32| {
        let left = cx - offset;
        let (top, bbox_top, bbox_bottom) = match dir {
            TailDir::Up => {
                let tip_y = target.bottom - cut_v;
                let top = tip_y + g.tail_h;
                (top, tip_y, top + g.body_h)
            }
            _ => {
                let tip_y = target.top + cut_v;
                let top = tip_y - g.tail_h - g.body_h;
                (top, top, tip_y)
            }
        };
        let body = RECT {
            left,
            top,
            right: left + body_w,
            bottom: top + g.body_h,
        };
        let bbox = RECT {
            top: bbox_top,
            bottom: bbox_bottom,
            ..body
        };
        (body, Tail { dir, offset }, bbox)
    };
    let centre = body_w / 2;
    out.push(vertical(TailDir::Up, centre));
    out.push(vertical(TailDir::Down, centre));
    for dir in [TailDir::Up, TailDir::Down] {
        out.push(vertical(dir, inner));
        out.push(vertical(dir, body_w - inner));
    }

    let top = cy - g.body_h / 2;
    let offset = cy - top;
    // Right of the element, tail pointing left at it.
    let tip_x = target.right - cut_h;
    let left = tip_x + g.tail_h;
    let body = RECT {
        left,
        top,
        right: left + body_w,
        bottom: top + g.body_h,
    };
    out.push((
        body,
        Tail {
            dir: TailDir::Left,
            offset,
        },
        RECT {
            left: tip_x,
            ..body
        },
    ));
    // Left of the element, tail pointing right at it.
    let tip_x = target.left + cut_h;
    let right = tip_x - g.tail_h;
    let body = RECT {
        left: right - body_w,
        top,
        right,
        bottom: top + g.body_h,
    };
    out.push((
        body,
        Tail {
            dir: TailDir::Right,
            offset,
        },
        RECT {
            right: tip_x,
            ..body
        },
    ));
    out
}

struct OverlayState {
    laid: Vec<LaidHint>,
    style: HintStyle,
    typed: String,
    /// Index into `laid` of the chosen entry, set by `key_down` on a full
    /// label match. `None` means "user cancelled or window closed."
    selected: Option<usize>,
    /// Modifier key held at the moment `selected` was set. Sampled in
    /// `key_down` on the completing keystroke so the caller (see
    /// `PickResult`) can decide between Invoke / DoubleClick / RightClick.
    /// Meaningful only when `selected` is `Some`.
    modifier: PickModifier,
    /// Set to `true` when the modal pump should exit (selection made,
    /// Esc pressed, or focus lost). Replaces the old DestroyWindow +
    /// PostQuitMessage flow now that the HWND is persistent.
    done: bool,
    /// Rasterised label (and extra-pill title) text, parallel to `laid`.
    /// Built once at pick start; frames just composite the masks.
    text: Vec<HintText>,
    /// Callout dimensions, `Some` when `style.shape` is `Callout`.
    callout_geom: Option<CalloutGeom>,
    /// Pre-rendered callout bodies (shape + gradient + border + shadow),
    /// one per distinct size/tail combination in `laid`. Built once at
    /// pick start so keystroke re-renders only blit.
    sprites: HashMap<SpriteKey, Sprite>,
}

/// Glyph coverage for one hint, with where each mask goes in the DIB.
#[derive(Default)]
struct HintText {
    label: Coverage,
    label_at: (i32, i32),
    extra: Option<(Coverage, (i32, i32))>,
}

/// Shrink `r` by `dx` on the left/right and `dy` on the top/bottom.
fn inset(r: &RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right - dx,
        bottom: r.bottom - dy,
    }
}

impl OverlayState {
    unsafe fn new(
        hints: Vec<Hint>,
        style: HintStyle,
        origin_x: i32,
        origin_y: i32,
        text: &mut LabelText,
    ) -> Self {
        let metrics = TextMetrics::measure(&style, text);
        let laid = lay_out(&hints, &style, metrics, origin_x, origin_y);
        let callout_geom = style.callout_geom_if_enabled();
        let text = Self::rasterize_text(&laid, &style, callout_geom, text);
        let mut sprites = HashMap::new();
        if let Some(g) = callout_geom {
            for h in laid.iter().filter(|h| h.callout) {
                sprites.entry(SpriteKey::for_hint(h)).or_insert_with(|| {
                    build_callout_sprite(
                        h.badge_rect.right - h.badge_rect.left,
                        h.badge_rect.bottom - h.badge_rect.top,
                        g,
                        h.tail,
                        style.badge_bg,
                        style.border,
                        style.badge_opacity,
                    )
                });
            }
        }
        Self {
            laid,
            style,
            typed: String::new(),
            selected: None,
            modifier: PickModifier::None,
            done: false,
            text,
            callout_geom,
            sprites,
        }
    }

    /// Box the label text is centred in: inside the padding for rect
    /// badges, clear of the rounded corners and border for callouts.
    fn label_rect(style: &HintStyle, geom: Option<CalloutGeom>, laid: &LaidHint) -> RECT {
        match (geom, laid.callout) {
            (Some(g), true) => inset(&laid.badge_rect, g.radius, 1),
            _ => inset(&laid.badge_rect, style.padding_x, style.padding_y),
        }
    }

    /// Rasterise every hint's text in one pass.
    fn rasterize_text(
        laid: &[LaidHint],
        style: &HintStyle,
        geom: Option<CalloutGeom>,
        text: &mut LabelText,
    ) -> Vec<HintText> {
        let mut items = Vec::with_capacity(laid.len());
        let mut at = Vec::with_capacity(laid.len());
        fn item<'a>(text: &'a str, font: Font, r: &RECT) -> TextItem<'a> {
            TextItem {
                text,
                font,
                w: r.right - r.left,
                h: r.bottom - r.top,
            }
        }
        for h in laid {
            let r = Self::label_rect(style, geom, h);
            items.push(item(&h.label, label_font(style), &r));
            at.push((r.left, r.top));
            if let (Some(er), Some(extra)) = (&h.extra_rect, &h.extra) {
                let r = inset(er, style.padding_x, style.padding_y);
                items.push(item(extra, extra_font(style), &r));
                at.push((r.left, r.top));
            }
        }
        let mut masks = text.rasterize(&items).into_iter().zip(at);
        laid.iter()
            .map(|h| {
                let (label, label_at) = masks.next().expect("one mask per label");
                let extra = h
                    .extra_rect
                    .and(h.extra.as_ref())
                    .map(|_| masks.next().expect("one mask per title"));
                HintText {
                    label,
                    label_at,
                    extra,
                }
            })
            .collect()
    }

    /// Composite a text mask in `color`, with the optional 1px shadow.
    unsafe fn draw_text_mask(
        &self,
        bits: *mut u8,
        dib_w: i32,
        dib_h: i32,
        cov: &Coverage,
        (x, y): (i32, i32),
        color: COLORREF,
    ) {
        if self.style.text_shadow {
            // Dark grey rather than black so it reads as depth, not as a
            // second character.
            blit_coverage(bits, dib_w, dib_h, cov, x + 1, y + 1, COLORREF(0x00404040));
        }
        blit_coverage(bits, dib_w, dib_h, cov, x, y, color);
    }

    /// Render the current state into the supplied off-screen ARGB DIB,
    /// then atomically push the result to DWM via UpdateLayeredWindow.
    /// No WM_PAINT round-trip; no magenta color-key tax.
    unsafe fn render_to_dib(
        &self,
        hwnd: HWND,
        mem_dc: HDC,
        bits: *mut u8,
        width: i32,
        height: i32,
    ) {
        // Reset the DIB to fully transparent. memset on a contiguous
        // BGRA buffer is cheaper than per-pixel iteration and matches
        // what the kernel does internally for fresh DIB sections.
        if !bits.is_null() && width > 0 && height > 0 {
            std::ptr::write_bytes(bits, 0, (width as usize) * (height as usize) * 4);
        }

        self.draw_all(bits, width, height);

        // Hand the finished frame to DWM. Opacity is already baked into
        // each pixel (badge fills, borders and leaders are translucent,
        // glyphs are not), so the window itself is composited unfaded.
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = windows::Win32::Foundation::SIZE {
            cx: width,
            cy: height,
        };
        let src_pt = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            None,
            Some(&size as *const _),
            mem_dc,
            Some(&src_pt as *const _),
            COLORREF(0),
            Some(&blend as *const _),
            ULW_ALPHA,
        );
    }

    /// Draw every visible hint into an already-cleared DIB. Split out of
    /// [`Self::render_to_dib`] so the benchmarks can time our drawing
    /// without the full-surface clear or the DWM hand-off.
    unsafe fn draw_all(&self, bits: *mut u8, width: i32, height: i32) {
        for (laid, text) in self.laid.iter().zip(&self.text) {
            self.draw_hint(bits, width, height, laid, text);
        }
    }

    unsafe fn draw_hint(
        &self,
        bits: *mut u8,
        dib_w: i32,
        dib_h: i32,
        laid: &LaidHint,
        text: &HintText,
    ) {
        // Hide hints whose label doesn't match the typed prefix.
        if !self.typed.is_empty() && !laid.label.starts_with(&self.typed) {
            return;
        }

        // -- leader outline + connector arrow (drawn first so the badge
        //    paints on top) --
        //
        // A callout's tail already points at its element, so the outline
        // and connector are only for tail-less badges.
        if self.style.show_leader && laid.tail.is_none() {
            self.draw_leader_dib(bits, dib_w, dib_h, laid);
        }

        if let (Some(_), true) = (self.callout_geom, laid.callout) {
            if let Some(sprite) = self.sprites.get(&SpriteKey::for_hint(laid)) {
                let body = &laid.badge_rect;
                blit_sprite(
                    bits,
                    dib_w,
                    dib_h,
                    sprite,
                    body.left - sprite.body_x,
                    body.top - sprite.body_y,
                );
                self.draw_text_mask(
                    bits,
                    dib_w,
                    dib_h,
                    &text.label,
                    text.label_at,
                    self.style.badge_fg,
                );
                return;
            }
        }

        // -- matchable badge --
        //
        // Fill and border carry the badge opacity; the label goes on top
        // at full opacity so it stays crisp on a see-through badge.
        let a = self.style.badge_opacity;
        fill_rect_argb(
            bits,
            dib_w,
            dib_h,
            &laid.badge_rect,
            premul(self.style.badge_bg, a),
        );
        frame_rect_argb(
            bits,
            dib_w,
            dib_h,
            &laid.badge_rect,
            premul(self.style.border, a),
        );
        self.draw_text_mask(
            bits,
            dib_w,
            dib_h,
            &text.label,
            text.label_at,
            self.style.badge_fg,
        );

        // -- optional extra pill --
        if let (Some(extra_rect), Some((cov, at))) = (&laid.extra_rect, &text.extra) {
            fill_rect_argb(
                bits,
                dib_w,
                dib_h,
                extra_rect,
                premul(self.style.extra_bg, a),
            );
            frame_rect_argb(bits, dib_w, dib_h, extra_rect, premul(self.style.border, a));
            self.draw_text_mask(bits, dib_w, dib_h, cov, *at, self.style.extra_fg);
        }
    }

    /// Visually associate the badge with its click target.
    ///
    /// Two complementary signals — both gated on
    /// [`HintStyle::show_leader`]:
    ///
    /// 1. **Target outline.** A 1px frame around the element in the
    ///    leader color. Always drawn (when the feature is on), so the
    ///    user can see at a glance which underlying control each badge
    ///    represents — even when the badge sits in the corner of a
    ///    large button and a connector line would degenerate to a stub.
    /// 2. **Connector arrow.** A line + filled triangular arrowhead
    ///    from the badge to the element. Only drawn when the badge is
    ///    visibly offset from the element (smart positioning kicked
    ///    the badge off the control to dodge a collision); for badges
    ///    that already sit inside the element the outline alone is the
    ///    cleaner visual.
    unsafe fn draw_leader_dib(&self, bits: *mut u8, dib_w: i32, dib_h: i32, laid: &LaidHint) {
        let badge = &laid.badge_rect;
        let target = &laid.target_rect;

        // Outline the target rect in the badge background color so the
        // badge↔target association is immediate (yellow badge = yellow
        // box around its element). Drawn directly into the DIB so it's
        // at the badge opacity, like the badge itself.
        // Callouts never outline their target: they point with their
        // tail, and a tail-less fallback callout gets just the connector
        // below. Rect badges (incl. extra-pill hints in callout mode) do.
        if !laid.callout {
            let c = premul(self.style.badge_bg, self.style.badge_opacity);
            frame_rect_argb(bits, dib_w, dib_h, target, c);
        }

        // Connector arrow only when the badge is meaningfully detached
        // from its target. When the badge sits inside the target the
        // outline already does the job and a tiny in-element arrow
        // just adds noise.
        let badge_inside_target = badge.left >= target.left
            && badge.right <= target.right
            && badge.top >= target.top
            && badge.bottom <= target.bottom;
        if !badge_inside_target {
            let (start, end) = leader_endpoints(badge, target);
            let dx = end.x - start.x;
            let dy = end.y - start.y;
            let len_sq = dx * dx + dy * dy;
            // Anything shorter than ~6px reads as a stray pixel rather
            // than an arrow — skip the connector but keep the outline.
            if len_sq >= 36 {
                let c = premul(self.style.leader_color, self.style.badge_opacity);
                draw_line_argb(bits, dib_w, dib_h, start.x, start.y, end.x, end.y, c);
                let head = arrowhead_polygon(start, end);
                fill_triangle_argb(bits, dib_w, dib_h, &head, c);
            }
        }
    }

    fn key_down(&mut self, vk: u32, scancode: u32) {
        if vk == VK_ESCAPE.0 as u32 {
            self.selected = None;
            self.done = true;
            return;
        }
        if vk == VK_BACK.0 as u32 {
            self.typed.pop();
            crate::telemetry::note_backspace();
            return;
        }
        // Translate through the active keyboard layout so any key in the
        // alphabet (digits, `;`, Dvorak/Cyrillic letters…) is matchable.
        // The QWERTY character of the same physical key is accepted too,
        // for alphabets that fell back to QWERTY on keys the layout can't
        // render — but only when the OS character isn't used by any label,
        // so a mistyped key on Dvorak resets instead of silently matching
        // its QWERTY twin. Non-character keys (Shift, Ctrl, F-keys) are
        // ignored.
        let (os_char, qwerty_char) = super::keyboard_layout::key_chars(vk, scancode);
        let in_labels = |c: char| self.laid.iter().any(|h| h.label.contains(c));
        let ch = match os_char {
            Some(c) if in_labels(c) => Some(c),
            _ => qwerty_char.filter(|c| in_labels(*c)).or(os_char),
        };
        // Keys outside the alphabet are ignored like before, except letters,
        // which keep the old "wrong letter resets the prefix" behaviour. So
        // a stray `,` mid-label doesn't throw away what was typed.
        let ch = ch.filter(|c| in_labels(*c) || c.is_alphabetic());
        if let Some(ch) = ch {
            self.typed.push(ch);
            crate::telemetry::note_key();

            if let Some(idx) = self.laid.iter().position(|h| h.label == self.typed) {
                // Sample modifiers *only* on the completing keystroke.
                // Sampling on every prefix character would force the
                // user to hold Ctrl/Shift continuously across a
                // multi-letter label — annoying and easy to fumble.
                // Reading state once at completion is also the
                // semantically correct moment: it's the keystroke that
                // commits the pick.
                self.modifier = current_modifier();
                self.selected = Some(idx);
                self.done = true;
                return;
            }

            let any_prefix = self.laid.iter().any(|h| h.label.starts_with(&self.typed));
            if !any_prefix {
                self.typed.clear();
            }
        }
    }
}

/// Sample the Ctrl/Shift state right now.
///
/// `GetAsyncKeyState` returns a `SHORT` whose high-order bit is set
/// while the key is down; we test against `0x8000` (the cast through
/// `u16` keeps the bit pattern intact regardless of platform short
/// signedness). Ctrl takes precedence over Shift when both are held
/// — a deliberate, documented choice (see [`PickModifier`]) that
/// avoids needing a four-state enum for what users would mistype
/// half the time anyway.
fn current_modifier() -> PickModifier {
    const PRESSED: u16 = 0x8000;
    unsafe {
        let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & PRESSED != 0;
        let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16) & PRESSED != 0;
        if ctrl {
            PickModifier::Ctrl
        } else if shift {
            PickModifier::Shift
        } else {
            PickModifier::None
        }
    }
}

/// Anchor strategy for placing a badge relative to its target element.
///
/// We try these in order and pick the first one that doesn't collide with
/// an already-placed badge **and** stays inside the source element's
/// monitor. The order is tuned so the layout still *feels* like "badge
/// sits on the top-left of the thing" most of the time, while giving us
/// escape hatches when two elements share the same anchor or when the
/// element is too close to a monitor edge for the preferred placement
/// to fit.
#[derive(Debug, Clone, Copy)]
enum BadgePosition {
    /// Top-left of the element. The classic vimium/jumpkey look — keeps
    /// labels visually associated with the *start* of the control.
    TopLeft,
    /// Top-right of the element. Useful when the top-left of two adjacent
    /// controls would collide (think: two buttons in a toolbar).
    TopRight,
    /// Outside, just above the element. Falls off the element entirely so
    /// nothing visual is obscured. Skipped automatically when the element
    /// is too close to its monitor's top edge.
    OutsideTop,
    /// Outside, just below the element. The fallback when `OutsideTop`
    /// would clip off the top of the monitor (or off into the previous
    /// monitor on multi-monitor setups).
    OutsideBottom,
    /// Bottom-right of the element. Last "still on the element" option
    /// before we resort to vertical stacking.
    BottomRight,
}

/// Default candidate order: badge above the element first, then on it,
/// then below it as the off-element fallback. Used by the element picker
/// — small UI controls benefit from having the badge offset above them
/// so the user can see what they're about to invoke before pressing
/// the matching keys.
const ELEMENT_POSITION_CANDIDATES: [BadgePosition; 5] = [
    BadgePosition::OutsideTop,
    BadgePosition::TopLeft,
    BadgePosition::TopRight,
    BadgePosition::BottomRight,
    BadgePosition::OutsideBottom,
];

/// Window-picker candidate order: badge inside the window's top-left
/// (i.e. on the title bar) first. `OutsideTop` is dropped because for
/// any window at `y == 0` (i.e. maximized on the top of a monitor) it
/// would render above the monitor and be invisible.
const WINDOW_POSITION_CANDIDATES: [BadgePosition; 3] = [
    BadgePosition::TopLeft,
    BadgePosition::TopRight,
    BadgePosition::BottomRight,
];

/// Compute the (x, y) anchor (in client coordinates) for a given strategy.
///
/// `bounds` is the element's screen-space rect, `total_w`/`total_h` are
/// the badge+extra-pill dimensions, and `origin_x`/`origin_y` translate
/// from screen-space into client-space.
fn anchor_for(
    pos: BadgePosition,
    bounds: &Bounds,
    total_w: i32,
    total_h: i32,
    origin_x: i32,
    origin_y: i32,
) -> (i32, i32) {
    let x_base = bounds.x - origin_x;
    let y_base = bounds.y - origin_y;
    match pos {
        BadgePosition::TopLeft => (x_base, y_base),
        BadgePosition::TopRight => (x_base + bounds.width - total_w, y_base),
        BadgePosition::OutsideTop => (x_base, y_base - total_h - PILL_GAP),
        BadgePosition::OutsideBottom => (x_base, y_base + bounds.height + PILL_GAP),
        BadgePosition::BottomRight => (
            x_base + bounds.width - total_w,
            y_base + bounds.height - total_h,
        ),
    }
}

/// Bounds of the monitor whose work area contains the most of `bounds`,
/// in screen-space (physical pixels).
///
/// Returns `None` when the monitor query fails — the caller treats that
/// as "no monitor constraint, place anywhere" rather than refusing to
/// lay out.
unsafe fn monitor_for_bounds(bounds: &Bounds) -> Option<RECT> {
    let rect = RECT {
        left: bounds.x,
        top: bounds.y,
        right: bounds.x + bounds.width,
        bottom: bounds.y + bounds.height,
    };
    let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return None;
    }
    Some(info.rcMonitor)
}

/// Translate a screen-space `RECT` into the same client-space the
/// renderer uses (origin = top-left of the virtual desktop).
fn screen_to_client_rect(rect: RECT, origin_x: i32, origin_y: i32) -> RECT {
    RECT {
        left: rect.left - origin_x,
        top: rect.top - origin_y,
        right: rect.right - origin_x,
        bottom: rect.bottom - origin_y,
    }
}

/// DPI scale factor for the monitor containing `bounds`. Returns 1.0 at
/// standard 96 DPI, 1.5 at 144 DPI (150%), 2.0 at 192 DPI (200%), etc.
/// Falls back to 1.0 when the query fails (rare virtual-display edge case).
unsafe fn dpi_scale_for_bounds(bounds: &Bounds) -> f32 {
    let rect = RECT {
        left: bounds.x,
        top: bounds.y,
        right: bounds.x + bounds.width,
        bottom: bounds.y + bounds.height,
    };
    let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
    let mut dpi_x: u32 = 96;
    let mut dpi_y: u32 = 96;
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    dpi_x as f32 / 96.0
}

/// DPI scale factor for the primary monitor (the one at 0,0). Used by
/// the Settings UI to suggest a DPI-aware badge font size before any
/// hints have been placed.
pub fn primary_monitor_dpi_scale() -> f32 {
    let origin = Bounds {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    unsafe { dpi_scale_for_bounds(&origin) }
}

/// Suggested element badge font size for the primary monitor's DPI.
pub fn suggested_element_font_size() -> i32 {
    let base = HintStyle::elements().font_height;
    (base as f32 * primary_monitor_dpi_scale()).round() as i32
}

/// Suggested window badge font size for the primary monitor's DPI.
pub fn suggested_window_font_size() -> i32 {
    let base = HintStyle::windows().font_height;
    (base as f32 * primary_monitor_dpi_scale()).round() as i32
}

/// True when `rect` is fully contained inside `monitor` (in the same
/// coordinate space). Used during layout to drop candidate positions
/// that would push the badge off the source element's monitor.
fn rect_inside_monitor(rect: &RECT, monitor: &RECT) -> bool {
    rect.left >= monitor.left
        && rect.top >= monitor.top
        && rect.right <= monitor.right
        && rect.bottom <= monitor.bottom
}

/// Resolve hint anchors into final draw rectangles.
///
/// Two responsibilities:
///
/// 1. Convert each hint's screen-space anchor to client-space (subtract
///    the virtual-desktop origin so labels land on the right monitor on
///    multi-monitor setups whose primary isn't the leftmost).
/// 2. Detect collisions between badges. We try a small set of anchor
///    positions (style-dependent — [`ELEMENT_POSITION_CANDIDATES`] or
///    [`WINDOW_POSITION_CANDIDATES`]) for each hint before falling
///    back to vertical stacking from the original anchor. This keeps two
///    adjacent controls from rendering badges on top of each other while
///    still keeping each badge close to its element.
///
/// We estimate text widths from the font height rather than measuring
/// (which would need an HDC and font selection here). For Segoe UI Bold a
/// glyph is roughly 0.62 × font height; for the regular extra-pill text
/// it's around 0.50. Generous enough to avoid clipping; close enough that
/// the layout feels tight.
/// Font for a style's matchable labels.
fn label_font(style: &HintStyle) -> Font {
    Font {
        px: style.font_height,
        role: Role::Label,
    }
}

/// Font for a style's extra (window title) pill: ~75% of the label
/// height in regular weight, visible but clearly secondary.
fn extra_font(style: &HintStyle) -> Font {
    Font {
        px: (style.font_height * 3) / 4,
        role: Role::Title,
    }
}

/// Per-character advance widths (px) used to size badges. The overlay
/// font is monospaced, so a label is `chars × advance` wide.
#[derive(Debug, Clone, Copy)]
struct TextMetrics {
    label_advance: f32,
    extra_advance: f32,
}

impl TextMetrics {
    /// Advances measured from the font the labels are drawn with, so
    /// badge widths match the drawn text.
    fn measure(style: &HintStyle, text: &mut LabelText) -> Self {
        Self {
            label_advance: text.advance(label_font(style)),
            extra_advance: text.advance(extra_font(style)),
        }
    }

    /// Font-free estimate (the ratios layout used before text was
    /// measured). Keeps layout tests independent of installed fonts.
    #[cfg(any(test, feature = "bench"))]
    fn estimate(style: &HintStyle) -> Self {
        let fh = style.font_height as f32;
        Self {
            label_advance: ((fh * 0.62) as i32) as f32,
            extra_advance: ((fh * 0.50) as i32) as f32,
        }
    }

    fn label_w(&self, chars: usize) -> i32 {
        (chars as f32 * self.label_advance).ceil() as i32
    }

    fn extra_w(&self, chars: usize) -> i32 {
        (chars as f32 * self.extra_advance).ceil() as i32
    }
}

/// [`lay_out`] with [`TextMetrics::estimate`], for tests and benches.
#[cfg(any(test, feature = "bench"))]
fn lay_out_estimated(
    hints: &[Hint],
    style: &HintStyle,
    origin_x: i32,
    origin_y: i32,
) -> Vec<LaidHint> {
    lay_out(
        hints,
        style,
        TextMetrics::estimate(style),
        origin_x,
        origin_y,
    )
}

fn lay_out(
    hints: &[Hint],
    style: &HintStyle,
    metrics: TextMetrics,
    origin_x: i32,
    origin_y: i32,
) -> Vec<LaidHint> {
    let row_h = style.font_height + style.padding_y * 2;

    // Process hints in reading order (top-then-left) so collision
    // resolution stacks predictably from the natural anchor downward.
    let mut order: Vec<usize> = (0..hints.len()).collect();
    order.sort_by_key(|&i| (hints[i].bounds.y - origin_y, hints[i].bounds.x - origin_x));

    let mut result: Vec<Option<LaidHint>> = (0..hints.len()).map(|_| None).collect();
    // Bounding box (in client coords) of every laid hint, kept dense so
    // the spatial grid can index into it; collision queries iterate the
    // grid's bucket lists, not the whole vector.
    let mut placed: Vec<RECT> = Vec::with_capacity(hints.len());
    let mut grid = SpatialGrid::with_capacity(hints.len());

    // Callouts only make sense for a lone label: a hint with an extra
    // title pill (window picker) keeps the rectangular layout.
    let callout_geom = style.callout_geom_if_enabled();

    for &i in &order {
        let h = &hints[i];
        let callout = callout_geom.filter(|_| h.extra.is_none());

        let label_w = metrics.label_w(h.label.chars().count());
        let (badge_w, row_h) = match callout {
            Some(g) => (g.body_width(label_w), g.body_h),
            None => (label_w + style.padding_x * 2, row_h),
        };

        let extra_w = h.extra.as_ref().map(|s| {
            let chars = s.chars().count().min(MAX_EXTRA_CHARS);
            metrics.extra_w(chars) + style.padding_x * 2
        });

        let total_w = badge_w + extra_w.map(|w| PILL_GAP + w).unwrap_or(0);
        let total_h = row_h;

        // Determine which monitor owns this element. We constrain badge
        // placement to that monitor so a hint at the right edge of a
        // 1920px monitor never bleeds onto the leftmost pixel column of
        // the next monitor over (a confusing UX bug on multi-monitor
        // setups). When the monitor query fails (rare; usually means
        // some odd virtual display) we fall back to "no constraint" so
        // we still produce *some* layout.
        let monitor_client = unsafe {
            monitor_for_bounds(&h.bounds).map(|m| screen_to_client_rect(m, origin_x, origin_y))
        };

        // Element rect in client space. Callouts aim their tail at it,
        // and the leader-line painter needs it to figure out where each
        // badge should point.
        let target_rect = RECT {
            left: h.bounds.x - origin_x,
            top: h.bounds.y - origin_y,
            right: h.bounds.x - origin_x + h.bounds.width,
            bottom: h.bounds.y - origin_y + h.bounds.height,
        };

        // Phase 0 (callout shape): below / above / slid / beside the
        // element with a tail on it. First spot that is free and on the
        // element's monitor wins; otherwise fall through to the corner
        // positions below, drawn as a rounded badge without a tail.
        if let Some(g) = callout {
            let spot = callout_candidates(&target_rect, badge_w, g)
                .into_iter()
                .find(|(_, _, bbox)| {
                    monitor_client.map_or(true, |m| rect_inside_monitor(bbox, &m))
                        && !grid.any_intersects(bbox, &placed)
                });
            if let Some((badge_rect, tail, bbox)) = spot {
                let idx = placed.len();
                placed.push(bbox);
                grid.insert(idx, &bbox);
                result[i] = Some(LaidHint {
                    label: h.label.clone(),
                    extra: None,
                    badge_rect,
                    extra_rect: None,
                    target_rect,
                    callout: true,
                    tail: Some(tail),
                });
                continue;
            }
        }

        // Phase 1: try each anchor strategy in order. The first one that
        // doesn't collide with an already-placed badge AND fits on the
        // source element's monitor wins. The order is style-dependent —
        // see `WINDOW_POSITION_CANDIDATES` for why the window picker
        // can't use `OutsideTop`.
        let candidates: &[BadgePosition] = if style.prefer_inside_anchor {
            &WINDOW_POSITION_CANDIDATES
        } else {
            &ELEMENT_POSITION_CANDIDATES
        };
        let mut chosen: Option<(i32, i32)> = None;
        for &pos in candidates {
            let (cx, cy) = anchor_for(pos, &h.bounds, total_w, total_h, origin_x, origin_y);
            let candidate = RECT {
                left: cx,
                top: cy,
                right: cx + total_w,
                bottom: cy + total_h,
            };
            // Cross-monitor guard: if we know the element's monitor and
            // this candidate would land outside it, reject the candidate
            // and try the next strategy. Without this, an element at
            // y == 0 with the element-style preference for `OutsideTop`
            // would render its badge on the previous monitor (or off the
            // virtual desktop entirely).
            if let Some(m) = monitor_client {
                if !rect_inside_monitor(&candidate, &m) {
                    continue;
                }
            }
            if !grid.any_intersects(&candidate, &placed) {
                chosen = Some((cx, cy));
                break;
            }
        }

        // Phase 2: fall back to "stack downward from the original anchor"
        // if every preferred position is already taken. Bounded by a
        // safety cap so a pathological input can't loop forever.
        let (x, mut y) = chosen.unwrap_or_else(|| {
            anchor_for(
                BadgePosition::TopLeft,
                &h.bounds,
                total_w,
                total_h,
                origin_x,
                origin_y,
            )
        });

        if chosen.is_none() {
            let mut attempts = 0;
            loop {
                let candidate = RECT {
                    left: x,
                    top: y,
                    right: x + total_w,
                    bottom: y + total_h,
                };
                if !grid.any_intersects(&candidate, &placed) {
                    break;
                }
                y += row_h + PILL_GAP;
                attempts += 1;
                if attempts > 256 {
                    break;
                }
            }
        }

        // Phase 3: clamp the final placement to the source monitor.
        // Even when phase-1 picked a candidate that fit, phase-2's
        // stacking loop or aggressive fallbacks may have walked the
        // badge below the monitor's bottom edge. Pull it back up
        // instead of letting it leak to the next monitor.
        let (mut x, mut y) = (x, y);
        if let Some(m) = monitor_client {
            if x + total_w > m.right {
                x = m.right - total_w;
            }
            if x < m.left {
                x = m.left;
            }
            if y + total_h > m.bottom {
                y = m.bottom - total_h;
            }
            if y < m.top {
                y = m.top;
            }
        }

        let badge_rect = RECT {
            left: x,
            top: y,
            right: x + badge_w,
            bottom: y + row_h,
        };
        let extra_rect = extra_w.map(|w| RECT {
            left: x + badge_w + PILL_GAP,
            top: y,
            right: x + badge_w + PILL_GAP + w,
            bottom: y + row_h,
        });

        let bbox = RECT {
            left: x,
            top: y,
            right: x + total_w,
            bottom: y + total_h,
        };
        let idx = placed.len();
        placed.push(bbox);
        grid.insert(idx, &bbox);
        result[i] = Some(LaidHint {
            label: h.label.clone(),
            extra: h.extra.clone(),
            badge_rect,
            extra_rect,
            target_rect,
            callout: callout.is_some(),
            tail: None,
        });
    }

    result
        .into_iter()
        .map(|o| o.expect("laid every hint"))
        .collect()
}

fn rects_intersect(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// Cell side, in client pixels, for [`SpatialGrid`]. Tuned to roughly
/// the size of a window-style badge so the typical badge touches 1-4
/// cells, never enough to make the per-cell list grow past `SmallVec`'s
/// inline capacity.
const GRID_CELL: i32 = 64;

/// Hash-bucketed broad-phase index for badge bounding boxes. Each cell
/// holds the indices (into the caller-owned `Vec<RECT>`) of every
/// placed rect that touches the cell. Collision queries hit only the
/// cells the candidate overlaps, dropping `lay_out`'s collision pass
/// from O(n²) to amortized O(n) for our typical n = 50-800 badges.
///
/// Key choices:
/// - `i32` cell coords let us bucket the full virtual-desktop range
///   without overflow even for the +/-32k px coords on extreme
///   multi-monitor setups.
/// - `SmallVec<[usize; 8]>` inlines the bucket contents until a cell
///   gets unusually crowded, which keeps the per-bucket allocation
///   count at zero for the common case.
struct SpatialGrid {
    cells: HashMap<(i32, i32), SmallVec<[usize; 8]>>,
}

impl SpatialGrid {
    fn with_capacity(hint_count: usize) -> Self {
        // Each badge typically lights up 1-4 cells, so reserving for
        // ~2x the badge count avoids most rehash bumps without
        // over-allocating for tiny picks.
        Self {
            cells: HashMap::with_capacity(hint_count.saturating_mul(2)),
        }
    }

    /// Range of cell coordinates a rect overlaps, expressed as the
    /// half-open `(x_lo..x_hi, y_lo..y_hi)`.
    fn cell_range(rect: &RECT) -> (i32, i32, i32, i32) {
        let x_lo = rect.left.div_euclid(GRID_CELL);
        let y_lo = rect.top.div_euclid(GRID_CELL);
        // -1 on the upper edge so a rect that ends exactly on a cell
        // boundary doesn't claim the next cell over (right/bottom in
        // RECT are exclusive).
        let x_hi = (rect.right - 1).div_euclid(GRID_CELL);
        let y_hi = (rect.bottom - 1).div_euclid(GRID_CELL);
        (x_lo, y_lo, x_hi, y_hi)
    }

    fn insert(&mut self, idx: usize, rect: &RECT) {
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        let (x_lo, y_lo, x_hi, y_hi) = Self::cell_range(rect);
        for cy in y_lo..=y_hi {
            for cx in x_lo..=x_hi {
                self.cells.entry((cx, cy)).or_default().push(idx);
            }
        }
    }

    /// True if any previously-inserted rect (looked up via `placed`)
    /// intersects `candidate`. Visits each candidate-id at most once
    /// per cell, then deduplicates via a tiny stack-allocated set so
    /// rects spanning multiple cells don't get tested twice.
    fn any_intersects(&self, candidate: &RECT, placed: &[RECT]) -> bool {
        if candidate.right <= candidate.left || candidate.bottom <= candidate.top {
            return false;
        }
        let (x_lo, y_lo, x_hi, y_hi) = Self::cell_range(candidate);
        let mut seen: SmallVec<[usize; 16]> = SmallVec::new();
        for cy in y_lo..=y_hi {
            for cx in x_lo..=x_hi {
                let Some(bucket) = self.cells.get(&(cx, cy)) else {
                    continue;
                };
                for &idx in bucket {
                    if seen.contains(&idx) {
                        continue;
                    }
                    seen.push(idx);
                    if rects_intersect(&placed[idx], candidate) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Pick start/end points for the leader line connecting `badge` to
/// `target`. Returns `(start_on_badge, end_on_target)`.
///
/// We classify the relative position of the badge along each axis
/// (left/inside/right and above/inside/below) and pick the edges that
/// face each other. For axis-aligned cases (badge purely above /
/// purely beside the target) this collapses to a clean perpendicular
/// arrow; for diagonal cases (badge in a corner outside the element)
/// we get a clean diagonal pointing at the nearest corner of the
/// element.
fn leader_endpoints(badge: &RECT, target: &RECT) -> (POINT, POINT) {
    // Horizontal classification.
    let (badge_x, target_x) = if badge.right <= target.left {
        (badge.right, target.left)
    } else if badge.left >= target.right {
        (badge.left, target.right)
    } else {
        // Overlap on this axis — meet at the centre of the overlap so
        // the arrow stays inside both rects horizontally.
        let cx = (badge.left.max(target.left) + badge.right.min(target.right)) / 2;
        (cx, cx)
    };

    let (badge_y, target_y) = if badge.bottom <= target.top {
        (badge.bottom, target.top)
    } else if badge.top >= target.bottom {
        (badge.top, target.bottom)
    } else {
        let cy = (badge.top.max(target.top) + badge.bottom.min(target.bottom)) / 2;
        (cy, cy)
    };

    (
        POINT {
            x: badge_x,
            y: badge_y,
        },
        POINT {
            x: target_x,
            y: target_y,
        },
    )
}

/// Build the three vertices of a small filled triangle pointing from
/// `start` toward `end`, with the tip at `end`.
///
/// The arrowhead is a fixed ~6×6 px triangle — big enough to read at a
/// glance without competing with the badge for visual weight.
fn arrowhead_polygon(start: POINT, end: POINT) -> [POINT; 3] {
    const HEAD_LEN: f32 = 7.0;
    const HEAD_HALF_W: f32 = 4.0;

    let dx = (end.x - start.x) as f32;
    let dy = (end.y - start.y) as f32;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let ux = dx / len;
    let uy = dy / len;
    // Perpendicular to (ux, uy), rotated 90° clockwise.
    let px = -uy;
    let py = ux;

    let base_x = end.x as f32 - ux * HEAD_LEN;
    let base_y = end.y as f32 - uy * HEAD_LEN;

    let left = POINT {
        x: (base_x + px * HEAD_HALF_W).round() as i32,
        y: (base_y + py * HEAD_HALF_W).round() as i32,
    };
    let right = POINT {
        x: (base_x - px * HEAD_HALF_W).round() as i32,
        y: (base_y - py * HEAD_HALF_W).round() as i32,
    };
    [end, left, right]
}

/// Low-level mouse hook callback installed for the duration of a pick.
///
/// Windows 10+ routes `WM_MOUSEWHEEL` to the window under the cursor
/// (not the focus window) when "Scroll inactive windows when I hover
/// over them" is enabled (the default). That means wheel events bypass
/// the overlay entirely and scroll the page underneath, leaving the
/// badges at stale positions. This hook intercepts the wheel before
/// it reaches any window and posts [`WM_SCROLL_DISMISS`] to the
/// overlay so `wnd_proc` can cancel the pick.
unsafe extern "system" fn scroll_hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wp.0 as u32;
        if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL_RAW {
            SCROLL_HOOK_TARGET.with(|cell| {
                let hwnd = cell.get();
                if !hwnd.0.is_null() {
                    let _ = PostMessageW(hwnd, WM_SCROLL_DISMISS, WPARAM(0), LPARAM(0));
                }
            });
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lp.0 as *const CREATESTRUCTW;
        let state_ptr = (*cs).lpCreateParams;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }

    // GWLP_USERDATA points at the OverlayState owned by the active
    // pick_hint() call. It's null between picks (the persistent HWND is
    // hidden), in which case we just forward to DefWindowProc — the
    // window has nothing to render or react to.
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let state = &mut *state_ptr;

    match msg {
        // ULW_ALPHA layered windows don't get WM_PAINT for their content
        // (DWM composites directly from the DIB we hand it via
        // UpdateLayeredWindow). Swallow it so DefWindowProcW doesn't try
        // to BeginPaint/EndPaint on the layered surface.
        WM_PAINT => LRESULT(0),
        WM_KEYDOWN => {
            state.key_down(wp.0 as u32, ((lp.0 as u32) >> 16) & 0xFF);
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            // Treat focus loss as cancel — but only when the pick is
            // still active. The persistent-overlay teardown calls
            // ShowWindow(SW_HIDE) on a window that still owns focus,
            // and Windows synchronously dispatches WM_KILLFOCUS during
            // that hide; without the `state.done` guard that path would
            // clobber a just-set `selected` back to `None` and silently
            // swallow the user's pick.
            if !state.done {
                state.selected = None;
                state.done = true;
            }
            LRESULT(0)
        }
        WM_SCROLL_DISMISS => {
            if !state.done {
                tracing::debug!("overlay dismissed: scroll detected");
                state.selected = None;
                state.done = true;
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn ensure_class_registered(hinstance: HINSTANCE) -> Result<()> {
    let mut existing = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        ..Default::default()
    };
    if GetClassInfoExW(hinstance, CLASS_NAME, &mut existing).is_ok() {
        return Ok(());
    }
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    let atom = RegisterClassExW(&class);
    if atom == 0 {
        return Err(anyhow!("RegisterClassExW returned 0"));
    }
    Ok(())
}

unsafe fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    (
        GetSystemMetrics(SM_XVIRTUALSCREEN),
        GetSystemMetrics(SM_YVIRTUALSCREEN),
        GetSystemMetrics(SM_CXVIRTUALSCREEN),
        GetSystemMetrics(SM_CYVIRTUALSCREEN),
    )
}

/// Show the hint overlay and block on a Win32 message loop until the
/// user picks a hint, presses `Esc`, or the window loses focus.
///
/// Returns `Ok(Some(result))` describing the chosen hint and the
/// modifier (if any) held when the user committed the pick — see
/// [`PickResult`]. Returns `Ok(None)` on cancel / focus loss / empty
/// input.
///
/// The HWND, off-screen DIB, and memory DC are owned by a thread-local
/// [`PersistentOverlay`] and reused across calls; only the per-pick
/// [`OverlayState`] is allocated/freed each time. This drops the
/// per-pick window-creation latency (~20-40 ms on Windows 11) to zero
/// after the first hotkey press.
pub fn pick_hint(hints: Vec<Hint>, style: HintStyle) -> Result<Option<PickResult>> {
    if hints.is_empty() {
        return Ok(None);
    }

    PERSISTENT_OVERLAY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let _s = crate::tel_span!("overlay_create").entered();
            *slot = Some(unsafe { PersistentOverlay::new()? });
        }
        let overlay = slot.as_mut().expect("just initialized");
        unsafe { overlay.run_pick(hints, style) }
    })
}

// Per-thread cache of the persistent overlay. Hotkey handling is
// single-threaded (it runs on the main message loop), so a thread-local
// is enough to make the HWND/DIB/DC truly process-wide.
thread_local! {
    static PERSISTENT_OVERLAY: RefCell<Option<PersistentOverlay>> = const { RefCell::new(None) };
}

// Overlay HWND exposed to `scroll_hook_proc` so the low-level mouse
// hook can post `WM_SCROLL_DISMISS` to the right window. Set/cleared
// inside `PersistentOverlay::run_pick`; null between picks.
thread_local! {
    static SCROLL_HOOK_TARGET: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

/// Persistent rendering resources for the overlay: the layered HWND
/// covering the virtual desktop, an off-screen ARGB DIB sized to that
/// desktop, and a memory DC bound to the DIB. Recreated lazily on a
/// virtual-desktop size change (monitor connect/disconnect).
struct PersistentOverlay {
    hwnd: HWND,
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
    origin_x: i32,
    origin_y: i32,
    /// Label rasteriser and font cache, kept across picks so fonts are
    /// set up once per size rather than on every hotkey press.
    text: LabelText,
}

impl PersistentOverlay {
    unsafe fn new() -> Result<Self> {
        // Best-effort: tag the process as PerMonitorV2 DPI-aware so
        // source pixel coords line up with our overlay coords on
        // high-DPI displays. Safe to call repeatedly; ignore "already
        // set" / older-OS errors.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinstance: HINSTANCE = GetModuleHandleW(PCWSTR::null())?.into();
        ensure_class_registered(hinstance)?;

        let (vx, vy, vw, vh) = virtual_screen_rect();

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            WINDOW_TITLE,
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            None,
            None,
            hinstance,
            None,
        )
        .context("CreateWindowExW failed")?;

        let (mem_dc, dib, bits) =
            create_dib_surface(vw, vh).context("failed to allocate overlay DIB surface")?;

        let _ = hinstance;
        Ok(Self {
            hwnd,
            mem_dc,
            dib,
            bits,
            width: vw,
            height: vh,
            origin_x: vx,
            origin_y: vy,
            text: LabelText::new(),
        })
    }

    /// Reallocate DIB + reposition HWND if the virtual desktop changed
    /// (e.g. monitor hot-plug). Idempotent when nothing has moved.
    unsafe fn ensure_geometry(&mut self) -> Result<()> {
        let (vx, vy, vw, vh) = virtual_screen_rect();
        if vx == self.origin_x && vy == self.origin_y && vw == self.width && vh == self.height {
            return Ok(());
        }
        let _ = SetWindowPos(
            self.hwnd,
            None,
            vx,
            vy,
            vw,
            vh,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        if vw != self.width || vh != self.height {
            // Tear down the old DIB before allocating the new one so we
            // don't double the peak memory footprint on reconnects.
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
            let (mem_dc, dib, bits) =
                create_dib_surface(vw, vh).context("failed to reallocate overlay DIB surface")?;
            self.mem_dc = mem_dc;
            self.dib = dib;
            self.bits = bits;
            self.width = vw;
            self.height = vh;
        }
        self.origin_x = vx;
        self.origin_y = vy;
        Ok(())
    }

    unsafe fn run_pick(
        &mut self,
        hints: Vec<Hint>,
        style: HintStyle,
    ) -> Result<Option<PickResult>> {
        self.ensure_geometry()?;

        let (vx, vy, vw, vh) = (self.origin_x, self.origin_y, self.width, self.height);
        tracing::debug!(
            vx,
            vy,
            vw,
            vh,
            hint_count = hints.len(),
            "overlay virtual desktop rect"
        );

        let mut style = style;
        if style.auto_scale_dpi {
            if let Some(first) = hints.first() {
                let scale = dpi_scale_for_bounds(&first.bounds);
                style = style.scaled_for_dpi(scale);
                if scale > 1.0 {
                    tracing::debug!(scale, font_height = style.font_height, "DPI-scaled overlay");
                }
            }
        }

        let mut state = {
            let _s = crate::tel_span!("overlay_state", n = hints.len() as u64).entered();
            OverlayState::new(hints, style, vx, vy, &mut self.text)
        };

        // Bind the state pointer to the persistent HWND so wnd_proc can
        // route key events into it. We clear the pointer before
        // dropping the state so a stray late message can't dereference
        // freed memory.
        SetWindowLongPtrW(
            self.hwnd,
            GWLP_USERDATA,
            (&mut state as *mut OverlayState) as isize,
        );

        // Initial render + show. ShowWindow with SW_SHOWNOACTIVATE keeps
        // the previously focused app visually unchanged underneath the
        // overlay; we still SetForegroundWindow + SetFocus so key
        // events route to us.
        {
            let _s = crate::tel_span!("render").entered();
            state.render_to_dib(self.hwnd, self.mem_dc, self.bits, self.width, self.height);
        }
        // Pixels handed to DWM (UpdateLayeredWindow) but not yet shown.
        crate::tel_event!("first_ulw");
        {
            let _s = crate::tel_span!("show_focus").entered();
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
        }
        crate::tel_event!("shown");

        // Install a low-level mouse hook to detect scroll events.
        // Windows 10+'s "scroll inactive windows" routes WM_MOUSEWHEEL
        // to the window under the cursor, bypassing our focused overlay.
        // The hook intercepts wheel events globally and posts
        // WM_SCROLL_DISMISS so the overlay cancels before the badges
        // desync from the scrolled content.
        SCROLL_HOOK_TARGET.with(|cell| cell.set(self.hwnd));
        let scroll_hook: Option<HHOOK> =
            SetWindowsHookExW(WH_MOUSE_LL, Some(scroll_hook_proc), None, 0).ok();

        // Modal pump: spin until the state flags `done`. After every
        // dispatched message we re-render so live prefix-typing visibly
        // hides non-matching badges. Clippy can't see through the
        // wnd_proc indirection that flips `state.done`, so silence the
        // "not mutated in the loop body" lint here.
        let mut last_typed_len = state.typed.len();
        let mut msg = MSG::default();
        #[allow(clippy::while_immutable_condition)]
        while !state.done {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if state.done {
                break;
            }
            if state.typed.len() != last_typed_len {
                state.render_to_dib(self.hwnd, self.mem_dc, self.bits, self.width, self.height);
                last_typed_len = state.typed.len();
            }
        }
        crate::tel_event!("key_complete");

        // Tear down the scroll hook before anything else so a stray
        // WM_SCROLL_DISMISS can't arrive after we've snapshotted the
        // result or started hiding the window.
        if let Some(hook) = scroll_hook {
            let _ = UnhookWindowsHookEx(hook);
        }
        SCROLL_HOOK_TARGET.with(|cell| cell.set(HWND(std::ptr::null_mut())));

        // Snapshot the user's selection BEFORE any teardown. ShowWindow
        // (SW_HIDE) on a focused window synchronously dispatches
        // WM_KILLFOCUS through wnd_proc, and even with the `state.done`
        // guard in the handler we don't want any future bookkeeping in
        // there to risk overwriting `selected`. Read once, return that.
        let result = state.selected.map(|index| PickResult {
            index,
            modifier: state.modifier,
        });

        // Hide first, then unbind the state pointer. The order matters
        // because ShowWindow can pump a few WM_KILLFOCUS / WM_NCACTIVATE
        // messages synchronously and we want them to find a still-valid
        // state (so the wnd_proc handler short-circuits on `state.done`)
        // rather than a null pointer it then has to special-case.
        let _ = ShowWindow(self.hwnd, SW_HIDE);
        SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);

        Ok(result)
    }
}

impl Drop for PersistentOverlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

/// Allocate a top-down 32-bit BGRA DIB section, bind it to a fresh
/// memory DC, and return raw pointers to all three. The caller owns
/// the lifetimes and must DeleteDC + DeleteObject when done.
unsafe fn create_dib_surface(width: i32, height: i32) -> Result<(HDC, HBITMAP, *mut u8)> {
    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(screen_dc);
    let _ = ReleaseDC(None, screen_dc);
    if mem_dc.is_invalid() {
        return Err(anyhow!("CreateCompatibleDC returned NULL"));
    }

    // Negative biHeight = top-down, which lines up with our (x, y)
    // pixel-addressing convention so we don't have to flip rows when
    // patching alpha.
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        .context("CreateDIBSection failed")?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem_dc);
        return Err(anyhow!("CreateDIBSection returned NULL bits"));
    }
    let _ = SelectObject(mem_dc, HGDIOBJ(dib.0));

    Ok((mem_dc, dib, bits as *mut u8))
}

// -- Direct ARGB rasterization helpers --------------------------------
//
// Everything visible after UpdateLayeredWindow is written straight into
// the DIB as premultiplied BGRA (GDI drawing would leave alpha at 0).
// Colours are converted once with [`premul`], which also bakes in the
// badge opacity.
//
// All of these treat `bits` as a top-down BGRA buffer of `dib_w * dib_h`
// pixels and silently clip writes that fall outside the buffer.

/// `color` at opacity `alpha`, premultiplied, in DIB byte order
/// `[b, g, r, a]` (`COLORREF` is `0x00BBGGRR`, red in the low byte).
fn premul(color: COLORREF, alpha: u8) -> [u8; 4] {
    let c = color.0;
    let ch = |shift: u32| (((c >> shift) & 0xFF) * alpha as u32 + 127) / 255;
    [ch(16) as u8, ch(8) as u8, ch(0) as u8, alpha]
}

/// Write one premultiplied pixel at `(x, y)`.
#[inline]
unsafe fn put_pixel_argb(bits: *mut u8, dib_w: i32, dib_h: i32, x: i32, y: i32, px: [u8; 4]) {
    if x < 0 || y < 0 || x >= dib_w || y >= dib_h || bits.is_null() {
        return;
    }
    let stride = (dib_w as usize) * 4;
    let p = bits.add((y as usize) * stride + (x as usize) * 4);
    std::ptr::copy_nonoverlapping(px.as_ptr(), p, 4);
}

/// Fill `rect ∩ dib` with one premultiplied pixel value. Used for badge
/// fills and the four edges of [`frame_rect_argb`].
unsafe fn fill_rect_argb(bits: *mut u8, dib_w: i32, dib_h: i32, rect: &RECT, px: [u8; 4]) {
    let x0 = rect.left.max(0);
    let y0 = rect.top.max(0);
    let x1 = rect.right.min(dib_w);
    let y1 = rect.bottom.min(dib_h);
    if x0 >= x1 || y0 >= y1 || bits.is_null() {
        return;
    }
    let stride = (dib_w as usize) * 4;
    let value = u32::from_ne_bytes(px);
    for y in y0..y1 {
        let row = bits.add((y as usize) * stride + (x0 as usize) * 4) as *mut u32;
        std::slice::from_raw_parts_mut(row, (x1 - x0) as usize).fill(value);
    }
}

/// Composite glyph coverage `cov` in `color` with its top-left at
/// `(x0, y0)`: premultiplied "over", so glyph pixels end up fully
/// opaque wherever the coverage is full, whatever was underneath.
unsafe fn blit_coverage(
    bits: *mut u8,
    dib_w: i32,
    dib_h: i32,
    cov: &Coverage,
    x0: i32,
    y0: i32,
    color: COLORREF,
) {
    if bits.is_null() || cov.a.is_empty() {
        return;
    }
    let fg = premul(color, 255);
    let stride = (dib_w as usize) * 4;
    let sx0 = (-x0).max(0);
    let sx1 = cov.w.min(dib_w - x0);
    for sy in (-y0).max(0)..cov.h.min(dib_h - y0) {
        let src = &cov.a[(sy * cov.w) as usize..((sy + 1) * cov.w) as usize];
        let row = bits.add(((y0 + sy) as usize) * stride);
        for sx in sx0..sx1 {
            let c = src[sx as usize] as u32;
            if c == 0 {
                continue;
            }
            let p = row.add(((x0 + sx) as usize) * 4);
            let inv = 255 - c;
            for (k, &f) in fg.iter().enumerate() {
                let d = *p.add(k) as u32;
                *p.add(k) = ((f as u32 * c + d * inv + 127) / 255) as u8;
            }
        }
    }
}

/// 1px frame around `rect`, drawn directly into the DIB. Mirrors
/// `FrameRect`'s semantics (rect.right and rect.bottom are exclusive)
/// so callers can keep the same coordinates they used with GDI.
unsafe fn frame_rect_argb(bits: *mut u8, dib_w: i32, dib_h: i32, rect: &RECT, color: [u8; 4]) {
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let top = RECT {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.top + 1,
    };
    let bottom = RECT {
        left: rect.left,
        top: rect.bottom - 1,
        right: rect.right,
        bottom: rect.bottom,
    };
    let left = RECT {
        left: rect.left,
        top: rect.top,
        right: rect.left + 1,
        bottom: rect.bottom,
    };
    let right = RECT {
        left: rect.right - 1,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    };
    fill_rect_argb(bits, dib_w, dib_h, &top, color);
    fill_rect_argb(bits, dib_w, dib_h, &bottom, color);
    fill_rect_argb(bits, dib_w, dib_h, &left, color);
    fill_rect_argb(bits, dib_w, dib_h, &right, color);
}

/// Bresenham line rasterizer that writes ARGB pixels directly. We use
/// it instead of `LineTo` because GDI line drawing leaves the alpha
/// channel at 0 (so the line would be invisible after
/// UpdateLayeredWindow even though the pixels were written).
#[allow(clippy::too_many_arguments)]
unsafe fn draw_line_argb(
    bits: *mut u8,
    dib_w: i32,
    dib_h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: [u8; 4],
) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    loop {
        put_pixel_argb(bits, dib_w, dib_h, x, y, color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Filled triangle in ARGB via a simple scanline fill. The arrowhead
/// triangles we draw are at most ~7 px on a side, so a barycentric or
/// edge-walk approach would be over-engineering — bounding-box +
/// orientation test is plenty.
unsafe fn fill_triangle_argb(
    bits: *mut u8,
    dib_w: i32,
    dib_h: i32,
    pts: &[POINT; 3],
    color: [u8; 4],
) {
    let min_x = pts.iter().map(|p| p.x).min().unwrap_or(0).max(0);
    let max_x = pts.iter().map(|p| p.x).max().unwrap_or(0).min(dib_w - 1);
    let min_y = pts.iter().map(|p| p.y).min().unwrap_or(0).max(0);
    let max_y = pts.iter().map(|p| p.y).max().unwrap_or(0).min(dib_h - 1);
    if min_x > max_x || min_y > max_y {
        return;
    }
    // Edge-function sign test: a point is inside the triangle iff it
    // lies on the same side of all three edges. We pick the convention
    // by sampling the centroid so the test works for both winding
    // orders (the arrowhead helper doesn't guarantee CW vs CCW).
    let edge = |ax: i32, ay: i32, bx: i32, by: i32, cx: i32, cy: i32| -> i32 {
        (bx - ax) * (cy - ay) - (by - ay) * (cx - ax)
    };
    let cx = (pts[0].x + pts[1].x + pts[2].x) / 3;
    let cy = (pts[0].y + pts[1].y + pts[2].y) / 3;
    let s0 = edge(pts[0].x, pts[0].y, pts[1].x, pts[1].y, cx, cy);
    let s1 = edge(pts[1].x, pts[1].y, pts[2].x, pts[2].y, cx, cy);
    let s2 = edge(pts[2].x, pts[2].y, pts[0].x, pts[0].y, cx, cy);
    let want_pos = s0 >= 0 && s1 >= 0 && s2 >= 0;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let e0 = edge(pts[0].x, pts[0].y, pts[1].x, pts[1].y, x, y);
            let e1 = edge(pts[1].x, pts[1].y, pts[2].x, pts[2].y, x, y);
            let e2 = edge(pts[2].x, pts[2].y, pts[0].x, pts[0].y, x, y);
            let inside = if want_pos {
                e0 >= 0 && e1 >= 0 && e2 >= 0
            } else {
                e0 <= 0 && e1 <= 0 && e2 <= 0
            };
            if inside {
                put_pixel_argb(bits, dib_w, dib_h, x, y, color);
            }
        }
    }
}

// -- Callout sprites ---------------------------------------------------
//
// A callout (rounded body + tail + border + gradient + drop shadow) is
// rendered once per distinct size/tail into a small premultiplied BGRA
// buffer, then blitted per hint. Edges are anti-aliased from signed
// distances (one evaluation per pixel), and the shadow blur happens at
// build time only, so per-frame cost is a copy plus the label text.

/// Drop-shadow offset (down) and box-blur radius, in pixels.
const SHADOW_OFFSET: i32 = 2;
const SHADOW_BLUR: i32 = 2;
/// Shadow opacity (0..1), as in Homerow's `rgba(0,0,0,0.2)`.
const SHADOW_ALPHA: f32 = 0.2;

/// Cache key: every callout with the same body size and tail placement
/// is pixel-identical apart from its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SpriteKey {
    w: i32,
    h: i32,
    tail: Option<Tail>,
}

impl SpriteKey {
    fn for_hint(h: &LaidHint) -> Self {
        Self {
            w: h.badge_rect.right - h.badge_rect.left,
            h: h.badge_rect.bottom - h.badge_rect.top,
            tail: h.tail,
        }
    }
}

/// Pre-rendered callout.
struct Sprite {
    w: i32,
    h: i32,
    /// Where the body's top-left sits inside the sprite (the rest is
    /// tail and shadow margin).
    body_x: i32,
    body_y: i32,
    /// Premultiplied BGRA, row-major, `w * h` pixels.
    px: Vec<[u8; 4]>,
    /// Per row, the `[start, end)` columns that aren't fully
    /// transparent, so blits skip empty corners and margins.
    spans: Vec<(i32, i32)>,
}

/// Blend two colors: `t = 0` → `a`, `t = 1` → `b`. Returns floats in
/// 0..=255 in DIB byte order `[b, g, r]` (`COLORREF` is `0x00BBGGRR`,
/// red in the low byte).
fn mix_bgr(a: COLORREF, b: COLORREF, t: f32) -> [f32; 3] {
    let ch = |c: COLORREF, shift: u32| ((c.0 >> shift) & 0xFF) as f32;
    [16, 8, 0].map(|s| ch(a, s) + (ch(b, s) - ch(a, s)) * t)
}

/// Signed distance from `(px, py)` to a rounded box centred at
/// `(cx, cy)` with half-extents `(hw, hh)` and corner radius `r`.
/// Negative inside.
fn sd_round_box(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - hw + r;
    let qy = (py - cy).abs() - hh + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Signed distance to a triangle, as the max of the distances to its
/// three edge lines. Exact inside and along edges (all the
/// anti-aliasing needs); slightly short outside the corners.
fn sd_triangle(px: f32, py: f32, tri: &[(f32, f32); 3]) -> f32 {
    let (cx, cy) = (
        (tri[0].0 + tri[1].0 + tri[2].0) / 3.0,
        (tri[0].1 + tri[1].1 + tri[2].1) / 3.0,
    );
    let mut d = f32::MIN;
    for i in 0..3 {
        let (ax, ay) = tri[i];
        let (bx, by) = tri[(i + 1) % 3];
        let (ex, ey) = (bx - ax, by - ay);
        let len = (ex * ex + ey * ey).sqrt().max(1e-6);
        // Normal pointing away from the centroid.
        let (mut nx, mut ny) = (ey / len, -ex / len);
        if (cx - ax) * nx + (cy - ay) * ny > 0.0 {
            nx = -nx;
            ny = -ny;
        }
        d = d.max((px - ax) * nx + (py - ay) * ny);
    }
    d
}

/// Render a callout body of `body_w` × `body_h` with an optional tail.
/// Colors: vertical gradient derived from `fill` (lighter top, `fill`
/// in the middle, slightly darker bottom), a 1px `border`, and a soft
/// drop shadow underneath.
fn build_callout_sprite(
    body_w: i32,
    body_h: i32,
    g: CalloutGeom,
    tail: Option<Tail>,
    fill: COLORREF,
    border: COLORREF,
    opacity: u8,
) -> Sprite {
    // Whole sprite (fill, border, edges, shadow) at the badge opacity.
    let op = opacity as f32 / 255.0;
    let dir = tail.map(|t| t.dir);
    let th = g.tail_h;
    let margin = |d: TailDir| if dir == Some(d) { th } else { 0 };
    let body_x = SHADOW_BLUR + margin(TailDir::Left);
    let body_y = SHADOW_BLUR + margin(TailDir::Up);
    let w = body_x + body_w + SHADOW_BLUR + margin(TailDir::Right);
    let h = body_y + body_h + SHADOW_BLUR + SHADOW_OFFSET + margin(TailDir::Down);

    let (bx, by, bw, bh) = (body_x as f32, body_y as f32, body_w as f32, body_h as f32);
    let (hw, hh) = (bw / 2.0, bh / 2.0);
    let (cx, cy) = (bx + hw, by + hh);
    let r = g.radius as f32;
    let half = g.tail_half_w as f32;
    let thf = th as f32;
    // Triangle in sprite space. The tip is at the pixel centre of the
    // aimed-at column/row; the base sinks 2px into the body so the
    // union has no seam.
    let tri = tail.map(|t| {
        let o = t.offset as f32 + 0.5;
        match t.dir {
            TailDir::Up => [
                (bx + o, by - thf),
                (bx + o - half, by + 2.0),
                (bx + o + half, by + 2.0),
            ],
            TailDir::Down => [
                (bx + o, by + bh + thf),
                (bx + o - half, by + bh - 2.0),
                (bx + o + half, by + bh - 2.0),
            ],
            TailDir::Left => [
                (bx - thf, by + o),
                (bx + 2.0, by + o - half),
                (bx + 2.0, by + o + half),
            ],
            TailDir::Right => [
                (bx + bw + thf, by + o),
                (bx + bw - 2.0, by + o - half),
                (bx + bw - 2.0, by + o + half),
            ],
        }
    });

    let white = COLORREF(0x00FF_FFFF);
    let black = COLORREF(0);
    let top = mix_bgr(fill, white, 0.6);
    let mid = mix_bgr(fill, fill, 0.0);
    let bottom = mix_bgr(fill, black, 0.1);
    let border_c = mix_bgr(border, border, 0.0);

    let n = (w * h) as usize;
    let mut shape_a = vec![0f32; n];
    let mut color = vec![[0f32; 3]; n];
    for y in 0..h {
        let py = y as f32 + 0.5;
        let t = ((py - by) / bh).clamp(0.0, 1.0);
        let fill_c: [f32; 3] = if t < 0.5 {
            let k = t * 2.0;
            [0, 1, 2].map(|i| top[i] + (mid[i] - top[i]) * k)
        } else {
            let k = (t - 0.5) * 2.0;
            [0, 1, 2].map(|i| mid[i] + (bottom[i] - mid[i]) * k)
        };
        for x in 0..w {
            let px = x as f32 + 0.5;
            let mut d = sd_round_box(px, py, cx, cy, hw, hh, r);
            if let Some(tri) = &tri {
                d = d.min(sd_triangle(px, py, tri));
            }
            let a = (0.5 - d).clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let core = (0.5 - (d + 1.0)).clamp(0.0, 1.0);
            let i = (y * w + x) as usize;
            shape_a[i] = a;
            // Premultiplied: fill weighted by interior coverage, border
            // by the 1px rim between interior and edge.
            color[i] = [0, 1, 2].map(|c| fill_c[c] * core + border_c[c] * (a - core));
        }
    }

    // Shadow: shape alpha shifted down, separable box blur.
    let mut shadow = vec![0f32; n];
    for y in SHADOW_OFFSET..h {
        for x in 0..w {
            shadow[(y * w + x) as usize] = shape_a[((y - SHADOW_OFFSET) * w + x) as usize];
        }
    }
    let k = (2 * SHADOW_BLUR + 1) as f32;
    let mut tmp = vec![0f32; n];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            for dx in -SHADOW_BLUR..=SHADOW_BLUR {
                let xx = x + dx;
                if (0..w).contains(&xx) {
                    sum += shadow[(y * w + xx) as usize];
                }
            }
            tmp[(y * w + x) as usize] = sum / k;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            for dy in -SHADOW_BLUR..=SHADOW_BLUR {
                let yy = y + dy;
                if (0..h).contains(&yy) {
                    sum += tmp[(yy * w + x) as usize];
                }
            }
            shadow[(y * w + x) as usize] = sum / k * SHADOW_ALPHA;
        }
    }

    let mut px = vec![[0u8; 4]; n];
    let mut spans = Vec::with_capacity(h as usize);
    for y in 0..h {
        let (mut first, mut last) = (w, 0);
        for x in 0..w {
            let i = (y * w + x) as usize;
            let a = shape_a[i];
            // Shape over (black) shadow: the shadow adds alpha only.
            let out_a = a + shadow[i] * (1.0 - a);
            let alpha = (out_a * op * 255.0).round() as u8;
            if alpha == 0 {
                continue;
            }
            let [b, gch, rch] = color[i].map(|c| (c * op).round().clamp(0.0, alpha as f32) as u8);
            px[i] = [b, gch, rch, alpha];
            first = first.min(x);
            last = x + 1;
        }
        spans.push(if first < last { (first, last) } else { (0, 0) });
    }

    Sprite {
        w,
        h,
        body_x,
        body_y,
        px,
        spans,
    }
}

/// Composite `sprite` into the DIB with its top-left at `(x0, y0)`.
/// Opaque pixels are copied; partially transparent ones (anti-aliased
/// edges, shadow) are blended over what's there, so a shadow never
/// punches a hole in a neighbouring badge.
unsafe fn blit_sprite(bits: *mut u8, dib_w: i32, dib_h: i32, sprite: &Sprite, x0: i32, y0: i32) {
    if bits.is_null() {
        return;
    }
    let stride = (dib_w as usize) * 4;
    for sy in 0..sprite.h {
        let y = y0 + sy;
        if y < 0 || y >= dib_h {
            continue;
        }
        let (s0, s1) = sprite.spans[sy as usize];
        let start = s0.max(-x0);
        let end = s1.min(dib_w - x0);
        if start >= end {
            continue;
        }
        let row = bits.add((y as usize) * stride);
        let src_row = &sprite.px[(sy * sprite.w) as usize..((sy + 1) * sprite.w) as usize];
        for sx in start..end {
            let src = src_row[sx as usize];
            let a = src[3];
            if a == 0 {
                continue;
            }
            let dst = row.add(((x0 + sx) as usize) * 4);
            // Copy when opaque or over nothing (the usual case: sprites
            // are translucent at badge opacity but rarely overlap).
            if a == 255 || *dst.add(3) == 0 {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, 4);
            } else {
                let inv = 255 - a as u32;
                for (c, &sc) in src.iter().enumerate() {
                    let d = *dst.add(c) as u32;
                    *dst.add(c) = (sc as u32 + (d * inv + 127) / 255) as u8;
                }
            }
        }
    }
}

/// Benchmark-only access to the overlay's private layout and drawing
/// code. Compiled only with the `bench` cargo feature; not part of the
/// public API. Everything here draws into an in-memory DIB — no HWND,
/// no `UpdateLayeredWindow` — so timings reflect our code, not DWM.
#[cfg(feature = "bench")]
#[doc(hidden)]
pub mod bench {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{SM_CXSCREEN, SM_CYSCREEN};

    /// Make the process per-monitor DPI aware, as the real overlay is.
    /// Must run before any DPI or geometry query, or the first queries
    /// see virtualised 96-DPI values.
    fn ensure_dpi_aware() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }

    /// Primary monitor DPI scale (1.0 = 96 DPI), measured DPI-aware.
    pub fn dpi_scale() -> f32 {
        ensure_dpi_aware();
        primary_monitor_dpi_scale()
    }

    /// Size of the primary monitor in physical pixels. Synthetic hints
    /// are placed on it so `lay_out`'s per-hint monitor query resolves
    /// the same way on every run.
    pub fn primary_size() -> (i32, i32) {
        ensure_dpi_aware();
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }

    /// Tiny deterministic PRNG (LCG) so scenarios are identical run to run.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next() % ((hi - lo).max(1) as u32)) as i32
        }
    }

    fn with_labels(bounds: Vec<Bounds>) -> Vec<Hint> {
        let labels = crate::HintEngine::new(crate::DEFAULT_ALPHABET).generate(bounds.len());
        bounds
            .into_iter()
            .zip(labels)
            .map(|(bounds, label)| Hint {
                bounds,
                label,
                extra: None,
            })
            .collect()
    }

    /// `n` element-sized rects (16–200 px wide, 16–40 px tall) on a
    /// jittered grid across the primary monitor. Dense enough at 300+
    /// that badges collide like they do over a busy app.
    pub fn scattered_hints(n: usize) -> Vec<Hint> {
        let (sw, sh) = primary_size();
        let mut rng = Lcg(0x5EED);
        let cols = ((n as f32 * sw as f32 / sh as f32).sqrt().ceil() as i32).max(1);
        let rows = ((n as i32 + cols - 1) / cols).max(1);
        let cell_w = sw / cols;
        let cell_h = sh / rows;
        let bounds = (0..n as i32)
            .map(|i| {
                let (c, r) = (i % cols, i / cols);
                let w = rng.range(16, 200).min(cell_w.max(17) - 1);
                let h = rng.range(16, 40).min(cell_h.max(17) - 1);
                Bounds {
                    x: c * cell_w + rng.range(0, (cell_w - w).max(1)),
                    y: r * cell_h + rng.range(0, (cell_h - h).max(1)),
                    width: w,
                    height: h,
                }
            })
            .collect();
        with_labels(bounds)
    }

    /// `n` list rows, 300×22 px, stacked in columns down the primary
    /// monitor — the menu / tree-view / file-list case where rows are
    /// shorter than a badge plus gap.
    pub fn dense_list_hints(n: usize) -> Vec<Hint> {
        let (_, sh) = primary_size();
        let rows_per_col = ((sh - 80) / 22).max(1) as usize;
        let bounds = (0..n)
            .map(|i| Bounds {
                x: 40 + (i / rows_per_col) as i32 * 360,
                y: 40 + (i % rows_per_col) as i32 * 22,
                width: 300,
                height: 22,
            })
            .collect();
        with_labels(bounds)
    }

    /// Run only the layout pass. Returns the hint count so the optimizer
    /// can't discard the work.
    pub fn lay_out_only(hints: &[Hint], style: &HintStyle) -> usize {
        lay_out_estimated(hints, style, 0, 0).len()
    }

    /// Off-screen BGRA surface standing in for the overlay DIB.
    pub struct Surface {
        mem_dc: HDC,
        dib: HBITMAP,
        bits: *mut u8,
        width: i32,
        height: i32,
    }

    impl Surface {
        pub fn new(width: i32, height: i32) -> Self {
            let (mem_dc, dib, bits) =
                unsafe { create_dib_surface(width, height).expect("bench DIB surface") };
            Self {
                mem_dc,
                dib,
                bits,
                width,
                height,
            }
        }

        /// Raw premultiplied BGRA pixels, top-down.
        pub fn pixels(&self) -> &[u8] {
            unsafe {
                std::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize)
            }
        }

        /// The full-surface clear `render_to_dib` does every frame.
        pub fn clear(&mut self) {
            unsafe {
                std::ptr::write_bytes(self.bits, 0, (self.width * self.height * 4) as usize);
            }
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.mem_dc);
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
        }
    }

    thread_local! {
        static BENCH_TEXT: RefCell<LabelText> = RefCell::new(LabelText::new());
    }

    /// A laid-out pick, ready to draw repeatedly.
    pub struct Prepared(OverlayState);

    impl Prepared {
        /// Lay out `hints` and rasterise their text, as `run_pick` does at
        /// pick start. Shares one [`LabelText`] per thread, as the
        /// persistent overlay does across picks.
        pub fn new(hints: Vec<Hint>, style: HintStyle) -> Self {
            BENCH_TEXT.with(|t| {
                Self(unsafe { OverlayState::new(hints, style, 0, 0, &mut t.borrow_mut()) })
            })
        }

        /// Badge (and extra pill) rects of every laid hint, in surface
        /// coordinates: `(left, top, right, bottom)`.
        pub fn badge_rects(&self) -> Vec<(i32, i32, i32, i32)> {
            self.0
                .laid
                .iter()
                .map(|h| {
                    let r = h.extra_rect.unwrap_or(h.badge_rect);
                    (h.badge_rect.left, h.badge_rect.top, r.right, r.bottom)
                })
                .collect()
        }

        /// Simulate the user having typed `prefix` (hides non-matching hints).
        pub fn set_typed(&mut self, prefix: &str) {
            self.0.typed = prefix.to_string();
        }

        /// The most common first letter among multi-letter labels: typing
        /// it leaves roughly 1/alphabet of the hints visible.
        pub fn common_prefix(&self) -> String {
            let mut counts: HashMap<char, usize> = HashMap::new();
            for h in self.0.laid.iter().filter(|h| h.label.chars().count() > 1) {
                if let Some(c) = h.label.chars().next() {
                    *counts.entry(c).or_default() += 1;
                }
            }
            counts
                .into_iter()
                .max_by_key(|&(c, n)| (n, std::cmp::Reverse(c)))
                .map(|(c, _)| c.to_string())
                .unwrap_or_default()
        }

        /// How the hints were placed: `[below centred, above centred,
        /// slid, side, no tail]`. Used to judge layout quality.
        pub fn placement_mix(&self) -> [usize; 5] {
            let mut mix = [0; 5];
            for h in &self.0.laid {
                let slot = match h.tail {
                    None => 4,
                    Some(t) => match t.dir {
                        TailDir::Left | TailDir::Right => 3,
                        _ if t.offset != (h.badge_rect.right - h.badge_rect.left) / 2 => 2,
                        TailDir::Up => 0,
                        TailDir::Down => 1,
                    },
                };
                mix[slot] += 1;
            }
            mix
        }

        /// Number of distinct cached callout sprites.
        pub fn sprite_count(&self) -> usize {
            self.0.sprites.len()
        }

        /// Draw every visible hint (no clear, no DWM hand-off).
        pub fn draw(&self, surface: &mut Surface) {
            unsafe { self.0.draw_all(surface.bits, surface.width, surface.height) }
        }
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex() {
        let c = parse_hex_color("#FFE500").unwrap();
        assert_eq!(c.0, 0x0000E5FF);
    }

    #[test]
    fn parses_three_digit_hex() {
        let c = parse_hex_color("#F00").unwrap();
        assert_eq!(c.0, 0x000000FF);
    }

    #[test]
    fn parses_without_hash() {
        let c = parse_hex_color("FFFFFF").unwrap();
        assert_eq!(c.0, 0x00FFFFFF);
    }

    #[test]
    fn rejects_bad_length() {
        assert!(parse_hex_color("#FFFF").is_err());
    }

    #[test]
    fn rejects_non_hex() {
        assert!(parse_hex_color("#GGHHII").is_err());
    }
}

#[cfg(test)]
mod opacity_tests {
    use super::*;

    #[test]
    fn opacity_zero_keeps_default() {
        let mut alpha: u8 = 230;
        apply_opacity_override(&mut alpha, 0);
        assert_eq!(alpha, 230, "0% must mean 'preset default'");
    }

    #[test]
    fn opacity_full_is_max() {
        let mut alpha: u8 = 100;
        apply_opacity_override(&mut alpha, 100);
        assert_eq!(alpha, 255);
    }

    #[test]
    fn opacity_half_is_about_half() {
        let mut alpha: u8 = 0;
        apply_opacity_override(&mut alpha, 50);
        assert!(
            alpha > 120 && alpha < 135,
            "50% should be ~127, got {alpha}"
        );
    }

    #[test]
    fn opacity_over_one_hundred_clamps() {
        let mut alpha: u8 = 0;
        apply_opacity_override(&mut alpha, 250);
        assert_eq!(alpha, 255, "values > 100% must clamp to fully opaque");
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// Element preset pinned to the rectangular shape: these tests cover
    /// the corner-anchor layout. Callout layout lives in `callout_tests`.
    fn element_style() -> HintStyle {
        HintStyle {
            shape: BadgeShape::Rect,
            ..HintStyle::elements()
        }
    }

    fn window_style() -> HintStyle {
        HintStyle::windows()
    }

    fn hint(x: i32, y: i32, w: i32, h: i32, label: &str) -> Hint {
        Hint {
            bounds: Bounds {
                x,
                y,
                width: w,
                height: h,
            },
            label: label.to_string(),
            extra: None,
        }
    }

    #[test]
    fn element_style_places_single_hint_inside_top_left() {
        // v0.4.x: the element style now prefers `TopLeft` so the badge
        // sits inside the top-left corner of the control rather than
        // floating above it. Combined with the smaller font + lower
        // opacity this keeps the rest of the control visible while the
        // user types — the previous OutsideTop placement turned dense
        // screens (multi-monitor + Cursor / VS Code) into a wall of
        // overlapping badges.
        let style = element_style();
        let laid = lay_out_estimated(&[hint(100, 200, 50, 30, "a")], &style, 0, 0);
        assert_eq!(laid[0].badge_rect.left, 100);
        assert_eq!(laid[0].badge_rect.top, 200);
    }

    #[test]
    fn window_style_places_single_hint_inside_top_left() {
        // The window style prefers `TopLeft` because window targets are
        // typically maximized — `OutsideTop` would push the badge above
        // the monitor and make it invisible.
        let style = window_style();
        let laid = lay_out_estimated(&[hint(100, 200, 800, 600, "a")], &style, 0, 0);
        assert_eq!(laid[0].badge_rect.left, 100);
        assert_eq!(laid[0].badge_rect.top, 200);
    }

    #[test]
    fn second_hint_sharing_anchor_picks_alternate_position() {
        // Two hints at the same anchor — the smart-positioning pass should
        // shove the second one to a different corner instead of stacking
        // it directly below. We use the window style here so the test
        // reasons in concrete top-left coordinates without having to
        // subtract the OutsideTop offset.
        let style = window_style();
        let laid = lay_out_estimated(
            &[hint(0, 0, 200, 100, "a"), hint(0, 0, 200, 100, "b")],
            &style,
            0,
            0,
        );
        // First hint takes the top-left.
        assert_eq!(laid[0].badge_rect.left, 0);
        assert_eq!(laid[0].badge_rect.top, 0);
        // Second hint must end up somewhere that doesn't overlap the
        // first — exact corner depends on which candidate frees first,
        // but it should NOT be at (0, 0).
        assert!(
            laid[1].badge_rect.left != 0 || laid[1].badge_rect.top != 0,
            "second hint should pick a non-colliding alternate position"
        );
    }

    #[test]
    fn anchor_for_top_right_subtracts_badge_width() {
        let bounds = Bounds {
            x: 100,
            y: 200,
            width: 300,
            height: 50,
        };
        let (x, _y) = anchor_for(BadgePosition::TopRight, &bounds, 60, 24, 0, 0);
        assert_eq!(x, 100 + 300 - 60);
    }

    #[test]
    fn anchor_for_outside_top_lifts_above() {
        let bounds = Bounds {
            x: 0,
            y: 100,
            width: 50,
            height: 30,
        };
        let (_x, y) = anchor_for(BadgePosition::OutsideTop, &bounds, 40, 24, 0, 0);
        assert!(y < 100, "OutsideTop must place the badge above the element");
    }

    #[test]
    fn anchor_for_outside_bottom_drops_below() {
        let bounds = Bounds {
            x: 0,
            y: 100,
            width: 50,
            height: 30,
        };
        let (_x, y) = anchor_for(BadgePosition::OutsideBottom, &bounds, 40, 24, 0, 0);
        assert!(
            y > 100 + 30,
            "OutsideBottom must place the badge fully below the element"
        );
    }

    #[test]
    fn rect_inside_monitor_accepts_contained_rect() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let inside = RECT {
            left: 100,
            top: 100,
            right: 200,
            bottom: 130,
        };
        assert!(rect_inside_monitor(&inside, &monitor));
    }

    #[test]
    fn rect_inside_monitor_rejects_overlap_left_edge() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let leaks = RECT {
            left: -10,
            top: 100,
            right: 50,
            bottom: 130,
        };
        assert!(!rect_inside_monitor(&leaks, &monitor));
    }

    #[test]
    fn rect_inside_monitor_rejects_overlap_top_edge() {
        // Element at y=0 with OutsideTop placement would push the badge
        // above the monitor — that's exactly the case smart positioning
        // exists to detect.
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let leaks = RECT {
            left: 100,
            top: -30,
            right: 200,
            bottom: 0,
        };
        assert!(!rect_inside_monitor(&leaks, &monitor));
    }

    #[test]
    fn screen_to_client_rect_subtracts_origin() {
        let r = RECT {
            left: 1920,
            top: 0,
            right: 3840,
            bottom: 1080,
        };
        let client = screen_to_client_rect(r, 100, 50);
        assert_eq!(client.left, 1820);
        assert_eq!(client.top, -50);
        assert_eq!(client.right, 3740);
        assert_eq!(client.bottom, 1030);
    }

    #[test]
    fn lay_out_records_target_rect_in_client_space() {
        // origin offset must be subtracted from every target rect so the
        // leader painter doesn't have to know about virtual-desktop coords.
        let laid = lay_out_estimated(&[hint(150, 250, 60, 40, "a")], &element_style(), 100, 200);
        assert_eq!(laid[0].target_rect.left, 50);
        assert_eq!(laid[0].target_rect.top, 50);
        assert_eq!(laid[0].target_rect.right, 110);
        assert_eq!(laid[0].target_rect.bottom, 90);
    }
}

#[cfg(test)]
mod callout_tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{SM_CXSCREEN, SM_CYSCREEN};

    fn style() -> HintStyle {
        HintStyle::elements()
    }

    fn hint(x: i32, y: i32, w: i32, h: i32, label: &str) -> Hint {
        Hint {
            bounds: Bounds {
                x,
                y,
                width: w,
                height: h,
            },
            label: label.to_string(),
            extra: None,
        }
    }

    fn body_w(r: &RECT) -> i32 {
        r.right - r.left
    }

    /// Tip of a laid callout's tail, in client coords.
    fn tip(h: &LaidHint) -> POINT {
        let t = h.tail.expect("callout has a tail");
        let g = style().callout_geom();
        let b = &h.badge_rect;
        match t.dir {
            TailDir::Up => POINT {
                x: b.left + t.offset,
                y: b.top - g.tail_h,
            },
            TailDir::Down => POINT {
                x: b.left + t.offset,
                y: b.bottom + g.tail_h,
            },
            TailDir::Left => POINT {
                x: b.left - g.tail_h,
                y: b.top + t.offset,
            },
            TailDir::Right => POINT {
                x: b.right + g.tail_h,
                y: b.top + t.offset,
            },
        }
    }

    /// Body plus tail, as `lay_out` reserves it.
    fn bbox(h: &LaidHint) -> RECT {
        let g = style().callout_geom();
        let mut r = h.badge_rect;
        match h.tail.map(|t| t.dir) {
            Some(TailDir::Up) => r.top -= g.tail_h,
            Some(TailDir::Down) => r.bottom += g.tail_h,
            Some(TailDir::Left) => r.left -= g.tail_h,
            Some(TailDir::Right) => r.right += g.tail_h,
            None => {}
        }
        r
    }

    fn assert_no_overlap(laid: &[LaidHint]) {
        for (i, a) in laid.iter().enumerate() {
            for b in &laid[i + 1..] {
                assert!(
                    !rects_intersect(&bbox(a), &bbox(b)),
                    "callouts for {} and {} overlap",
                    a.label,
                    b.label
                );
            }
        }
    }

    #[test]
    fn elements_default_to_callout_and_windows_to_rect() {
        assert_eq!(HintStyle::elements().shape, BadgeShape::Callout);
        assert_eq!(HintStyle::windows().shape, BadgeShape::Rect);
    }

    #[test]
    fn config_shape_overrides_preset() {
        let c = crate::config::BadgeColors {
            shape: "rect".into(),
            ..Default::default()
        };
        assert_eq!(HintStyle::elements_from_config(&c).shape, BadgeShape::Rect);
        let c = crate::config::BadgeColors {
            shape: "Callout".into(),
            ..Default::default()
        };
        assert_eq!(
            HintStyle::windows_from_config(&c).shape,
            BadgeShape::Callout
        );
        let c = crate::config::BadgeColors {
            shape: "hexagon".into(),
            ..Default::default()
        };
        assert_eq!(
            HintStyle::elements_from_config(&c).shape,
            BadgeShape::Callout
        );
    }

    #[test]
    fn single_callout_sits_below_cutting_into_bottom_centre() {
        let g = style().callout_geom();
        assert!(g.cut_in > 0);
        let laid = lay_out_estimated(&[hint(400, 300, 60, 30, "a")], &style(), 0, 0);
        let h = &laid[0];
        let t = h.tail.expect("tail");
        assert_eq!(t.dir, TailDir::Up);
        assert_eq!(t.offset, body_w(&h.badge_rect) / 2, "centred");
        assert_eq!(h.badge_rect.top, 330 - g.cut_in + g.tail_h);
        assert_eq!(
            tip(h),
            POINT {
                x: 430,
                y: 330 - g.cut_in
            }
        );
    }

    #[test]
    fn cut_in_is_capped_for_tiny_elements() {
        // A 6px-tall control: the tip may reach at most 2px inside.
        let laid = lay_out_estimated(&[hint(400, 300, 60, 6, "a")], &style(), 0, 0);
        assert_eq!(tip(&laid[0]), POINT { x: 430, y: 304 });
    }

    #[test]
    fn callout_flips_above_at_monitor_bottom() {
        let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let g = style().callout_geom();
        let laid = lay_out_estimated(&[hint(400, sh - 30, 60, 30, "a")], &style(), 0, 0);
        let h = &laid[0];
        assert_eq!(h.tail.map(|t| t.dir), Some(TailDir::Down));
        assert_eq!(h.badge_rect.bottom, sh - 30 + g.cut_in - g.tail_h);
        assert_eq!(
            tip(h),
            POINT {
                x: 430,
                y: sh - 30 + g.cut_in
            }
        );
    }

    #[test]
    fn callout_slides_at_monitor_left_edge_and_keeps_aim() {
        // Centre x == inner: a centred body would cross x = 0, but
        // sliding so the tip sits at the body's inner left edge fits.
        let g = style().callout_geom();
        let inner = g.inner();
        let laid = lay_out_estimated(&[hint(0, 300, 2 * inner, 30, "a")], &style(), 0, 0);
        let h = &laid[0];
        let t = h.tail.expect("tail");
        assert_eq!(t.dir, TailDir::Up);
        assert_eq!(t.offset, inner);
        assert_eq!(h.badge_rect.left, 0);
        assert_eq!(
            tip(h),
            POINT {
                x: inner,
                y: 330 - g.cut_in
            }
        );
    }

    #[test]
    fn crowded_row_keeps_every_tail_aimed_without_overlap() {
        // Three buttons spaced so the first takes "below", the second
        // "above", and the third collides with both centred spots but
        // fits once slid sideways.
        let g = style().callout_geom();
        let glyph_w = ((style().font_height as f32) * 0.62) as i32;
        let w = g.body_width(2 * glyph_w);
        let d = (w / 2 + g.inner() + 1) / 2;
        assert!(2 * d < w, "spacing must still collide when centred");
        let hints: Vec<Hint> = (0..3)
            .map(|i| hint(400 + i * d, 300, 16, 16, &format!("a{i}")))
            .collect();
        let laid = lay_out_estimated(&hints, &style(), 0, 0);
        assert_no_overlap(&laid);
        let cut = g.cut_in.min(16 / 3);
        for (h, src) in laid.iter().zip(&hints) {
            let p = tip(h);
            let cx = src.bounds.x + src.bounds.width / 2;
            match h.tail.unwrap().dir {
                TailDir::Up => assert_eq!(
                    p,
                    POINT {
                        x: cx,
                        y: 316 - cut
                    }
                ),
                TailDir::Down => assert_eq!(
                    p,
                    POINT {
                        x: cx,
                        y: 300 + cut
                    }
                ),
                _ => {}
            }
        }
        assert!(
            laid.iter()
                .any(|h| h.tail.unwrap().offset != body_w(&h.badge_rect) / 2),
            "at least one callout had to slide"
        );
    }

    #[test]
    fn short_list_rows_fall_back_to_side_callouts() {
        // Rows shorter than a callout: below/above run out quickly, so
        // later rows point sideways at their row.
        let g = style().callout_geom();
        let hints: Vec<Hint> = (0..3)
            .map(|i| hint(300, 300 + i * 10, 200, 10, &format!("{i}")))
            .collect();
        let laid = lay_out_estimated(&hints, &style(), 0, 0);
        assert_no_overlap(&laid);
        assert_eq!(laid[0].tail.map(|t| t.dir), Some(TailDir::Up));
        let side: Vec<usize> = (0..3)
            .filter(|&i| {
                matches!(
                    laid[i].tail.map(|t| t.dir),
                    Some(TailDir::Left) | Some(TailDir::Right)
                )
            })
            .collect();
        assert!(!side.is_empty(), "some row needed a side callout");
        for i in side {
            let p = tip(&laid[i]);
            assert_eq!(
                p.y,
                305 + 10 * i as i32,
                "aims at the row's vertical centre"
            );
            assert!(
                p.x == 300 + g.cut_in || p.x == 500 - g.cut_in,
                "tip cuts into the row's near edge, got {}",
                p.x
            );
        }
    }

    #[test]
    fn hints_with_extra_text_keep_rect_layout() {
        let mut h = hint(400, 300, 60, 30, "a");
        h.extra = Some("title".into());
        let laid = lay_out_estimated(&[h], &style(), 0, 0);
        assert!(laid[0].tail.is_none());
        assert!(!laid[0].callout, "drawn (and outlined) as a rect badge");
        assert!(laid[0].extra_rect.is_some());
    }

    #[test]
    fn layout_works_on_any_primary_monitor_size() {
        // Smoke test: a scattered grid lays out with no overlaps.
        let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let hints: Vec<Hint> = (0..60)
            .map(|i| {
                hint(
                    (i % 10) * (sw / 10),
                    100 + (i / 10) * 40,
                    30,
                    20,
                    &format!("{i}"),
                )
            })
            .collect();
        let laid = lay_out_estimated(&hints, &style(), 0, 0);
        let tailed: Vec<LaidHint> = laid.into_iter().filter(|h| h.tail.is_some()).collect();
        assert_no_overlap(&tailed);
    }

    fn sprite(tail: Option<Tail>) -> Sprite {
        let g = style().callout_geom();
        build_callout_sprite(
            24,
            g.body_h,
            g,
            tail,
            COLORREF(0x0000E5FF),
            COLORREF(0x00202020),
            255,
        )
    }

    fn pixel(s: &Sprite, x: i32, y: i32) -> [u8; 4] {
        s.px[(y * s.w + x) as usize]
    }

    #[test]
    fn sprite_is_opaque_inside_and_clear_outside() {
        let g = style().callout_geom();
        let s = sprite(Some(Tail {
            dir: TailDir::Up,
            offset: 12,
        }));
        assert_eq!(pixel(&s, s.body_x + 12, s.body_y + g.body_h / 2)[3], 255);
        assert_eq!(pixel(&s, 0, 0)[3], 0, "margin corner is transparent");
        assert!(
            pixel(&s, s.body_x, s.body_y)[3] < 128,
            "rounded corner is cut away"
        );
        assert!(
            pixel(&s, s.body_x + 12, s.body_y - g.tail_h)[3] > 0,
            "tail reaches its tip"
        );
        assert_eq!(
            pixel(&s, s.body_x + 2, s.body_y - g.tail_h)[3],
            0,
            "tail is narrow at the tip"
        );
    }

    #[test]
    fn sprite_pixels_are_premultiplied_and_spans_cover_them() {
        for dir in [TailDir::Up, TailDir::Down, TailDir::Left, TailDir::Right] {
            let s = sprite(Some(Tail { dir, offset: 9 }));
            for y in 0..s.h {
                let (a, b) = s.spans[y as usize];
                for x in 0..s.w {
                    let p = pixel(&s, x, y);
                    assert!(
                        p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3],
                        "{dir:?} ({x},{y})"
                    );
                    if p[3] > 0 {
                        assert!(a <= x && x < b, "{dir:?} span misses ({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn pixel_helpers_write_colorref_in_dib_channel_order() {
        // #FFE500 yellow: COLORREF 0x0000E5FF must land as B=0, G=E5, R=FF.
        let yellow = parse_hex_color("#FFE500").unwrap();
        let mut buf = vec![0u8; 4 * 4 * 4];
        unsafe {
            put_pixel_argb(buf.as_mut_ptr(), 4, 4, 0, 0, premul(yellow, 255));
            let r = RECT {
                left: 1,
                top: 1,
                right: 2,
                bottom: 2,
            };
            fill_rect_argb(buf.as_mut_ptr(), 4, 4, &r, premul(yellow, 255));
        }
        assert_eq!(&buf[0..4], &[0x00, 0xE5, 0xFF, 0xFF]);
        let i = (4 + 1) * 4;
        assert_eq!(&buf[i..i + 4], &[0x00, 0xE5, 0xFF, 0xFF]);
    }

    #[test]
    fn callouts_never_outline_targets() {
        // A callout with no room for a tail must still not get a target
        // outline; a rect badge (e.g. an extra-pill hint in callout
        // mode) still does.
        let hint = |callout: bool| LaidHint {
            label: "a".into(),
            extra: None,
            badge_rect: RECT {
                left: 2,
                top: 2,
                right: 10,
                bottom: 8,
            },
            extra_rect: None,
            target_rect: RECT {
                left: 1,
                top: 1,
                right: 30,
                bottom: 30,
            },
            callout,
            tail: None,
        };
        let outline_at_corner = |callout: bool| {
            let mut text = LabelText::new();
            let state =
                unsafe { OverlayState::new(Vec::new(), HintStyle::elements(), 0, 0, &mut text) };
            let mut buf = vec![0u8; 32 * 32 * 4];
            unsafe { state.draw_leader_dib(buf.as_mut_ptr(), 32, 32, &hint(callout)) };
            // Bottom-right corner of the target frame, away from any arrow.
            let i = (29 * 32 + 29) * 4;
            buf[i + 3]
        };
        assert_eq!(outline_at_corner(true), 0);
        // Outlines carry the badge opacity, like the badge.
        assert_eq!(
            outline_at_corner(false),
            HintStyle::elements().badge_opacity
        );
    }

    #[test]
    fn sprite_has_soft_shadow_below_body() {
        let g = style().callout_geom();
        let s = sprite(None);
        let below = pixel(&s, s.body_x + 12, s.body_y + g.body_h + 1);
        assert!(
            below[3] > 0 && below[3] < 80,
            "faint shadow, got {}",
            below[3]
        );
        assert_eq!(&below[..3], &[0, 0, 0], "shadow is black");
    }

    #[test]
    fn blit_blends_edges_over_existing_pixels() {
        let s = sprite(None);
        let (w, h) = (s.w + 4, s.h + 4);
        // Opaque blue background.
        let mut buf: Vec<u8> = [255u8, 0, 0, 255].repeat((w * h) as usize);
        unsafe { blit_sprite(buf.as_mut_ptr(), w, h, &s, 2, 2) };
        for p in buf.chunks(4) {
            assert_eq!(p[3], 255, "stays opaque over an opaque background");
            assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
        }
        // Body centre is the sprite's yellow (B=0, R=255 for #FFE500),
        // not the blue background.
        let mid_row = 2 + s.body_y + style().callout_geom().body_h / 2;
        let i = ((mid_row * w + 2 + s.body_x + 12) * 4) as usize;
        assert!(buf[i] < 20, "blue channel, got {}", buf[i]);
        assert!(buf[i + 2] > 200, "red channel");
    }

    #[test]
    fn blit_clips_at_buffer_edges() {
        let s = sprite(None);
        let mut buf = vec![0u8; 10 * 10 * 4];
        unsafe {
            blit_sprite(buf.as_mut_ptr(), 10, 10, &s, -5, -5);
            blit_sprite(buf.as_mut_ptr(), 10, 10, &s, 8, 8);
        }
    }

    #[test]
    fn premul_scales_channels_by_alpha() {
        let yellow = COLORREF(0x0000E5FF);
        assert_eq!(premul(yellow, 255), [0x00, 0xE5, 0xFF, 0xFF]);
        assert_eq!(premul(yellow, 0), [0, 0, 0, 0]);
        assert_eq!(premul(yellow, 128), [0x00, 0x73, 0x80, 0x80]);
    }

    #[test]
    fn sprite_opacity_scales_alpha_and_stays_premultiplied() {
        let g = style().callout_geom();
        let full = sprite(None);
        let half = build_callout_sprite(
            24,
            g.body_h,
            g,
            None,
            COLORREF(0x0000E5FF),
            COLORREF(0x00202020),
            128,
        );
        let (x, y) = (full.body_x + 12, full.body_y + g.body_h / 2);
        assert_eq!(pixel(&half, x, y)[3], 128);
        for (p, q) in half.px.iter().zip(&full.px) {
            assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
            assert!(p[3] <= q[3]);
        }
    }

    #[test]
    fn coverage_blit_makes_full_ink_opaque_over_translucent_fill() {
        let cov = Coverage {
            w: 3,
            h: 1,
            a: vec![0, 128, 255],
        };
        // Translucent yellow fill (badge at ~76%).
        let fill = premul(COLORREF(0x0000E5FF), 195);
        let mut buf: Vec<u8> = fill.repeat(3);
        unsafe { blit_coverage(buf.as_mut_ptr(), 3, 1, &cov, 0, 0, COLORREF(0)) };
        assert_eq!(&buf[0..4], &fill, "no ink leaves the fill alone");
        assert_eq!(&buf[8..12], &[0, 0, 0, 255], "full ink is opaque black");
        let mid = &buf[4..8];
        assert!(mid[3] > fill[3] && mid[3] < 255, "partial ink raises alpha");
        assert!(mid[2] < fill[2], "partial ink darkens");
        assert!(mid[0] <= mid[3] && mid[1] <= mid[3] && mid[2] <= mid[3]);
    }

    #[test]
    fn coverage_blit_clips_at_buffer_edges() {
        let cov = Coverage {
            w: 4,
            h: 4,
            a: vec![255; 16],
        };
        let mut buf = vec![0u8; 3 * 3 * 4];
        unsafe {
            blit_coverage(buf.as_mut_ptr(), 3, 3, &cov, -2, -2, COLORREF(0x00FF_FFFF));
            blit_coverage(buf.as_mut_ptr(), 3, 3, &cov, 2, 2, COLORREF(0x00FF_FFFF));
        }
        assert_eq!(&buf[0..4], &[255; 4]);
        assert_eq!(&buf[32..36], &[255; 4]);
        assert_eq!(&buf[8..12], &[0; 4]);
    }
}

#[cfg(test)]
mod spatial_grid_tests {
    use super::*;

    fn r(l: i32, t: i32, w: i32, h: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: l + w,
            bottom: t + h,
        }
    }

    #[test]
    fn empty_grid_reports_no_collision() {
        let grid = SpatialGrid::with_capacity(0);
        let placed: Vec<RECT> = Vec::new();
        assert!(!grid.any_intersects(&r(0, 0, 50, 20), &placed));
    }

    #[test]
    fn detects_collision_within_same_cell() {
        let mut grid = SpatialGrid::with_capacity(2);
        let placed = vec![r(0, 0, 30, 30)];
        grid.insert(0, &placed[0]);
        // Candidate overlaps placed[0] inside the same 64-px cell.
        assert!(grid.any_intersects(&r(10, 10, 20, 20), &placed));
    }

    #[test]
    fn ignores_rect_in_unrelated_cell() {
        let mut grid = SpatialGrid::with_capacity(2);
        let placed = vec![r(0, 0, 20, 20)];
        grid.insert(0, &placed[0]);
        // Candidate at (500, 500) — far away, no shared cell.
        assert!(!grid.any_intersects(&r(500, 500, 20, 20), &placed));
    }

    #[test]
    fn dedupes_rects_spanning_multiple_cells() {
        // A wide rect spans cells (0,0) and (1,0). The candidate also
        // spans both cells — without dedupe we'd test the placed rect
        // twice. Correctness is unaffected, but the test pins the
        // optimization in place via a side-channel: a degenerate
        // candidate that fits between two large rects must not falsely
        // report collision because of double-visiting.
        let mut grid = SpatialGrid::with_capacity(2);
        let placed = vec![r(0, 0, 100, 20), r(0, 100, 100, 20)];
        for (i, rect) in placed.iter().enumerate() {
            grid.insert(i, rect);
        }
        // Candidate sits in the gap between the two placed rects but
        // spans the same horizontal cells.
        assert!(!grid.any_intersects(&r(20, 40, 60, 20), &placed));
    }

    #[test]
    fn handles_negative_coordinates() {
        // Multi-monitor setups with the primary in the middle put the
        // virtual desktop's leftmost monitor at negative client coords.
        let mut grid = SpatialGrid::with_capacity(2);
        let placed = vec![r(-100, -50, 40, 40)];
        grid.insert(0, &placed[0]);
        assert!(grid.any_intersects(&r(-90, -40, 20, 20), &placed));
        assert!(!grid.any_intersects(&r(200, 200, 20, 20), &placed));
    }
}

#[cfg(test)]
mod leader_tests {
    use super::*;

    fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    #[test]
    fn endpoints_for_badge_above_target_meet_at_facing_edges() {
        // Badge sits directly above the element with a vertical gap.
        let badge = rect(100, 50, 140, 70);
        let target = rect(100, 100, 200, 200);
        let (start, end) = leader_endpoints(&badge, &target);
        // Start should leave the bottom of the badge…
        assert_eq!(start.y, 70);
        // …and land on the top of the target.
        assert_eq!(end.y, 100);
        // X stays inside the horizontal overlap.
        assert!(start.x >= 100 && start.x <= 140);
        assert!(end.x >= 100 && end.x <= 140);
    }

    #[test]
    fn endpoints_for_badge_inside_target_collapse_axes() {
        // Badge sits in the top-left of the target — overlapping on both
        // axes. The painter skips drawing in this case, but
        // `leader_endpoints` should still return non-degenerate points
        // for any caller that asks (no panics, in particular).
        let badge = rect(100, 100, 140, 120);
        let target = rect(100, 100, 300, 300);
        let (start, end) = leader_endpoints(&badge, &target);
        // Both x and y axes overlap, so start == end (zero-length line).
        assert_eq!(start.x, end.x);
        assert_eq!(start.y, end.y);
    }

    #[test]
    fn arrowhead_tip_lands_on_end_point() {
        let start = POINT { x: 0, y: 0 };
        let end = POINT { x: 100, y: 0 };
        let head = arrowhead_polygon(start, end);
        assert_eq!(head[0].x, 100);
        assert_eq!(head[0].y, 0);
        // The other two points form the back of the triangle, behind the tip.
        assert!(head[1].x < 100);
        assert!(head[2].x < 100);
    }

    #[test]
    fn arrowhead_for_diagonal_keeps_tip_at_end() {
        let start = POINT { x: 0, y: 0 };
        let end = POINT { x: 30, y: 40 };
        let head = arrowhead_polygon(start, end);
        assert_eq!(head[0].x, 30);
        assert_eq!(head[0].y, 40);
    }
}
