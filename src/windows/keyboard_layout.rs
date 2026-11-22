//! Windows side of the keyboard model: capture the active OS layout as a
//! [`Layout`], and translate keystrokes to characters.
//!
//! Notes from probing real layouts (US, Dvorak, German, French, Russian,
//! Japanese, Greek, Hebrew):
//!
//! - `ToUnicodeEx` with flag `0x4` doesn't touch the kernel keyboard state,
//!   so probing never corrupts a pending dead key in the user's session.
//!   Dead keys (German `^ ´`, French `^`, Greek `΄`) return a negative
//!   count and are skipped — they can't be a one-press hint key.
//! - Japanese/Chinese input methods sit on a plain QWERTY base layout, so
//!   they resolve like US.
//! - Consolas (the overlay font) covers Latin, Greek and Cyrillic but not
//!   Hebrew/Arabic/Thai. Characters without a glyph are dropped from the
//!   detected layout, which makes presets fall back to the QWERTY
//!   character on that key; [`key_chars`] then accepts the QWERTY
//!   character for the same physical key so those hints stay typeable.
//! - Windows updates a background thread's layout lazily, so jumpkey's own
//!   thread can report a stale layout at hotkey time. We read the layout
//!   of the *foreground window's* thread instead (the app the user is
//!   typing in) via [`capture_active_layout`], and keystroke translation
//!   in the overlay reuses that captured layout, so labels and input
//!   always agree.

use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, GetGlyphIndicesW, SelectObject,
    GGI_MARK_NONEXISTING_GLYPHS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, MapVirtualKeyExW, ToUnicodeEx, HKL, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP,
};

use std::sync::atomic::{AtomicIsize, Ordering};

/// Layout captured by the last [`capture_active_layout`] call. `0` =
/// nothing captured yet. Process-wide so the Settings window and the
/// overlay agree no matter which thread they run on.
static CAPTURED_HKL: AtomicIsize = AtomicIsize::new(0);

use crate::input::{Key, NamedKey};
use crate::keyboard::{Layout, PhysKey, Row};

/// `ToUnicodeEx` flag: don't change keyboard state (Windows 10 1607+).
const TU_NO_STATE_CHANGE: u32 = 0x4;

/// Set-1 scan code → physical position for the main block.
const SCANCODES: &[(u32, PhysKey)] = &{
    const fn k(row: Row, col: i8) -> PhysKey {
        PhysKey::new(row, col)
    }
    use Row::*;
    [
        (0x29, k(Number, -1)),
        (0x02, k(Number, 0)),
        (0x03, k(Number, 1)),
        (0x04, k(Number, 2)),
        (0x05, k(Number, 3)),
        (0x06, k(Number, 4)),
        (0x07, k(Number, 5)),
        (0x08, k(Number, 6)),
        (0x09, k(Number, 7)),
        (0x0A, k(Number, 8)),
        (0x0B, k(Number, 9)),
        (0x0C, k(Number, 10)),
        (0x0D, k(Number, 11)),
        (0x10, k(Top, 0)),
        (0x11, k(Top, 1)),
        (0x12, k(Top, 2)),
        (0x13, k(Top, 3)),
        (0x14, k(Top, 4)),
        (0x15, k(Top, 5)),
        (0x16, k(Top, 6)),
        (0x17, k(Top, 7)),
        (0x18, k(Top, 8)),
        (0x19, k(Top, 9)),
        (0x1A, k(Top, 10)),
        (0x1B, k(Top, 11)),
        (0x2B, k(Top, 12)),
        (0x1E, k(Home, 0)),
        (0x1F, k(Home, 1)),
        (0x20, k(Home, 2)),
        (0x21, k(Home, 3)),
        (0x22, k(Home, 4)),
        (0x23, k(Home, 5)),
        (0x24, k(Home, 6)),
        (0x25, k(Home, 7)),
        (0x26, k(Home, 8)),
        (0x27, k(Home, 9)),
        (0x28, k(Home, 10)),
        (0x2C, k(Bottom, 0)),
        (0x2D, k(Bottom, 1)),
        (0x2E, k(Bottom, 2)),
        (0x2F, k(Bottom, 3)),
        (0x30, k(Bottom, 4)),
        (0x31, k(Bottom, 5)),
        (0x32, k(Bottom, 6)),
        (0x33, k(Bottom, 7)),
        (0x34, k(Bottom, 8)),
        (0x35, k(Bottom, 9)),
        // ISO-only key left of Z; listed last so the ANSI `\` key wins
        // when both produce the same character.
        (0x56, k(Bottom, -1)),
    ]
};

