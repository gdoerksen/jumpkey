//! Synthesized keyboard input via `SendInput`.

use anyhow::{bail, Context, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, SendInput, UnregisterHotKey, HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    MOD_WIN, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_SHIFT, VK_SPACE,
};

/// A parsed chord like `ctrl+shift+space`: modifiers first, then one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// As written, for messages.
    pub text: String,
    pub modifiers: Vec<u16>,
    pub key: u16,
}

impl Chord {
    /// Parse `ctrl+shift+space`, `Ctrl+Alt+J`, `win+f2`, `ctrl+shift+,` …
    /// Case-insensitive; `+` separates parts. A trailing `+` key is
    /// written as `ctrl++` → plus (rare; not used by any default).
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty hotkey");
        }
        let mut parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
        // `ctrl++` splits into ["ctrl", "", ""]: the key is '+'.
        if parts.len() >= 3
            && parts[parts.len() - 1].is_empty()
            && parts[parts.len() - 2].is_empty()
        {
            parts.truncate(parts.len() - 2);
            parts.push("plus".into());
        }
        let key_name = parts.pop().context("missing key")?;
        let mut modifiers = Vec::new();
        for m in &parts {
            let vk = match m.as_str() {
                "ctrl" | "control" => VK_CONTROL.0,
                "shift" => VK_SHIFT.0,
                "alt" | "menu" => VK_MENU.0,
                "win" | "super" | "meta" => VK_LWIN.0,
                other => bail!("unknown modifier `{other}` in hotkey `{s}`"),
            };
            modifiers.push(vk);
        }
        Ok(Self {
            text: s.to_string(),
            modifiers,
            key: key_vk(&key_name).with_context(|| format!("unknown key `{key_name}` in `{s}`"))?,
        })
    }
}

impl Chord {
    /// Whether no other application has registered this chord as a
    /// global hotkey. Checked by briefly registering it ourselves.
    ///
    /// A taken chord makes jumpkey show a blocking "hotkey conflict"
    /// message box at startup, and the benchmark would hang on it.
    pub fn is_free(&self) -> bool {
        let mut mods = MOD_NOREPEAT;
        for &m in &self.modifiers {
            mods |= match m {
                x if x == VK_CONTROL.0 => MOD_CONTROL,
                x if x == VK_SHIFT.0 => MOD_SHIFT,
                x if x == VK_MENU.0 => MOD_ALT,
                x if x == VK_LWIN.0 => MOD_WIN,
                _ => HOT_KEY_MODIFIERS(0),
            };
        }
        const PROBE_ID: i32 = 0x5EED;
        // SAFETY: thread-level hotkey (no HWND), unregistered right away.
        unsafe {
            if RegisterHotKey(None, PROBE_ID, mods, self.key as u32).is_ok() {
                let _ = UnregisterHotKey(None, PROBE_ID);
                true
            } else {
                false
            }
        }
    }
}

fn key_vk(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    if b.len() == 1 {
        let c = b[0];
        return match c {
            b'a'..=b'z' => Some(c.to_ascii_uppercase() as u16),
            b'0'..=b'9' => Some(c as u16),
            b',' => Some(0xBC), // VK_OEM_COMMA
            b'.' => Some(0xBE), // VK_OEM_PERIOD
            b'-' => Some(0xBD), // VK_OEM_MINUS
            b';' => Some(0xBA), // VK_OEM_1
            b'/' => Some(0xBF), // VK_OEM_2
            b'`' => Some(0xC0), // VK_OEM_3
            b'[' => Some(0xDB),
            b'\\' => Some(0xDC),
            b']' => Some(0xDD),
            b'\'' => Some(0xDE),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1); // VK_F1..
        }
    }
    Some(match name {
        "space" => VK_SPACE.0,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => VK_ESCAPE.0,
        "plus" | "=" => 0xBB,
        "comma" => 0xBC,
        "period" => 0xBE,
        "backslash" => 0xDC,
        _ => return None,
    })
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press and release the chord as one atomic `SendInput` batch, so no
/// real keystroke can interleave with it.
pub fn send_chord(chord: &Chord) -> Result<()> {
    let mut inputs = Vec::with_capacity(chord.modifiers.len() * 2 + 2);
    for &m in &chord.modifiers {
        inputs.push(key_input(m, false));
    }
    inputs.push(key_input(chord.key, false));
    inputs.push(key_input(chord.key, true));
    for &m in chord.modifiers.iter().rev() {
        inputs.push(key_input(m, true));
    }
    send(&inputs)
}

