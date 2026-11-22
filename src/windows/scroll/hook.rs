//! Scroll mode's input thread: a `WH_KEYBOARD_LL` and a `WH_MOUSE_LL`
//! hook on a dedicated thread, installed only while a session runs.
//!
//! Why a dedicated thread (spike #34): Windows lets a low-level hook
//! callback take about 320 ms (`LowLevelHooksTimeout`) before it passes
//! the key to the app anyway. jumpkey's main thread runs UIA calls that
//! can take longer, so the hook gets a thread that does nothing else.
//!
//! The callback stays tiny:
//! 1. map the event to a [`KeyEvent`] (`key_from_ll` + [`KeyTracker`]);
//! 2. ask the pure [`ScrollMode`] for a verdict and effects;
//! 3. queue the effects for the main thread and wake it with a thread
//!    message.
//!
//! Events carrying [`JUMPKEY_INPUT_TAG`] are jumpkey's own and are
//! ignored; every other event, injected or not, counts as real (keys
//! remapped by PowerToys or AutoHotkey arrive injected).
//!
//! The thread also ends the session on:
//! - a mouse button press (never swallowed: the click reaches the app);
//! - another window coming to the foreground (`EVENT_SYSTEM_FOREGROUND`);
//! - a desktop switch (UAC, Ctrl+Alt+Del: `EVENT_SYSTEM_DESKTOPSWITCH`)
//!   or a session lock (`WTSRegisterSessionNotification`). The hook can't
//!   see keys on the secure desktop, so held-key state would go stale.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PeekMessageW, PostThreadMessageW, RegisterClassExW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND, HC_ACTION, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
    PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_QUIT,
    WM_RBUTTONDOWN, WM_WTSSESSION_CHANGE, WM_XBUTTONDOWN, WNDCLASSEXW, WTS_CONSOLE_DISCONNECT,
    WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK,
};

use crate::input::{KeyEvent, KeyTracker, Mods};
use crate::scroll::{Area, Chords, Effect, ExitReason, ScrollMode, Verdict};
use crate::windows::keyboard_layout::key_from_ll;
use crate::windows::JUMPKEY_INPUT_TAG;

/// Thread message to the hook thread: a [`Command`] is waiting.
const WM_HOOK_COMMAND: u32 = WM_APP + 0x51;

/// An unassigned virtual key, injected between a lone Alt's press and
/// release so the app sees Alt+key instead of an Alt tap (which would
/// open its menu bar). AutoHotkey's "menu mask key" uses the same one.
const VK_MASK: u16 = 0xE8;

/// What starting a session needs from the caller.
#[derive(Debug, Clone, Copy)]
pub struct HookConfig {
    /// jumpkey's hotkeys, matched in the hook.
    pub chords: Chords,
    /// Thread to wake when effects are queued (jumpkey's main thread).
    pub main_thread_id: u32,
    /// Thread message posted to `main_thread_id` with each batch of
    /// effects (`wParam`/`lParam` 0). Drain with [`ScrollHook::drain`].
    pub wake_message: u32,
    /// The foreground window when the session started: a foreground event
    /// for this window doesn't end the session.
    pub foreground: HWND,
}

enum Command {
    Areas(Vec<Area>, usize),
    Exit(ExitReason),
}