/// Physical position of a set-1 scan code, if it's on the main block.
pub fn phys_for_scancode(sc: u32) -> Option<PhysKey> {
    SCANCODES.iter().find(|(s, _)| *s == sc).map(|(_, k)| *k)
}

/// Platform-neutral [`Key`] for a set-1 scan code. `extended` is the
/// `E0` prefix (`LLKHF_EXTENDED` in a low-level hook), which tells the
/// arrows, Home/End, Delete and right Ctrl/Alt apart from the numpad and
/// left keys sharing their codes. Unknown keys become [`Key::Other`] with
/// `0xE000 | sc` for extended codes.
pub fn key_from_scancode(sc: u32, extended: bool) -> Key {
    use NamedKey::*;
    let named = match (sc, extended) {
        (0x01, false) => Some(Esc),
        (0x0F, false) => Some(Tab),
        (0x1C, _) => Some(Enter), // main Enter, or numpad Enter (extended)
        (0x0E, false) => Some(Backspace),
        (0x39, false) => Some(Space),
        (0x48, true) => Some(Up),
        (0x50, true) => Some(Down),
        (0x4B, true) => Some(Left),
        (0x4D, true) => Some(Right),
        (0x47, true) => Some(Home),
        (0x4F, true) => Some(End),
        (0x49, true) => Some(PageUp),
        (0x51, true) => Some(PageDown),
        (0x53, true) => Some(Delete),
        (0x2A | 0x36, false) => Some(Shift),
        (0x1D, _) => Some(Ctrl),
        (0x38, _) => Some(Alt),
        (0x5B | 0x5C, true) => Some(Meta),
        (0x3B..=0x44, false) => Some(F((sc - 0x3B + 1) as u8)),
        (0x57, false) => Some(F(11)),
        (0x58, false) => Some(F(12)),
        (0x64..=0x6E, false) => Some(F((sc - 0x64 + 13) as u8)),
        (0x76, false) => Some(F(24)),
        _ => None,
    };
    if let Some(n) = named {
        return Key::Named(n);
    }
    if !extended {
        if let Some(p) = phys_for_scancode(sc) {
            return Key::Phys(p);
        }
    }
    Key::Other(if extended {
        0xE000 | sc as u16
    } else {
        sc as u16
    })
}

/// [`Key`] and "is it a press" for one `WH_KEYBOARD_LL` event. Input sent
/// with a virtual-key code only (`SendInput` without a scan code) arrives
/// with scan code 0; the scan code is then looked up from the VK.
pub fn key_from_ll(info: &KBDLLHOOKSTRUCT) -> (Key, bool) {
    let down = (info.flags.0 & LLKHF_UP.0) == 0;
    let mut sc = info.scanCode;
    let mut extended = (info.flags.0 & LLKHF_EXTENDED.0) != 0;
    if sc == 0 {
        let mapped = unsafe { MapVirtualKeyExW(info.vkCode, MAPVK_VK_TO_VSC_EX, None) };
        sc = mapped & 0xFF;
        extended = (mapped >> 8) == 0xE0 || (mapped >> 8) == 0xE1;
    }
    (key_from_scancode(sc, extended), down)
}

/// Read the keyboard layout of the foreground window's thread (falling
/// back to this thread's), remember it for [`detect`] and [`key_chars`],
/// and return its opaque id for cheap "did the layout change?" checks.
/// Call this right before showing an overlay.
pub fn capture_active_layout() -> isize {
    let id = unsafe {
        let fg = GetForegroundWindow();
        let tid = if fg.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let hkl = GetKeyboardLayout(tid);
        if hkl.0.is_null() {
            GetKeyboardLayout(0)
        } else {
            hkl
        }
    }
    .0 as isize;
    CAPTURED_HKL.store(id, Ordering::Relaxed);
    id
}

