//! Platform-neutral key events: what a low-level keyboard hook (Windows
//! `WH_KEYBOARD_LL`, a macOS event tap, …) delivers to the core.
//!
//! The platform layer turns its raw codes into a [`Key`] plus "down or
//! up" (on Windows: `crate::windows::keyboard_layout::key_from_scancode`),
//! and a [`KeyTracker`] adds the held modifiers and the auto-repeat flag.
//! Modes such as scroll mode (#54) and the state machine consume the
//! resulting [`KeyEvent`]s and never see a platform code.
//!
//! Keys are **physical positions** where possible ([`Key::Phys`]), so
//! bindings like `h j k l` and the digits stay put on Dvorak, Colemak or
//! a non-Latin layout. Characters are a separate concern (the hint typing
//! path translates through the active layout).

use std::collections::HashSet;

use crate::keyboard::PhysKey;

/// A key, independent of platform and layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A key on the main block (letters, digits, punctuation), by position.
    Phys(PhysKey),
    /// A named key outside the main block, or a modifier.
    Named(NamedKey),
    /// Anything else (numpad, media keys, …), with the platform's raw code
    /// so it can still be passed through or logged.
    Other(u16),
}

/// Keys outside the main block that modes bind to. Left and right
/// modifiers are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum NamedKey {
    Esc,
    Tab,
    Enter,
    Backspace,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// Function key `F1`–`F24`.
    F(u8),
    Shift,
    Ctrl,
    Alt,
    /// Windows key / Command / Super.
    Meta,
}

/// Modifiers held, left and right merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[allow(missing_docs)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Mods {
    /// No modifier held.
    pub const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };

    /// True when no modifier is held.
    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }

    /// The flag a modifier key controls, if `key` is one.
    fn slot(&mut self, key: Key) -> Option<&mut bool> {
        match key {
            Key::Named(NamedKey::Shift) => Some(&mut self.shift),
            Key::Named(NamedKey::Ctrl) => Some(&mut self.ctrl),
            Key::Named(NamedKey::Alt) => Some(&mut self.alt),
            Key::Named(NamedKey::Meta) => Some(&mut self.meta),
            _ => None,
        }
    }
}

impl Key {
    /// True for Shift, Ctrl, Alt and Meta.
    pub fn is_modifier(&self) -> bool {
        Mods::default().slot(*self).is_some()
    }
}

/// One key press or release, as a mode sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Which key.
    pub key: Key,
    /// Modifiers held at this event, including this event's own key when
    /// it is a modifier being pressed.
    pub mods: Mods,
    /// `true` for a press (or auto-repeat), `false` for a release.
    pub down: bool,
    /// A press of a key that was already down (keyboard auto-repeat).
    pub repeat: bool,
}

/// Turns raw `(key, down)` pairs into [`KeyEvent`]s by remembering which
/// keys are held. Low-level hooks report neither repeat nor modifier
/// state reliably, so the core tracks both.
///
/// Because left and right modifiers are merged, releasing either Shift
/// clears `shift` even if the other is still held; that's rare enough to
/// accept. A release without a press (the press happened before the hook
/// was installed) is reported normally and changes nothing.
#[derive(Debug, Default, Clone)]
pub struct KeyTracker {
    held: HashSet<Key>,
    mods: Mods,
}

impl KeyTracker {
    /// A tracker that assumes nothing is held.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tracker seeded with modifiers already held when the hook started
    /// (e.g. the hotkey's own Ctrl+Alt still down).
    pub fn with_mods(mods: Mods) -> Self {
        Self {
            held: HashSet::new(),
            mods,
        }
    }

    /// Record one raw event and return it with modifiers and repeat filled
    /// in.
    pub fn track(&mut self, key: Key, down: bool) -> KeyEvent {
        let repeat = if down {
            !self.held.insert(key)
        } else {
            self.held.remove(&key);
            false
        };
        if let Some(flag) = self.mods.slot(key) {
            *flag = down;
        }
        KeyEvent {
            key,
            mods: self.mods,
            down,
            repeat,
        }
    }

    /// Modifiers currently held.
    pub fn mods(&self) -> Mods {
        self.mods
    }

    /// True if `key` is currently held.
    pub fn is_held(&self, key: Key) -> bool {
        self.held.contains(&key)
    }

    /// Forget everything (call on every mode exit so no key stays stuck).
    pub fn reset(&mut self) {
        self.held.clear();
        self.mods = Mods::NONE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::Row;

    const J: Key = Key::Phys(PhysKey::new(Row::Home, 6));
    const SHIFT: Key = Key::Named(NamedKey::Shift);
    const CTRL: Key = Key::Named(NamedKey::Ctrl);

    #[test]
    fn plain_press_and_release() {
        let mut t = KeyTracker::new();
        let d = t.track(J, true);
        assert!(d.down && !d.repeat && d.mods.is_empty());
        let u = t.track(J, false);
        assert!(!u.down && !u.repeat);
        assert!(!t.is_held(J));
    }

    #[test]
    fn second_press_without_release_is_repeat() {
        let mut t = KeyTracker::new();
        assert!(!t.track(J, true).repeat);
        assert!(t.track(J, true).repeat);
        assert!(t.track(J, true).repeat);
        t.track(J, false);
        assert!(!t.track(J, true).repeat);
    }

    #[test]
    fn modifiers_include_own_press_and_clear_on_release() {
        let mut t = KeyTracker::new();
        let s = t.track(SHIFT, true);
        assert!(s.mods.shift, "a modifier's own press counts");
        let j = t.track(J, true);
        assert_eq!(
            j.mods,
            Mods {
                shift: true,
                ..Mods::NONE
            }
        );
        let su = t.track(SHIFT, false);
        assert!(!su.mods.shift);
        assert!(t.track(J, false).mods.is_empty());
    }

    #[test]
    fn release_without_press_is_harmless() {
        let mut t = KeyTracker::new();
        let e = t.track(CTRL, false);
        assert!(!e.down && !e.repeat && e.mods.is_empty());
        let e = t.track(J, false);
        assert!(!e.down && !e.repeat);
    }

    #[test]
    fn seeded_mods_clear_on_release() {
        let mut t = KeyTracker::with_mods(Mods {
            ctrl: true,
            alt: true,
            ..Mods::NONE
        });
        assert!(t.track(J, true).mods.ctrl);
        t.track(CTRL, false);
        let e = t.track(J, false);
        assert!(!e.mods.ctrl && e.mods.alt);
    }

    #[test]
    fn reset_clears_held_keys_and_mods() {
        let mut t = KeyTracker::new();
        t.track(SHIFT, true);
        t.track(J, true);
        t.reset();
        assert!(t.mods().is_empty());
        assert!(!t.track(J, true).repeat, "J is no longer held");
    }

    #[test]
    fn is_modifier() {
        assert!(SHIFT.is_modifier() && CTRL.is_modifier());
        assert!(Key::Named(NamedKey::Meta).is_modifier());
        assert!(!J.is_modifier());
        assert!(!Key::Named(NamedKey::Esc).is_modifier());
        assert!(!Key::Other(0x4C).is_modifier());
    }
}