/// A running scroll-mode input session. Dropping it removes the hooks and
/// joins the thread.
pub struct ScrollHook {
    thread_id: u32,
    commands: Sender<Command>,
    effects: Receiver<Effect>,
    user_moved_pointer: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ScrollHook {
    /// Start the hook thread and install the hooks. Returns once the
    /// hooks are in place, so no key typed after this returns is missed.
    pub fn start(cfg: HookConfig) -> Result<Self> {
        let (fx_tx, fx_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
        let moved = Arc::new(AtomicBool::new(false));
        let moved_thread = Arc::clone(&moved);
        // HWND isn't Send; carry the raw value.
        let fg = cfg.foreground.0 as isize;
        let (chords, main_tid, wake) = (cfg.chords, cfg.main_thread_id, cfg.wake_message);
        let join = std::thread::Builder::new()
            .name("jumpkey-scroll-hook".into())
            .spawn(move || {
                // Seeded on the hook thread, right before the hook goes
                // in: the hotkey's own Ctrl+Alt are still held.
                let seeded = held_mods();
                let state = HookState {
                    mode: ScrollMode::new(chords, seeded.shift),
                    tracker: KeyTracker::with_mods(seeded),
                    out: fx_tx,
                    main_tid,
                    wake,
                    foreground: fg,
                    user_moved_pointer: moved_thread,
                };
                thread_main(state, cmd_rx, ready_tx);
            })
            .context("spawning scroll hook thread")?;
        let thread_id = match ready_rx.recv() {
            Ok(Ok(tid)) => tid,
            Ok(Err(e)) => {
                let _ = join.join();
                return Err(e);
            }
            Err(_) => {
                let _ = join.join();
                anyhow::bail!("scroll hook thread exited during start-up");
            }
        };
        Ok(Self {
            thread_id,
            commands: cmd_tx,
            effects: fx_rx,
            user_moved_pointer: moved,
            join: Some(join),
        })
    }

    /// Effects queued since the last call, in order.
    pub fn drain(&self) -> Vec<Effect> {
        self.effects.try_iter().collect()
    }

    /// Discovery finished: hand the areas to the session.
    pub fn set_areas(&self, areas: Vec<Area>, active: usize) {
        self.send(Command::Areas(areas, active));
    }

    /// End the session from outside (e.g. the main thread saw a reason).
    /// The resulting `Exit` effect arrives through [`Self::drain`].
    pub fn exit(&self, reason: ExitReason) {
        self.send(Command::Exit(reason));
    }

    /// True once the user moved the mouse during the session (physical
    /// moves only), so the pointer shouldn't be put back on exit.
    pub fn user_moved_pointer(&self) -> bool {
        self.user_moved_pointer.load(Ordering::Relaxed)
    }

    fn send(&self, cmd: Command) {
        if self.commands.send(cmd).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_HOOK_COMMAND, WPARAM(0), LPARAM(0));
            }
        }
    }
}

impl Drop for ScrollHook {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Modifiers really held right now (`GetAsyncKeyState`), to seed the
/// tracker: the hotkey's own Ctrl+Alt are down when the session starts.
fn held_mods() -> Mods {
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0;
    Mods {
        shift: down(VK_SHIFT),
        ctrl: down(VK_CONTROL),
        alt: down(VK_MENU),
        meta: down(VK_LWIN) || down(VK_RWIN),
    }
}

struct HookState {
    mode: ScrollMode,
    tracker: KeyTracker,
    out: Sender<Effect>,
    main_tid: u32,
    wake: u32,
    foreground: isize,
    user_moved_pointer: Arc<AtomicBool>,
}

impl HookState {
    fn emit(&self, fx: Vec<Effect>) {
        if fx.is_empty() {
            return;
        }
        for e in fx {
            let _ = self.out.send(e);
        }
        unsafe {
            let _ = PostThreadMessageW(self.main_tid, self.wake, WPARAM(0), LPARAM(0));
        }
    }