/// Id of the most recently captured layout, capturing now if nothing was
/// captured yet. Unlike [`capture_active_layout`] this doesn't re-read the
/// foreground window, so it's safe to call while one of jumpkey's own
/// windows (Settings) is in front — Windows may report a stale layout
/// for those.
pub fn last_layout_id() -> isize {
    match CAPTURED_HKL.load(Ordering::Relaxed) {
        0 => capture_active_layout(),
        id => id,
    }
}

/// The captured layout, capturing now if nothing was captured yet.
fn input_hkl() -> HKL {
    HKL(last_layout_id() as *mut core::ffi::c_void)
}

/// Read the captured Windows keyboard layout (see
/// [`capture_active_layout`]) as a [`Layout`]. Returns `None` only if the
/// layout produced no usable characters at all.
pub fn detect() -> Option<Layout> {
    unsafe {
        let hkl = input_hkl();
        let glyphs = GlyphProbe::new();
        let pairs: Vec<(PhysKey, char)> = SCANCODES
            .iter()
            .filter_map(|(sc, key)| {
                let c = unshifted_char(hkl, *sc)?;
                glyphs.has_glyph(c).then_some((*key, c))
            })
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(Layout::from_pairs(pairs))
        }
    }
}

/// Characters a key press can stand for: what the OS layout types for it
/// (unshifted, so Shift/Ctrl stay free as pick modifiers), and the
/// QWERTY character on the same physical key. The overlay accepts either,
/// so hints that fell back to QWERTY characters (unrenderable glyphs)
/// remain typeable.
pub fn key_chars(vk: u32, scancode: u32) -> (Option<char>, Option<char>) {
    let os = unsafe {
        let hkl = input_hkl();
        let mut buf = [0u16; 4];
        let state = [0u8; 256];
        let r = ToUnicodeEx(vk, scancode, &state, &mut buf, TU_NO_STATE_CHANGE, hkl);
        single_char(r, &buf)
    };
    let qwerty = phys_for_scancode(scancode).and_then(|k| Layout::qwerty_ref().char_at(k));
    (os, qwerty)
}

unsafe fn unshifted_char(hkl: HKL, sc: u32) -> Option<char> {
    let vk = MapVirtualKeyExW(sc, MAPVK_VSC_TO_VK_EX, hkl);
    if vk == 0 {
        return None;
    }
    let mut buf = [0u16; 4];
    let state = [0u8; 256];
    let r = ToUnicodeEx(vk, sc, &state, &mut buf, TU_NO_STATE_CHANGE, hkl);
    single_char(r, &buf)
}

/// Exactly one printable character, lowercased; dead keys (`r < 0`),
/// nothing (`r == 0`) and ligatures (`r > 1`) are rejected.
fn single_char(r: i32, buf: &[u16]) -> Option<char> {
    if r != 1 {
        return None;
    }
    let c = char::from_u32(buf[0] as u32)?;
    if c.is_control() || c.is_whitespace() {
        return None;
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => Some(l),
        _ => Some(c),
    }
}

/// A memory DC with the overlay font selected, for glyph-coverage checks.
struct GlyphProbe {
    dc: windows::Win32::Graphics::Gdi::HDC,
    font: windows::Win32::Graphics::Gdi::HFONT,
}

impl GlyphProbe {
    unsafe fn new() -> Self {
        let dc = CreateCompatibleDC(None);
        let font = CreateFontW(20, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 0, 0, w!("Consolas"));
        SelectObject(dc, font);
        Self { dc, font }
    }