/// Tap a single key.
pub fn tap(vk: u16) -> Result<()> {
    send(&[key_input(vk, false), key_input(vk, true)])
}

pub fn tap_escape() -> Result<()> {
    tap(VK_ESCAPE.0)
}

/// Tap Alt on its own. Windows only lets a process take the foreground
/// if it received the last input event; a synthesized Alt tap counts,
/// and on its own it doesn't trigger menus in most apps.
pub fn tap_alt() -> Result<()> {
    tap(VK_MENU.0)
}

/// Press or release one key.
pub fn key(vk: u16, up: bool) -> Result<()> {
    send(&[key_input(vk, up)])
}

/// The virtual key that types `c` on the current layout (unshifted).
pub fn vk_for_char(c: char) -> Option<u16> {
    let mut buf = [0u16; 2];
    let unit = c.encode_utf16(&mut buf).first().copied()?;
    // SAFETY: plain layout lookup.
    let r = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(unit) };
    (r != -1).then_some((r as u16) & 0xFF)
}

/// Type `text` key by key (each char tapped with its unshifted key).
pub fn type_text(text: &str) -> Result<()> {
    for c in text.chars() {
        let vk = vk_for_char(c).with_context(|| format!("no key types `{c}`"))?;
        tap(vk)?;
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    Ok(())
}

fn mouse_input(flags: u32, data: i32) -> INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT_MOUSE, MOUSEINPUT, MOUSE_EVENT_FLAGS};
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: MOUSE_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Move the pointer to `(x, y)` (physical screen pixels).
pub fn move_pointer(x: i32, y: i32) -> Result<()> {
    // SAFETY: plain pointer move.
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y)? };
    Ok(())
}

/// Left click at `(x, y)`.
pub fn click_at(x: i32, y: i32) -> Result<()> {
    move_pointer(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(30));
    // MOUSEEVENTF_LEFTDOWN / LEFTUP
    send(&[mouse_input(0x0002, 0), mouse_input(0x0004, 0)])
}

/// One wheel notch (down) at `(x, y)`.
pub fn wheel_at(x: i32, y: i32) -> Result<()> {
    move_pointer(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(30));
    // MOUSEEVENTF_WHEEL, one notch towards the user.
    send(&[mouse_input(0x0800, -120)])
}

/// Current pointer position.
pub fn pointer() -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT::default();
    // SAFETY: valid out-pointer.
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

fn send(inputs: &[INPUT]) -> Result<()> {
    // SAFETY: slice of fully-initialized INPUT structs; size is correct.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(
            "SendInput injected {sent} of {} events (blocked by UIPI or a secure desktop?)",
            inputs.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_chord() {
        let c = Chord::parse("Ctrl+Shift+Space").unwrap();
        assert_eq!(c.modifiers, vec![VK_CONTROL.0, VK_SHIFT.0]);
        assert_eq!(c.key, VK_SPACE.0);
    }

    #[test]
    fn parses_letters_digits_function_keys_and_punctuation() {
        assert_eq!(Chord::parse("alt+j").unwrap().key, b'J' as u16);
        assert_eq!(Chord::parse("ctrl+7").unwrap().key, b'7' as u16);
        assert_eq!(Chord::parse("win+F2").unwrap().key, 0x71);
        assert_eq!(Chord::parse("ctrl+shift+,").unwrap().key, 0xBC);
        assert_eq!(Chord::parse("ctrl++").unwrap().key, 0xBB);
    }

    #[test]
    fn rejects_unknown_parts() {
        assert!(Chord::parse("hyper+x").is_err());
        assert!(Chord::parse("ctrl+nope").is_err());
        assert!(Chord::parse("").is_err());
    }
}