    fn exit(&mut self, reason: ExitReason) {
        let fx = self.mode.exit(reason);
        self.emit(fx);
    }
}

thread_local! {
    /// The session, owned by the hook thread. The hook procs and the
    /// WinEvent / window procs all run on this thread.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut HookState) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok()?.as_mut().map(f))
}

fn thread_main(state: HookState, commands: Receiver<Command>, ready: Sender<Result<u32>>) {
    unsafe {
        // Make sure the thread has a message queue before anyone posts.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let tid = GetCurrentThreadId();
        STATE.with(|s| *s.borrow_mut() = Some(state));

        let kb =
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), HINSTANCE::default(), 0) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready.send(Err(anyhow::anyhow!("installing keyboard hook: {e}")));
                    return;
                }
            };
        let mouse = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), HINSTANCE::default(), 0)
        {
            Ok(h) => h,
            Err(e) => {
                let _ = UnhookWindowsHookEx(kb);
                let _ = ready.send(Err(anyhow::anyhow!("installing mouse hook: {e}")));
                return;
            }
        };
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let fg_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            flags,
        );
        let desk_hook = SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(win_event_proc),
            0,
            0,
            flags,
        );
        let session_wnd = create_session_window();
        let _ = ready.send(Ok(tid));

        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.hwnd.0.is_null() && msg.message == WM_HOOK_COMMAND {
                for cmd in commands.try_iter() {
                    with_state(|s| match cmd {
                        Command::Areas(areas, active) => s.mode.on_areas(areas, active),
                        Command::Exit(reason) => s.exit(reason),
                    });
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if let Some(h) = session_wnd {
            let _ = WTSUnRegisterSessionNotification(h);
            let _ = DestroyWindow(h);
        }
        for h in [fg_hook, desk_hook] {
            if !h.is_invalid() {
                let _ = UnhookWinEvent(h);
            }
        }
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(kb);
        STATE.with(|s| s.borrow_mut().take());
    }
}

/// A message-only window that receives `WM_WTSSESSION_CHANGE`. `None` if
/// it couldn't be created (the desktop-switch WinEvent still covers
/// Ctrl+Alt+Del and UAC).
unsafe fn create_session_window() -> Option<HWND> {
    let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let class = w!("jumpkey-scroll-session");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(session_wnd_proc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    // Already registered by an earlier session: fine.
    let _ = RegisterClassExW(&wc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        None,
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        None,
        hinst,
        None,
    )
    .ok()?;
    if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
        tracing::debug!("WTSRegisterSessionNotification failed; lock won't end scroll mode");
    }
    Some(hwnd)
}

unsafe extern "system" fn session_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        let event = wparam.0 as u32;
        if matches!(
            event,
            WTS_SESSION_LOCK | WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT
        ) {
            with_state(|s| s.exit(ExitReason::Lock));
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    with_state(|s| {
        if event == EVENT_SYSTEM_DESKTOPSWITCH {
            s.exit(ExitReason::Lock);
        } else if event == EVENT_SYSTEM_FOREGROUND && hwnd.0 as isize != s.foreground {
            s.exit(ExitReason::Foreground);
        }
    });
}

/// One tagged keyboard event.
fn tagged_key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: JUMPKEY_INPUT_TAG,
            },
        },
    }
}

/// Replace a lone Alt release with mask-key press + release + the same
/// Alt release, all tagged and in one batch so their order is fixed. The
/// app sees Alt+mask (no menu bar); the real release is swallowed by the
/// caller.
fn send_masked_alt_release(info: &KBDLLHOOKSTRUCT) {
    let mut alt_flags = KEYEVENTF_KEYUP;
    if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
        alt_flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let inputs = [
        tagged_key(VK_MASK, 0, KEYBD_EVENT_FLAGS(0)),
        tagged_key(VK_MASK, 0, KEYEVENTF_KEYUP),
        tagged_key(info.vkCode as u16, info.scanCode as u16, alt_flags),
    ];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        tracing::debug!(sent, "masked Alt release partly sent");
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo != JUMPKEY_INPUT_TAG {
            let swallow = with_state(|s| {
                let (key, down) = key_from_ll(info);
                let ev: KeyEvent = s.tracker.track(key, down);
                let (verdict, mut fx) = s.mode.on_key(ev);
                let mask = fx.contains(&Effect::MaskAlt);
                fx.retain(|e| *e != Effect::MaskAlt);
                s.emit(fx);
                if mask {
                    send_masked_alt_release(info);
                    return true;
                }
                verdict == Verdict::Swallow
            })
            .unwrap_or(false);
            if swallow {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        if info.dwExtraInfo != JUMPKEY_INPUT_TAG {
            if matches!(
                msg,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            ) {
                // Never swallowed: the click still reaches the app.
                with_state(|s| s.exit(ExitReason::Click));
            } else if msg == WM_MOUSEMOVE && info.flags & LLMHF_INJECTED == 0 {
                with_state(|s| s.user_moved_pointer.store(true, Ordering::Relaxed));
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}