    /// True when the overlay font can draw `c`. If the probe couldn't be
    /// created we optimistically say yes rather than drop every key.
    unsafe fn has_glyph(&self, c: char) -> bool {
        if self.dc.is_invalid() {
            return true;
        }
        let mut utf16 = [0u16; 2];
        let encoded = c.encode_utf16(&mut utf16);
        let len = encoded.len();
        let mut idx = [0u16; 2];
        let r = GetGlyphIndicesW(
            self.dc,
            PCWSTR(utf16.as_ptr()),
            len as i32,
            idx.as_mut_ptr(),
            GGI_MARK_NONEXISTING_GLYPHS,
        );
        r != u32::MAX && !idx[..len].contains(&0xFFFF)
    }
}

impl Drop for GlyphProbe {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font);
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancode_table_covers_every_qwerty_key() {
        let q = Layout::qwerty();
        for c in "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./".chars() {
            let key = q.key_of(c).unwrap();
            assert!(
                SCANCODES.iter().any(|(_, k)| *k == key),
                "no scan code for {c}"
            );
        }
    }

    #[test]
    fn single_char_rejects_dead_keys_and_ligatures() {
        assert_eq!(single_char(-1, &[b'^' as u16]), None);
        assert_eq!(single_char(0, &[0]), None);
        assert_eq!(single_char(2, &[b'a' as u16, b'b' as u16]), None);
        assert_eq!(single_char(1, &[b'A' as u16]), Some('a'));
        assert_eq!(single_char(1, &[b' ' as u16]), None);
    }

    #[test]
    fn scancodes_map_to_named_keys() {
        use NamedKey::*;
        let cases = [
            (0x01, false, Esc),
            (0x0F, false, Tab),
            (0x1C, false, Enter),
            (0x1C, true, Enter),
            (0x0E, false, Backspace),
            (0x39, false, Space),
            (0x48, true, Up),
            (0x50, true, Down),
            (0x4B, true, Left),
            (0x4D, true, Right),
            (0x47, true, Home),
            (0x4F, true, End),
            (0x49, true, PageUp),
            (0x51, true, PageDown),
            (0x53, true, Delete),
            (0x2A, false, Shift),
            (0x36, false, Shift),
            (0x1D, false, Ctrl),
            (0x1D, true, Ctrl),
            (0x38, false, Alt),
            (0x38, true, Alt),
            (0x5B, true, Meta),
            (0x5C, true, Meta),
            (0x3B, false, F(1)),
            (0x44, false, F(10)),
            (0x57, false, F(11)),
            (0x58, false, F(12)),
            (0x64, false, F(13)),
            (0x6E, false, F(23)),
            (0x76, false, F(24)),
        ];
        for (sc, ext, want) in cases {
            assert_eq!(
                key_from_scancode(sc, ext),
                Key::Named(want),
                "{sc:#x} ext={ext}"
            );
        }
    }

    #[test]
    fn extended_flag_separates_numpad_from_navigation() {
        // Numpad 8 / 2 / 4 / 6 share codes with the arrows.
        assert_eq!(key_from_scancode(0x48, false), Key::Other(0x48));
        assert_eq!(key_from_scancode(0x48, true), Key::Named(NamedKey::Up));
        assert_eq!(key_from_scancode(0x53, false), Key::Other(0x53));
        // Extended codes that aren't named keep the E0 marker.
        assert_eq!(key_from_scancode(0x5D, true), Key::Other(0xE05D));
    }

    #[test]
    fn main_block_maps_to_physical_keys() {
        assert_eq!(
            key_from_scancode(0x24, false),
            Key::Phys(PhysKey::new(Row::Home, 6))
        );
        assert_eq!(
            key_from_scancode(0x02, false),
            Key::Phys(PhysKey::new(Row::Number, 0))
        );
        // Every scancode in the table is a physical key, never a named one.
        for (sc, k) in SCANCODES {
            assert_eq!(key_from_scancode(*sc, false), Key::Phys(*k));
        }
    }

    #[test]
    fn key_chars_reports_qwerty_char_for_physical_key() {
        // VK 0 forces the OS translation to fail; the QWERTY fallback is
        // purely table-driven.
        let (_, qwerty) = key_chars(0, 0x1E);
        assert_eq!(qwerty, Some('a'));
    }
}
