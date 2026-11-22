//! Roadmap step 4 hook spike (throwaway, branch `spike/hook`, never merged).
//!
//! ```text
//! cargo run --release --example hook_spike -- menus
//! cargo run --release --example hook_spike -- stall
//! cargo run --release --example hook_spike -- scroll-areas --fixtures
//! cargo run --release --example hook_spike -- scroll-areas --wait 8
//! ```
//!
//! Prints counts, timings and yes/no answers only: no window titles,
//! element names or URLs.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    if let Err(e) = spike::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod spike {
    use std::collections::HashSet;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU32, Ordering::SeqCst};
    use std::sync::{mpsc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use jumpkey::input::{Key, KeyTracker, NamedKey};
    use jumpkey::windows::keyboard_layout::key_from_ll;
    use uiautomation::types::{
        ControlType, ExpandCollapseState, Handle, Point, TreeScope, UIProperty,
    };
    use uiautomation::variants::Variant;
    use uiautomation::{UIAutomation, UIElement};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// `dwExtraInfo` tag on the driver's own events ("JKSP").
    const MAGIC: usize = 0x4A4B_5350;
    const HOTKEY_ID: i32 = 0x4A4B;

    const CMD_SHOW: u32 = WM_APP + 1;
    const CMD_HIDE: u32 = WM_APP + 2;
    const CMD_HOOK_ON: u32 = WM_APP + 3;
    const CMD_HOOK_OFF: u32 = WM_APP + 4;
    const CMD_TOPMOST: u32 = WM_APP + 5;
    const CMD_QUIT: u32 = WM_APP + 6;

    // ---- state shared with the hook thread -------------------------------

    static MAIN_TID: AtomicU32 = AtomicU32::new(0);
    static HOOK: AtomicIsize = AtomicIsize::new(0);
    static OVERLAY: AtomicIsize = AtomicIsize::new(0);
    /// 0 = pass everything, 1 = production (swallow tagged non-modifier keys
    /// without Ctrl/Alt), 2 = swallow every tagged event.
    static MODE: AtomicU32 = AtomicU32::new(0);
    /// One-shot: the next tagged key-down sleeps this long in the hook.
    static STALL_MS: AtomicU32 = AtomicU32::new(0);
    static LAST_SEND: AtomicI64 = AtomicI64::new(0);
    static HOTKEYS: AtomicU32 = AtomicU32::new(0);
    static MENU_START: AtomicU32 = AtomicU32::new(0);
    static MENU_END: AtomicU32 = AtomicU32::new(0);
    static ACK: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
    static OVERLAY_RECT: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 0, 0));
    static LOG: Mutex<Vec<Rec>> = Mutex::new(Vec::new());
    static TRACKER: Mutex<Option<KeyTracker>> = Mutex::new(None);
    static QUIET: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Src {
        Tagged,
        Untagged,
        Real,
    }

    #[derive(Debug, Clone, Copy)]
    struct Rec {
        key: Key,
        down: bool,
        repeat: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
        src: Src,
        swallowed: bool,
        latency_us: Option<u64>,
    }

    fn qpc() -> i64 {
        let mut v = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut v);
        }
        v
    }

    fn qpc_us(d: i64) -> u64 {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        (d.max(0) as u128 * 1_000_000 / f.max(1) as u128) as u64
    }

    unsafe extern "system" fn ll_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let t0 = qpc();
            let info = &*(l.0 as *const KBDLLHOOKSTRUCT);
            let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
            let src = match (injected, info.dwExtraInfo == MAGIC) {
                (true, true) => Src::Tagged,
                (true, false) => Src::Untagged,
                (false, _) => Src::Real,
            };
            let (key, down) = key_from_ll(info);
            let ev = TRACKER
                .lock()
                .unwrap()
                .get_or_insert_with(KeyTracker::new)
                .track(key, down);
            if src == Src::Tagged && down {
                let s = STALL_MS.swap(0, SeqCst);
                if s > 0 {
                    thread::sleep(Duration::from_millis(s as u64));
                }
            }
            let swallow = src == Src::Tagged
                && match MODE.load(SeqCst) {
                    1 => !key.is_modifier() && !ev.mods.ctrl && !ev.mods.alt,
                    2 => true,
                    _ => false,
                };
            let latency_us = (src == Src::Tagged).then(|| qpc_us(t0 - LAST_SEND.load(SeqCst)));
            LOG.lock().unwrap().push(Rec {
                key,
                down,
                repeat: ev.repeat,
                ctrl: ev.mods.ctrl,
                alt: ev.mods.alt,
                shift: ev.mods.shift,
                src,
                swallowed: swallow,
                latency_us,
            });
            if swallow {
                return LRESULT(1);
            }
        }
        CallNextHookEx(HHOOK(HOOK.load(SeqCst) as _), code, w, l)
    }

    unsafe extern "system" fn win_event(
        _h: HWINEVENTHOOK,
        event: u32,
        _hwnd: HWND,
        _obj: i32,
        _child: i32,
        _tid: u32,
        _time: u32,
    ) {
        match event {
            EVENT_SYSTEM_MENUPOPUPSTART => MENU_START.fetch_add(1, SeqCst),
            EVENT_SYSTEM_MENUPOPUPEND => MENU_END.fetch_add(1, SeqCst),
            _ => 0,
        };
    }

    fn hook_on() {
        unsafe {
            let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), HINSTANCE::default(), 0)
                .expect("SetWindowsHookExW");
            HOOK.store(h.0 as isize, SeqCst);
        }
    }

    fn hook_off() {
        let h = HOOK.swap(0, SeqCst);
        if h != 0 {
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(h as _));
            }
        }
    }

    unsafe extern "system" fn def_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    /// The hook thread: message loop, overlay, hotkey, WinEvent hook.
    fn hook_thread_main(ready: mpsc::Sender<bool>) {
        unsafe {
            MAIN_TID.store(GetCurrentThreadId(), SeqCst);
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            let class = w!("JkSpikeOverlay");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(def_proc),
                hInstance: hinst,
                lpszClassName: class,
                hbrBackground: HBRUSH(CreateSolidBrush(COLORREF(0x0000_80FF)).0),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let ov = CreateWindowExW(
                WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW,
                class,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                hinst,
                None,
            )
            .unwrap();
            let _ = SetLayeredWindowAttributes(ov, COLORREF(0), 70, LWA_ALPHA);
            OVERLAY.store(ov.0 as isize, SeqCst);
            let hk = RegisterHotKey(
                None,
                HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
                VK_F9.0 as u32,
            )
            .is_ok();
            let weh = SetWinEventHook(
                EVENT_SYSTEM_MENUPOPUPSTART,
                EVENT_SYSTEM_MENUPOPUPEND,
                None,
                Some(win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            let _ = ready.send(hk);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let ack = match msg.message {
                    WM_HOTKEY if msg.wParam.0 as i32 == HOTKEY_ID => {
                        HOTKEYS.fetch_add(1, SeqCst);
                        false
                    }
                    CMD_SHOW => {
                        let (x, y, w, h) = *OVERLAY_RECT.lock().unwrap();
                        let _ = SetWindowPos(
                            ov,
                            HWND_TOPMOST,
                            x,
                            y,
                            w,
                            h,
                            SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        );
                        true
                    }
                    CMD_TOPMOST => {
                        let _ = SetWindowPos(
                            ov,
                            HWND_TOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                        );
                        true
                    }
                    CMD_HIDE => {
                        let _ = ShowWindow(ov, SW_HIDE);
                        true
                    }
                    CMD_HOOK_ON => {
                        hook_on();
                        true
                    }
                    CMD_HOOK_OFF => {
                        hook_off();
                        true
                    }
                    CMD_QUIT => break,
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                        false
                    }
                };
                if ack {
                    if let Some(tx) = ACK.lock().unwrap().as_ref() {
                        let _ = tx.send(());
                    }
                }
            }
            hook_off();
            let _ = UnhookWinEvent(weh);
            let _ = UnregisterHotKey(None, HOTKEY_ID);
            let _ = DestroyWindow(ov);
        }
    }

    struct HookThread {
        ack: mpsc::Receiver<()>,
        join: Option<thread::JoinHandle<()>>,
        hotkey_registered: bool,
    }

    impl HookThread {
        fn start() -> Self {
            let (tx, rx) = mpsc::channel();
            *ACK.lock().unwrap() = Some(tx);
            let (rtx, rrx) = mpsc::channel();
            let join = thread::spawn(move || hook_thread_main(rtx));
            let hotkey_registered = rrx.recv().unwrap();
            Self {
                ack: rx,
                join: Some(join),
                hotkey_registered,
            }
        }

        fn cmd(&self, m: u32) {
            unsafe {
                let _ = PostThreadMessageW(MAIN_TID.load(SeqCst), m, WPARAM(0), LPARAM(0));
            }
            // A stalled hook thread may answer late; don't block forever.
            let _ = self.ack.recv_timeout(Duration::from_secs(10));
        }

        fn show(&self, r: RECT) {
            *OVERLAY_RECT.lock().unwrap() = (r.left, r.top, r.right - r.left, r.bottom - r.top);
            self.cmd(CMD_SHOW);
        }
    }

    impl Drop for HookThread {
        fn drop(&mut self) {
            unsafe {
                let _ = PostThreadMessageW(MAIN_TID.load(SeqCst), CMD_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    // ---- input -------------------------------------------------------------

    fn key_input(vk: VIRTUAL_KEY, sc: u16, ext: bool, up: bool, tag: bool) -> INPUT {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if sc != 0 {
            flags |= KEYEVENTF_SCANCODE;
        }
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: if sc != 0 { VIRTUAL_KEY(0) } else { vk },
                    wScan: sc,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: if tag { MAGIC } else { 0 },
                },
            },
        }
    }

    fn send_one(i: INPUT) {
        LAST_SEND.store(qpc(), SeqCst);
        unsafe {
            SendInput(&[i], std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// Press and release a key by scan code, one SendInput per event.
    fn tap_sc(sc: u16, ext: bool, tag: bool) {
        send_one(key_input(VIRTUAL_KEY(0), sc, ext, false, tag));
        sleep(15);
        send_one(key_input(VIRTUAL_KEY(0), sc, ext, true, tag));
        sleep(15);
    }

    /// Press `mods` (scan codes), tap `sc`, release in reverse order.
    fn chord_sc(mods: &[(u16, bool)], sc: u16, ext: bool, tag: bool) {
        for &(m, e) in mods {
            send_one(key_input(VIRTUAL_KEY(0), m, e, false, tag));
            sleep(15);
        }
        tap_sc(sc, ext, tag);
        for &(m, e) in mods.iter().rev() {
            send_one(key_input(VIRTUAL_KEY(0), m, e, true, tag));
            sleep(15);
        }
    }

    const SC_ESC: u16 = 0x01;
    const SC_J: u16 = 0x24;
    const SC_K: u16 = 0x25;
    const SC_DOWN: u16 = 0x50; // extended
    const SC_RIGHT: u16 = 0x4D; // extended
    const SC_F9: u16 = 0x43;
    const SC_F10: u16 = 0x44;
    const SC_F24: u16 = 0x76;
    const SC_SPACE: u16 = 0x39;
    const SC_SHIFT: u16 = 0x2A;
    const SC_CTRL: u16 = 0x1D;
    const SC_ALT: u16 = 0x38;
    const SC_L: u16 = 0x26;
    const SC_A: u16 = 0x1E;

    fn esc_untagged(n: usize) {
        for _ in 0..n {
            tap_sc(SC_ESC, false, false);
            sleep(120);
        }
    }

    fn sleep(ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    // ---- windows -----------------------------------------------------------

    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    struct Win(isize);

    impl Win {
        fn h(self) -> HWND {
            HWND(self.0 as _)
        }
        fn class(self) -> String {
            let mut b = [0u16; 128];
            let n = unsafe { GetClassNameW(self.h(), &mut b) };
            String::from_utf16_lossy(&b[..n.max(0) as usize])
        }
        fn pid(self) -> u32 {
            let mut p = 0;
            unsafe { GetWindowThreadProcessId(self.h(), Some(&mut p)) };
            p
        }
        fn visible(self) -> bool {
            unsafe { IsWindowVisible(self.h()).as_bool() }
        }
        fn rect(self) -> RECT {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.h(), &mut r);
            }
            r
        }
        fn popup(self) -> bool {
            let s = unsafe { GetWindowLongW(self.h(), GWL_STYLE) } as u32;
            s & WS_POPUP.0 != 0
        }
    }

    fn top_windows() -> Vec<Win> {
        unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
            (*(l.0 as *mut Vec<Win>)).push(Win(h.0 as isize));
            true.into()
        }
        let mut v: Vec<Win> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut v as *mut _ as isize));
        }
        v
    }

    fn foreground() -> Win {
        Win(unsafe { GetForegroundWindow() }.0 as isize)
    }

    fn force_foreground(w: Win) -> bool {
        unsafe {
            let fg = GetForegroundWindow();
            let me = GetCurrentThreadId();
            let other = GetWindowThreadProcessId(fg, None);
            let attached =
                other != 0 && other != me && AttachThreadInput(me, other, true).as_bool();
            let _ = BringWindowToTop(w.h());
            let ok = SetForegroundWindow(w.h()).as_bool();
            if attached {
                let _ = AttachThreadInput(me, other, false);
            }
            sleep(250);
            ok && foreground() == w
        }
    }

    /// Wait for a new visible top-level window matching `pred`.
    fn wait_new_window(
        before: &HashSet<Win>,
        secs: u64,
        pred: impl Fn(Win) -> bool,
    ) -> Option<Win> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            if let Some(w) = top_windows()
                .into_iter()
                .find(|w| !before.contains(w) && w.visible() && pred(*w))
            {
                return Some(w);
            }
            sleep(200);
        }
        None
    }

    /// Is `ov` above `other` in z-order?
    fn above(ov: Win, other: Win) -> Option<bool> {
        let mut h = ov.h();
        for _ in 0..5000 {
            match unsafe { GetWindow(h, GW_HWNDNEXT) } {
                Ok(n) if !n.0.is_null() => {
                    if n.0 as isize == other.0 {
                        return Some(true);
                    }
                    h = n;
                }
                _ => break,
            }
        }
        // Not found below the overlay: check above it.
        let mut h = ov.h();
        for _ in 0..5000 {
            match unsafe { GetWindow(h, GW_HWNDPREV) } {
                Ok(n) if !n.0.is_null() => {
                    if n.0 as isize == other.0 {
                        return Some(false);
                    }
                    h = n;
                }
                _ => break,
            }
        }
        None
    }

    // ---- menu detection ------------------------------------------------------

    #[derive(Debug, Default, Clone)]
    struct Sig {
        m32768: Option<Win>,
        popups: Vec<Win>,
        winevent_open: bool,
        uia: Option<&'static str>,
        focus: Option<Vec<i32>>,
    }

    impl Sig {
        fn open(&self) -> bool {
            self.m32768.is_some()
                || !self.popups.is_empty()
                || self.winevent_open
                || self.uia.is_some()
        }
        fn names(&self) -> String {
            let mut v = Vec::new();
            if self.m32768.is_some() {
                v.push("#32768");
            }
            if !self.popups.is_empty() {
                v.push("popup-hwnd");
            }
            if self.winevent_open {
                v.push("winevent");
            }
            if let Some(u) = self.uia {
                v.push(u);
            }
            if v.is_empty() {
                "none".into()
            } else {
                v.join("+")
            }
        }
        /// The topmost menu window, for z-order checks.
        fn menu_window(&self) -> Option<Win> {
            self.m32768.or_else(|| self.popups.first().copied())
        }
    }

    struct Detector {
        uia: UIAutomation,
        pids: HashSet<u32>,
        baseline: HashSet<Win>,
        /// For menus drawn inside the window (no popup HWND): the window
        /// and its ListItem count before opening.
        list_items: Option<(Win, usize)>,
    }

    fn count_list_items(uia: &UIAutomation, w: Win) -> usize {
        let Ok(root) = uia.element_from_handle(Handle::from(w.0)) else {
            return 0;
        };
        let Ok(cond) = uia.create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::ListItem as i32),
            None,
        ) else {
            return 0;
        };
        root.find_all(TreeScope::Descendants, &cond)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    impl Detector {
        fn new(uia: &UIAutomation, pids: HashSet<u32>, in_window: Option<Win>) -> Self {
            MENU_START.store(0, SeqCst);
            MENU_END.store(0, SeqCst);
            Self {
                uia: uia.clone(),
                pids,
                baseline: top_windows().into_iter().filter(|w| w.visible()).collect(),
                list_items: in_window.map(|w| (w, count_list_items(uia, w))),
            }
        }

        fn sig(&self) -> Sig {
            let wins = top_windows();
            let ov = Win(OVERLAY.load(SeqCst));
            let m32768 = wins
                .iter()
                .copied()
                .find(|w| w.visible() && w.class() == "#32768");
            let popups = wins
                .iter()
                .copied()
                .filter(|w| {
                    *w != ov
                        && w.visible()
                        && !self.baseline.contains(w)
                        && w.popup()
                        && w.class() != "#32768"
                        && (self.pids.is_empty() || self.pids.contains(&w.pid()))
                })
                .collect();
            let winevent_open = MENU_START.load(SeqCst) > MENU_END.load(SeqCst);
            let (mut uia, focus) = self.uia_signal();
            if let (None, Some((w, base))) = (uia, self.list_items) {
                if count_list_items(&self.uia, w) > base {
                    uia = Some("uia-new-listitems");
                }
            }
            Sig {
                m32768,
                popups,
                winevent_open,
                uia,
                focus,
            }
        }

        fn uia_signal(&self) -> (Option<&'static str>, Option<Vec<i32>>) {
            let Ok(f) = self.uia.get_focused_element() else {
                return (None, None);
            };
            let rid = f.get_runtime_id().ok();
            if let Ok(ct) = f.get_control_type() {
                if matches!(ct, ControlType::MenuItem | ControlType::Menu) {
                    return (Some("uia-menu-focus"), rid);
                }
            }
            if let Ok(walker) = self.uia.get_raw_view_walker() {
                let mut el = f.clone();
                for _ in 0..5 {
                    if let Ok(p) =
                        el.get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>()
                    {
                        if matches!(p.get_state(), Ok(ExpandCollapseState::Expanded)) {
                            return (Some("uia-expanded"), rid);
                        }
                    }
                    match walker.get_parent(&el) {
                        Ok(p) => el = p,
                        Err(_) => break,
                    }
                }
            }
            (None, rid)
        }
    }

    // ---- menu cases --------------------------------------------------------

    struct Target {
        hwnd: Win,
        pids: HashSet<u32>,
    }

    type SetupFn = Box<dyn Fn(&Env) -> Option<Target>>;
    type OpenFn = Box<dyn Fn(&Env, &Target) -> bool>;

    struct Case {
        app: &'static str,
        menu: &'static str,
        setup: SetupFn,
        /// The menu is drawn inside the window: also count its list items.
        in_window: bool,
        open: OpenFn,
    }

    struct Env {
        uia: UIAutomation,
        own_edit: Win,
        vivaldi: Option<Win>,
        explorer: Option<Win>,
        settings: Option<Win>,
    }

    #[derive(Default)]
    struct Row {
        app: &'static str,
        menu: &'static str,
        signal: String,
        overlay_ok: Option<bool>,
        z_above: Option<bool>,
        hook_ok: Option<bool>,
        esc_leaked: Option<bool>,
        down_leaked: Option<bool>,
        hook_saw: Option<bool>,
        chord_hook_menu_ok: Option<bool>,
        chord_hook_seen: Option<bool>,
        chord_hook_hotkey: Option<bool>,
        chord_reg_menu_ok: Option<bool>,
        chord_reg_hotkey: Option<bool>,
        sub_above: Option<bool>,
        sub_above_after_reassert: Option<bool>,
        note: String,
    }

    fn yn(v: Option<bool>) -> &'static str {
        match v {
            Some(true) => "yes",
            Some(false) => "NO",
            None => "-",
        }
    }

    fn log_mark() -> usize {
        LOG.lock().unwrap().len()
    }

    fn log_since(m: usize) -> Vec<Rec> {
        LOG.lock().unwrap()[m..].to_vec()
    }

    fn reset_tracker() {
        *TRACKER.lock().unwrap() = Some(KeyTracker::new());
    }

    fn ensure_open(env: &Env, case: &Case, t: &Target, det: &Detector) -> bool {
        if det.sig().open() {
            return true;
        }
        esc_untagged(2);
        let _ = force_foreground(t.hwnd);
        (case.open)(env, t) && wait_open(det, 2500)
    }

    fn wait_open(det: &Detector, ms: u64) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            if det.sig().open() {
                return true;
            }
            sleep(100);
        }
        false
    }

    fn run_case(ht: &HookThread, env: &Env, case: &Case) -> Row {
        let mut row = Row {
            app: case.app,
            menu: case.menu,
            ..Default::default()
        };
        let Some(t) = (case.setup)(env) else {
            row.signal = "skipped".into();
            row.note = "target not available".into();
            return row;
        };
        if !force_foreground(t.hwnd) {
            row.note.push_str("foreground refused; ");
        }
        let det = Detector::new(&env.uia, t.pids.clone(), case.in_window.then_some(t.hwnd));
        if !((case.open)(env, &t) && wait_open(&det, 3000)) {
            row.signal = "unknown".into();
            let fg = foreground();
            let new: Vec<String> = top_windows()
                .into_iter()
                .filter(|w| !det.baseline.contains(w) && w.visible())
                .map(|w| {
                    format!(
                        "{} popup={} pid-match={}",
                        w.class(),
                        w.popup(),
                        w.pid() == t.hwnd.pid()
                    )
                })
                .collect();
            let focus = env
                .uia
                .get_focused_element()
                .ok()
                .and_then(|f| f.get_control_type().ok());
            row.note.push_str(&format!(
                "menu not detected after opening (foreground is target: {}; new windows {new:?}; focus {focus:?})",
                fg == t.hwnd
            ));
            esc_untagged(2);
            return row;
        }
        sleep(300);
        let s0 = det.sig();
        row.signal = s0.names();

        // A: overlay over the open menu.
        ht.show(t.hwnd.rect());
        sleep(500);
        let sa = det.sig();
        row.overlay_ok = Some(sa.open());
        if let Some(m) = sa.menu_window() {
            row.z_above = above(Win(OVERLAY.load(SeqCst)), m);
        }

        // B: hook in production mode; j and Down swallowed, then Esc.
        if sa.open() {
            reset_tracker();
            MODE.store(1, SeqCst);
            ht.cmd(CMD_HOOK_ON);
            sleep(100);
            let m = log_mark();
            let f_pre = det.sig().focus;
            sleep(400);
            let f0 = det.sig().focus;
            if f_pre != f0 {
                row.note.push_str("focus moved on its own; ");
            }
            tap_sc(SC_J, false, true);
            tap_sc(SC_DOWN, true, true);
            sleep(400);
            let sb1 = det.sig();
            row.down_leaked = match (&f0, &sb1.focus) {
                _ if f_pre != f0 => None,
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            };
            tap_sc(SC_ESC, false, true);
            sleep(500);
            let sb = det.sig();
            row.hook_ok = Some(sb1.open() && sb.open());
            row.esc_leaked = Some(sb1.open() && !sb.open());
            let recs = log_since(m);
            let seen_down = |k: Key| {
                recs.iter()
                    .any(|r| r.key == k && r.down && r.src == Src::Tagged)
            };
            row.hook_saw = Some(
                seen_down(Key::Named(NamedKey::Esc))
                    && seen_down(Key::Named(NamedKey::Down))
                    && recs.iter().any(|r| matches!(r.key, Key::Phys(_)) && r.down),
            );
            ht.cmd(CMD_HOOK_OFF);
        }

        // C: chord recognised by the hook, which swallows all of it.
        if ensure_open(env, case, &t, &det) {
            if !det.sig().open() {
                row.note.push_str("reopen failed before C; ");
            }
            reset_tracker();
            MODE.store(2, SeqCst);
            ht.cmd(CMD_HOOK_ON);
            sleep(100);
            let m = log_mark();
            let hk0 = HOTKEYS.load(SeqCst);
            chord_sc(
                &[(SC_CTRL, false), (SC_ALT, false), (SC_SHIFT, false)],
                SC_F9,
                false,
                true,
            );
            sleep(500);
            row.chord_hook_menu_ok = Some(det.sig().open());
            row.chord_hook_hotkey = Some(HOTKEYS.load(SeqCst) > hk0);
            row.chord_hook_seen = Some(log_since(m).iter().any(|r| {
                r.key == Key::Named(NamedKey::F(9)) && r.down && r.ctrl && r.alt && r.shift
            }));
            ht.cmd(CMD_HOOK_OFF);
        }

        // D: chord through RegisterHotKey; the production hook passes it.
        if ensure_open(env, case, &t, &det) {
            reset_tracker();
            MODE.store(1, SeqCst);
            ht.cmd(CMD_HOOK_ON);
            sleep(100);
            let hk0 = HOTKEYS.load(SeqCst);
            chord_sc(
                &[(SC_CTRL, false), (SC_ALT, false), (SC_SHIFT, false)],
                SC_F9,
                false,
                true,
            );
            sleep(500);
            row.chord_reg_menu_ok = Some(det.sig().open());
            row.chord_reg_hotkey = Some(HOTKEYS.load(SeqCst) > hk0);
            ht.cmd(CMD_HOOK_OFF);
        }

        // E: submenu opened after the overlay is shown (Right arrow, hook off).
        if ensure_open(env, case, &t, &det) {
            ht.show(t.hwnd.rect());
            let before: HashSet<Win> = top_windows().into_iter().filter(|w| w.visible()).collect();
            tap_sc(SC_DOWN, true, false);
            sleep(150);
            tap_sc(SC_RIGHT, true, false);
            sleep(600);
            let ov = Win(OVERLAY.load(SeqCst));
            let newest = top_windows().into_iter().find(|w| {
                !before.contains(w)
                    && w.visible()
                    && *w != ov
                    && (w.popup() || w.class() == "#32768")
            });
            if let Some(sub) = newest {
                row.sub_above = above(ov, sub);
                ht.cmd(CMD_TOPMOST);
                sleep(200);
                row.sub_above_after_reassert = above(ov, sub);
            } else {
                row.note.push_str("no submenu window; ");
            }
        }

        ht.cmd(CMD_HIDE);
        MODE.store(0, SeqCst);
        esc_untagged(3);
        row
    }

    /// Throwaway-profile Vivaldi, closed (whole process tree) on drop.
    struct Viv {
        child: Child,
        profile: std::path::PathBuf,
        win: Win,
    }

    impl Drop for Viv {
        fn drop(&mut self) {
            kill_tree(&mut self.child);
            sleep(500);
            let _ = std::fs::remove_dir_all(&self.profile);
        }
    }

    fn vivaldi(url: &str, profile_name: &str) -> Result<Viv> {
        let profile = scratch_dir(profile_name);
        let (child, win) = launch_vivaldi(url, &profile)?;
        Ok(Viv {
            child,
            profile,
            win,
        })
    }

    /// The fixture page's Document element (Vivaldi's own UI is a
    /// Document too, so match the page by its `file:` value; the value is
    /// only compared, never printed). Chromium builds it lazily: retry.
    fn find_document(uia: &UIAutomation, w: Win) -> Option<UIElement> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(8) {
            if let (Ok(root), Ok(cond)) = (
                uia.element_from_handle(Handle::from(w.0)),
                uia.create_property_condition(
                    UIProperty::ControlType,
                    Variant::from(ControlType::Document as i32),
                    None,
                ),
            ) {
                for d in root
                    .find_all(TreeScope::Descendants, &cond)
                    .unwrap_or_default()
                {
                    let v = d
                        .get_property_value(UIProperty::ValueValue)
                        .ok()
                        .and_then(|v| v.get_string().ok())
                        .unwrap_or_default();
                    if v.starts_with("file:") {
                        return Some(d);
                    }
                }
            }
            sleep(250);
        }
        None
    }

    /// Open the menu, put the overlay and the production hook over it, wait
    /// `secs` doing nothing (polling the menu once a second), then check the
    /// menu, the z-order and that the hook still swallows keys.
    fn run_wait_case(ht: &HookThread, env: &Env, case: &Case, secs: u64) -> String {
        let head = format!("| {} | {} |", case.app, case.menu);
        let Some(t) = (case.setup)(env) else {
            return format!("{head} skipped | - | - | - | - | - | - | - | target not available |");
        };
        let mut note = String::new();
        if !force_foreground(t.hwnd) {
            note.push_str("foreground refused; ");
        }
        let det = Detector::new(&env.uia, t.pids.clone(), case.in_window.then_some(t.hwnd));
        if !((case.open)(env, &t) && wait_open(&det, 3000)) {
            esc_untagged(2);
            return format!("{head} unknown | - | - | - | - | - | - | - | menu not detected |");
        }
        sleep(300);
        let s0 = det.sig();
        let ov = Win(OVERLAY.load(SeqCst));
        ht.show(t.hwnd.rect());
        reset_tracker();
        MODE.store(1, SeqCst);
        ht.cmd(CMD_HOOK_ON);
        sleep(300);
        let z0 = det.sig().menu_window().and_then(|m| above(ov, m));
        let fg0 = foreground();
        let mut fg_same = true;
        let mut closed_at = None;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if closed_at.is_none() && !det.sig().open() {
                closed_at = Some(t0.elapsed().as_secs());
            }
            if foreground() != fg0 {
                fg_same = false;
            }
            sleep(1000);
        }
        let s1 = det.sig();
        let open_after = s1.open();
        let z1 = s1.menu_window().and_then(|m| above(ov, m));
        // Keys after the wait.
        let m = log_mark();
        tap_sc(SC_J, false, true);
        tap_sc(SC_DOWN, true, true);
        sleep(300);
        tap_sc(SC_ESC, false, true);
        sleep(500);
        let recs = log_since(m);
        let saw = recs
            .iter()
            .any(|r| r.key == Key::Named(NamedKey::Esc) && r.down)
            && recs
                .iter()
                .any(|r| r.key == Key::Named(NamedKey::Down) && r.down);
        let esc_leaked = open_after.then(|| !det.sig().open());
        let m = log_mark();
        tap_sc(SC_F24, false, true);
        sleep(300);
        let sentinel = log_since(m)
            .iter()
            .any(|r| r.key == Key::Named(NamedKey::F(24)));
        ht.cmd(CMD_HIDE);
        ht.cmd(CMD_HOOK_OFF);
        MODE.store(0, SeqCst);
        esc_untagged(3);
        if s0.names() != s1.names() && open_after {
            note.push_str(&format!("signals changed to {}; ", s1.names()));
        }
        format!(
            "{head} {} | {} | {} | {} | {} / {} | {} | {} | {} | {} |",
            s0.names(),
            yn(Some(open_after)),
            closed_at.map(|c| format!("{c} s")).unwrap_or("-".into()),
            yn(Some(fg_same)),
            yn(z0),
            yn(z1),
            yn(Some(saw)),
            yn(esc_leaked),
            yn(Some(sentinel)),
            note.trim_end_matches("; ")
        )
    }

    fn launch_vivaldi(url: &str, profile: &std::path::Path) -> Result<(Child, Win)> {
        let exe = std::env::var("LOCALAPPDATA")? + r"\Vivaldi\Application\vivaldi.exe";
        let before: HashSet<Win> = top_windows().into_iter().collect();
        let child = Command::new(exe)
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--new-window",
                "--window-position=80,80",
                "--window-size=1600,1000",
                url,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("starting Vivaldi")?;
        let w = wait_new_window(&before, 20, |w| {
            let r = w.rect();
            w.class() == "Chrome_WidgetWin_1" && r.right - r.left > 800
        })
        .context("no Vivaldi window")?;
        Ok((child, w))
    }

    fn kill_tree(c: &mut Child) {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &c.id().to_string()])
            .output();
        let _ = c.wait();
    }

    fn fixture_url(name: &str) -> String {
        let root = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
        format!("file:///{root}/bench/fixtures/{name}")
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("jk-hook-spike").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn find_first(uia: &UIAutomation, root: &UIElement, ct: ControlType) -> Option<UIElement> {
        let cond = uia
            .create_property_condition(UIProperty::ControlType, Variant::from(ct as i32), None)
            .ok()?;
        root.find_first(TreeScope::Descendants, &cond).ok()
    }

    fn expand(el: &UIElement) -> bool {
        el.get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>()
            .and_then(|p| p.expand())
            .is_ok()
    }

    fn pids_of(w: Win) -> HashSet<u32> {
        [w.pid()].into_iter().collect()
    }

    // ---- own test window (Win32 EDIT on its own thread) ----------------------

    fn spawn_own_edit() -> Win {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            let frame = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                PCWSTR::null(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WINDOW_STYLE(ES_MULTILINE as u32),
                200,
                200,
                700,
                450,
                None,
                None,
                hinst,
                None,
            )
            .unwrap();
            let _ = tx.send(Win(frame.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        rx.recv().unwrap()
    }

    fn text_len(w: Win) -> i32 {
        unsafe { GetWindowTextLengthW(w.h()) }
    }

    fn clear_text(w: Win) {
        unsafe {
            let _ = SendMessageW(
                w.h(),
                WM_SETTEXT,
                WPARAM(0),
                LPARAM(w!("").as_ptr() as isize),
            );
        }
    }

    // ---- subcommands -----------------------------------------------------------

    pub fn run() -> Result<()> {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("menus") => menus(args.get(1).map(String::as_str), None),
            Some("menus-wait") => {
                let secs: u64 = args.get(1).context("menus-wait SECS [filter]")?.parse()?;
                menus(args.get(2).map(String::as_str), Some(secs))
            }
            Some("stall") => stall(),
            Some("probe-viv") => probe_viv(),
            Some("scroll-areas") => scroll_areas(&args[1..]),
            _ => bail!("usage: hook_spike menus | stall | scroll-areas (--fixtures | --wait SECS)"),
        }
    }

    fn baseline(ht: &HookThread, edit: Win) {
        println!("== baseline (own EDIT window, no menu)");
        let _ = force_foreground(edit);
        clear_text(edit);
        reset_tracker();
        MODE.store(1, SeqCst);
        ht.cmd(CMD_HOOK_ON);
        sleep(100);
        let m = log_mark();
        for _ in 0..50 {
            tap_sc(SC_J, false, true);
        }
        sleep(300);
        let swallowed_len = text_len(edit);
        tap_sc(SC_J, false, false);
        sleep(300);
        let untagged_len = text_len(edit);
        // Auto-repeat: three downs, one up.
        for _ in 0..3 {
            send_one(key_input(VIRTUAL_KEY(0), SC_K, false, false, true));
            sleep(30);
        }
        send_one(key_input(VIRTUAL_KEY(0), SC_K, false, true, true));
        // Right Alt (AltGr on some layouts) and a VK-only key (scan code 0).
        tap_sc(SC_ALT, true, true);
        send_one(key_input(VK_F10, 0, false, false, true));
        send_one(key_input(VK_F10, 0, false, true, true));
        sleep(300);
        ht.cmd(CMD_HOOK_OFF);
        MODE.store(0, SeqCst);
        let recs = log_since(m);
        let mut lat: Vec<u64> = recs
            .iter()
            .filter(|r| {
                r.src == Src::Tagged
                    && r.key
                        == Key::Phys(jumpkey::keyboard::PhysKey::new(
                            jumpkey::keyboard::Row::Home,
                            6,
                        ))
            })
            .filter_map(|r| r.latency_us)
            .collect();
        lat.sort_unstable();
        let pct = |p: f64| {
            lat.get(((lat.len() as f64 - 1.0) * p) as usize)
                .copied()
                .unwrap_or(0)
        };
        println!(
            "  tagged j x50 swallowed: text len {swallowed_len} (want 0); untagged j passed: len {untagged_len} (want 1)"
        );
        println!(
            "  hook latency (send -> callback), {} events: p50 {} us, p95 {} us, max {} us",
            lat.len(),
            pct(0.5),
            pct(0.95),
            lat.last().copied().unwrap_or(0)
        );
        let k = Key::Phys(jumpkey::keyboard::PhysKey::new(
            jumpkey::keyboard::Row::Home,
            7,
        ));
        let kd: Vec<bool> = recs
            .iter()
            .filter(|r| r.key == k && r.down)
            .map(|r| r.repeat)
            .collect();
        println!("  k down x3 repeat flags: {kd:?} (want [false, true, true])");
        let alt: Vec<String> = recs
            .iter()
            .skip_while(|r| r.key != Key::Named(NamedKey::Alt))
            .take(2)
            .map(|r| format!("{:?} down={} ctrl={} alt={}", r.key, r.down, r.ctrl, r.alt))
            .collect();
        println!("  right Alt arrives as: {alt:?}");
        let f10 = recs.iter().any(|r| r.key == Key::Named(NamedKey::F(10)));
        println!(
            "  VK-only F10 (scan code 0) mapped to F(10): {}",
            yn(Some(f10))
        );
        // The right-Alt tap passed through (modifiers aren't swallowed) and
        // a lone Alt tap puts the window in menu-bar mode: leave it.
        esc_untagged(1);
        let untagged = recs.iter().filter(|r| r.src == Src::Untagged).count();
        let untagged_swallowed = recs
            .iter()
            .filter(|r| r.src == Src::Untagged && r.swallowed)
            .count();
        let real = recs.iter().filter(|r| r.src == Src::Real).count();
        println!("  untagged injected seen {untagged}, swallowed {untagged_swallowed} (want 0); real keys during run {real}");
    }

    fn menus(only: Option<&str>, wait: Option<u64>) -> Result<()> {
        let uia = UIAutomation::new()?;
        let ht = HookThread::start();
        println!(
            "RegisterHotKey(Ctrl+Alt+Shift+F9): {}",
            if ht.hotkey_registered { "ok" } else { "FAILED" }
        );
        let own_edit = spawn_own_edit();
        sleep(500);
        baseline(&ht, own_edit);

        let notepad_running = top_windows().iter().any(|w| w.class() == "Notepad");

        // Explorer on the fixture folder.
        let before: HashSet<Win> = top_windows().into_iter().collect();
        let folder = concat!(env!("CARGO_MANIFEST_DIR"), r"\bench\fixtures\folder");
        let _ = Command::new("explorer.exe").arg(folder).spawn();
        let explorer = wait_new_window(&before, 15, |w| w.class() == "CabinetWClass");
        sleep(1500);

        // Vivaldi on the form fixture.
        let viv = vivaldi(&fixture_url("form.html"), "vivaldi-menus").ok();
        sleep(5000);
        if let Some(v) = &viv {
            let _ = find_document(&uia, v.win);
        }

        // Settings, on a page with a harmless dropdown.
        let settings_was_visible = top_windows().iter().any(|w| {
            w.visible()
                && w.class() == "ApplicationFrameWindow"
                && w.pid() != 0
                && is_settings(&uia, *w)
        });
        let before: HashSet<Win> = top_windows().into_iter().filter(|w| w.visible()).collect();
        let _ = Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:easeofaccess-visualeffects"])
            .spawn();
        let settings = wait_new_window(&before, 15, |w| w.class() == "ApplicationFrameWindow")
            .or_else(|| {
                top_windows().into_iter().find(|w| {
                    w.visible() && w.class() == "ApplicationFrameWindow" && is_settings(&uia, *w)
                })
            });
        sleep(3000);

        let env = Env {
            uia: uia.clone(),
            own_edit,
            vivaldi: viv.as_ref().map(|v| v.win),
            explorer,
            settings,
        };

        let shift_f10 = |_: &Env, _: &Target| {
            chord_sc(&[(SC_SHIFT, false)], SC_F10, false, false);
            true
        };
        let alt_space = |_: &Env, _: &Target| {
            chord_sc(&[(SC_ALT, false)], SC_SPACE, false, false);
            true
        };
        let cases: Vec<Case> = vec![
            Case {
                app: "own Win32",
                menu: "EDIT context",
                setup: Box::new(|e| {
                    Some(Target {
                        hwnd: e.own_edit,
                        pids: HashSet::new(),
                    })
                }),
                in_window: false,
                open: Box::new(|_, t| {
                    // A top-level EDIT ignores Shift+F10 and the Apps key;
                    // WM_CONTEXTMENU at the keyboard position runs the
                    // same TrackPopupMenu loop as a right-click.
                    unsafe {
                        let _ = PostMessageW(
                            t.hwnd.h(),
                            WM_CONTEXTMENU,
                            WPARAM(t.hwnd.0 as usize),
                            LPARAM(-1),
                        );
                    }
                    true
                }),
            },
            Case {
                app: "own Win32",
                menu: "system menu",
                setup: Box::new(|e| {
                    Some(Target {
                        hwnd: e.own_edit,
                        pids: HashSet::new(),
                    })
                }),
                in_window: false,
                open: Box::new(alt_space),
            },
            Case {
                app: "Explorer",
                menu: "folder context",
                setup: Box::new(|e| {
                    e.explorer.map(|w| Target {
                        hwnd: w,
                        pids: pids_of(w),
                    })
                }),
                in_window: false,
                open: Box::new(shift_f10),
            },
            Case {
                app: "Explorer",
                menu: "system menu",
                setup: Box::new(|e| {
                    e.explorer.map(|w| Target {
                        hwnd: w,
                        pids: pids_of(w),
                    })
                }),
                in_window: false,
                open: Box::new(alt_space),
            },
            Case {
                app: "Desktop",
                menu: "context",
                setup: Box::new(|_| {
                    desktop_list().map(|w| Target {
                        hwnd: w,
                        pids: pids_of(w),
                    })
                }),
                in_window: false,
                open: Box::new(shift_f10),
            },
            Case {
                app: "Vivaldi",
                menu: "page context",
                setup: Box::new(|e| {
                    e.vivaldi.map(|w| Target {
                        hwnd: w,
                        pids: HashSet::new(),
                    })
                }),
                in_window: false,
                open: Box::new(|e, t| {
                    // Focus the page body first.
                    if let Some(doc) = find_document(&e.uia, t.hwnd) {
                        let _ = doc.set_focus();
                    }
                    sleep(200);
                    chord_sc(&[(SC_SHIFT, false)], SC_F10, false, false);
                    true
                }),
            },
            Case {
                app: "Vivaldi",
                menu: "<select>",
                setup: Box::new(|e| {
                    e.vivaldi.map(|w| Target {
                        hwnd: w,
                        pids: HashSet::new(),
                    })
                }),
                in_window: false,
                open: Box::new(|e, t| {
                    let Some(doc) = find_document(&e.uia, t.hwnd) else {
                        return false;
                    };
                    let Some(cb) = find_first(&e.uia, &doc, ControlType::ComboBox) else {
                        return false;
                    };
                    let _ = cb.set_focus();
                    sleep(150);
                    // Alt+Down opens a focused <select>, like a user would.
                    chord_sc(&[(SC_ALT, false)], SC_DOWN, true, false);
                    sleep(300);
                    true
                }),
            },
            Case {
                app: "Vivaldi",
                menu: "address suggestions",
                setup: Box::new(|e| {
                    e.vivaldi.map(|w| Target {
                        hwnd: w,
                        pids: HashSet::new(),
                    })
                }),
                in_window: true,
                open: Box::new(|_, _| {
                    chord_sc(&[(SC_CTRL, false)], SC_L, false, false);
                    sleep(300);
                    tap_sc(SC_A, false, false);
                    sleep(600);
                    true
                }),
            },
            Case {
                app: "Settings",
                menu: "ComboBox flyout",
                setup: Box::new(|e| {
                    e.settings.map(|w| Target {
                        hwnd: w,
                        pids: HashSet::new(),
                    })
                }),
                in_window: false,
                open: Box::new(|e, t| {
                    let Ok(root) = e.uia.element_from_handle(Handle::from(t.hwnd.0)) else {
                        return false;
                    };
                    let Some(cb) = find_first(&e.uia, &root, ControlType::ComboBox) else {
                        return false;
                    };
                    let _ = cb.set_focus();
                    sleep(150);
                    expand(&cb)
                }),
            },
        ];

        if notepad_running {
            println!("Notepad cases skipped: your Notepad is open (the spike won't touch it).");
        }
        let mut rows = Vec::new();
        let mut wait_rows = Vec::new();
        for c in cases
            .iter()
            .filter(|c| only.map_or(true, |o| c.app.contains(o)))
        {
            eprintln!("-- {} / {}", c.app, c.menu);
            match wait {
                Some(secs) => wait_rows.push(run_wait_case(&ht, &env, c, secs)),
                None => rows.push(run_case(&ht, &env, c)),
            }
        }

        // Cleanup.
        if let Some(w) = explorer {
            unsafe {
                let _ = PostMessageW(w.h(), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        drop(viv);
        if let (Some(w), false) = (settings, settings_was_visible) {
            unsafe {
                let _ = PostMessageW(w.h(), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        unsafe {
            let _ = PostMessageW(own_edit.h(), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        drop(ht);

        if let Some(secs) = wait {
            println!();
            println!("| app | menu | open signal | menu open after {secs} s | closed after | foreground unchanged | overlay above menu (start / end) | then: hook saw keys | Esc leaked | sentinel seen | note |");
            println!("|---|---|---|---|---|---|---|---|---|---|---|");
            for r in &wait_rows {
                println!("{r}");
            }
            return Ok(());
        }
        println!();
        println!("| app | menu | open signal | survives overlay | overlay above menu | survives hook | Esc leaked | Down leaked | hook saw keys | hook-chord: menu ok / hook saw / WM_HOTKEY | RegisterHotKey: menu ok / WM_HOTKEY | submenu above overlay? / after re-assert | note |");
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for r in &rows {
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} / {} / {} | {} / {} | {} / {} | {} |",
                r.app,
                r.menu,
                r.signal,
                yn(r.overlay_ok),
                yn(r.z_above),
                yn(r.hook_ok),
                yn(r.esc_leaked),
                yn(r.down_leaked),
                yn(r.hook_saw),
                yn(r.chord_hook_menu_ok),
                yn(r.chord_hook_seen),
                yn(r.chord_hook_hotkey),
                yn(r.chord_reg_menu_ok),
                yn(r.chord_reg_hotkey),
                yn(r.sub_above.map(|a| !a)),
                yn(r.sub_above_after_reassert.map(|a| !a)),
                r.note.trim_end_matches("; ")
            );
        }
        Ok(())
    }

    fn is_settings(uia: &UIAutomation, w: Win) -> bool {
        // SystemSettings.exe hosts the content inside the frame window.
        let Ok(el) = uia.element_from_handle(Handle::from(w.0)) else {
            return false;
        };
        let Ok(cond) = uia.create_true_condition() else {
            return false;
        };
        el.find_all(TreeScope::Children, &cond)
            .map(|v| {
                v.iter().any(|c| {
                    c.get_process_id()
                        .ok()
                        .and_then(exe_of)
                        .is_some_and(|n| n.eq_ignore_ascii_case("SystemSettings.exe"))
                })
            })
            .unwrap_or(false)
    }

    fn exe_of(pid: u32) -> Option<String> {
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 512];
            let mut n = buf.len() as u32;
            QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut n,
            )
            .ok()?;
            let _ = windows::Win32::Foundation::CloseHandle(h);
            let s = String::from_utf16_lossy(&buf[..n as usize]);
            s.rsplit('\\').next().map(str::to_string)
        }
    }

    /// The desktop icon list (`SysListView32` under `SHELLDLL_DefView`).
    fn desktop_list() -> Option<Win> {
        for w in top_windows() {
            let c = w.class();
            if c != "Progman" && c != "WorkerW" {
                continue;
            }
            unsafe {
                if let Ok(def) = FindWindowExW(w.h(), None, w!("SHELLDLL_DefView"), None) {
                    if !def.0.is_null() {
                        if let Ok(lv) = FindWindowExW(def, None, w!("SysListView32"), None) {
                            if !lv.0.is_null() {
                                return Some(Win(lv.0 as isize));
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// What do Vivaldi's <select> and address suggestions create? Prints
    /// window classes/styles and focused control types only.
    fn probe_viv() -> Result<()> {
        let uia = UIAutomation::new()?;
        let viv = vivaldi(&fixture_url("form.html"), "vivaldi-probe")?;
        let w = viv.win;
        sleep(5000);
        let _ = force_foreground(w);
        let dump = |label: &str, before: &HashSet<Win>| {
            let new: Vec<String> = top_windows()
                .into_iter()
                .filter(|x| !before.contains(x) && x.visible())
                .map(|x| {
                    let r = x.rect();
                    format!(
                        "{} popup={} {}x{}",
                        x.class(),
                        x.popup(),
                        r.right - r.left,
                        r.bottom - r.top
                    )
                })
                .collect();
            let focus = uia
                .get_focused_element()
                .ok()
                .and_then(|f| f.get_control_type().ok());
            println!(
                "{label}: new windows {new:?}; focus {focus:?}; menu events {}/{}",
                MENU_START.load(SeqCst),
                MENU_END.load(SeqCst)
            );
        };
        let ht = HookThread::start();
        let root = uia.element_from_handle(Handle::from(w.0))?;
        let doc = find_document(&uia, w).context("no document")?;
        let cb = find_first(&uia, &doc, ControlType::ComboBox).context("no combobox")?;
        let _ = cb.set_focus();
        sleep(300);
        let before: HashSet<Win> = top_windows().into_iter().filter(|x| x.visible()).collect();
        MENU_START.store(0, SeqCst);
        MENU_END.store(0, SeqCst);
        chord_sc(&[(SC_ALT, false)], SC_DOWN, true, false);
        sleep(800);
        dump("select Alt+Down", &before);
        let state = cb
            .get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>()
            .and_then(|p| p.get_state());
        println!("  combobox state {state:?}");
        esc_untagged(1);
        sleep(300);
        MENU_START.store(0, SeqCst);
        MENU_END.store(0, SeqCst);
        let ok = expand(&cb);
        sleep(800);
        dump(&format!("select UIA expand ok={ok}"), &before);
        esc_untagged(1);
        sleep(300);
        MENU_START.store(0, SeqCst);
        MENU_END.store(0, SeqCst);
        chord_sc(&[(SC_CTRL, false)], SC_L, false, false);
        sleep(300);
        tap_sc(SC_A, false, false);
        sleep(1000);
        dump("address Ctrl+L a", &before);
        if let Ok(f) = uia.get_focused_element() {
            if let Ok(walker) = uia.get_raw_view_walker() {
                let mut el = f;
                let mut chain = Vec::new();
                for _ in 0..6 {
                    let ct = el.get_control_type().ok();
                    let exp = el
                        .get_pattern::<uiautomation::patterns::UIExpandCollapsePattern>()
                        .and_then(|p| p.get_state())
                        .ok();
                    chain.push(format!("{ct:?}/{exp:?}"));
                    match walker.get_parent(&el) {
                        Ok(p) => el = p,
                        Err(_) => break,
                    }
                }
                println!("  focus chain {chain:?}");
            }
            // Any List/ListItem/Menu near the top of the window?
            for ct in [ControlType::List, ControlType::Menu, ControlType::ListItem] {
                let n = find_first(&uia, &root, ct).is_some();
                println!("  window has {ct:?}: {n}");
            }
        }
        esc_untagged(2);
        drop(ht);
        drop(viv);
        Ok(())
    }

    // ---- stall -----------------------------------------------------------------

    fn stall() -> Result<()> {
        let ht = HookThread::start();
        let edit = spawn_own_edit();
        sleep(500);
        println!(
            "LowLevelHooksTimeout registry value: {}",
            hooks_timeout_reg()
        );
        println!("| stall | stalled key leaked | leak delay | later keys reach hook | later keys swallowed | sentinel seen | reinstall: sentinel seen / keys swallowed |");
        println!("|---|---|---|---|---|---|---|");
        for ms in [300u32, 1000, 5000] {
            let _ = force_foreground(edit);
            clear_text(edit);
            reset_tracker();
            MODE.store(1, SeqCst);
            ht.cmd(CMD_HOOK_ON);
            sleep(150);
            // Stalled key.
            STALL_MS.store(ms, SeqCst);
            let t0 = Instant::now();
            send_one(key_input(VIRTUAL_KEY(0), SC_J, false, false, true));
            let mut leak_at = None;
            while t0.elapsed() < Duration::from_millis(ms as u64 + 1500) {
                if leak_at.is_none() && text_len(edit) > 0 {
                    leak_at = Some(t0.elapsed().as_millis());
                }
                sleep(10);
            }
            send_one(key_input(VIRTUAL_KEY(0), SC_J, false, true, true));
            sleep(200);
            let leaked = text_len(edit) > 0;
            // Later keys.
            clear_text(edit);
            let m = log_mark();
            for _ in 0..5 {
                tap_sc(SC_K, false, true);
            }
            sleep(400);
            let reached = log_since(m)
                .iter()
                .filter(|r| r.down && r.src == Src::Tagged)
                .count();
            let later_swallowed = text_len(edit) == 0;
            // Sentinel: a tagged F24 tap the hook should see.
            let m = log_mark();
            tap_sc(SC_F24, false, true);
            sleep(300);
            let sentinel = log_since(m)
                .iter()
                .any(|r| r.key == Key::Named(NamedKey::F(24)));
            // Reinstall and retry.
            ht.cmd(CMD_HOOK_OFF);
            ht.cmd(CMD_HOOK_ON);
            sleep(150);
            clear_text(edit);
            let m = log_mark();
            tap_sc(SC_F24, false, true);
            for _ in 0..3 {
                tap_sc(SC_K, false, true);
            }
            sleep(400);
            let sentinel2 = log_since(m)
                .iter()
                .any(|r| r.key == Key::Named(NamedKey::F(24)));
            let swallowed2 = text_len(edit) == 0;
            ht.cmd(CMD_HOOK_OFF);
            MODE.store(0, SeqCst);
            println!(
                "| {ms} ms | {} | {} | {reached}/5 | {} | {} | {} / {} |",
                yn(Some(leaked)),
                leak_at.map(|t| format!("{t} ms")).unwrap_or("-".into()),
                yn(Some(later_swallowed)),
                yn(Some(sentinel)),
                yn(Some(sentinel2)),
                yn(Some(swallowed2)),
            );
            sleep(500);
        }
        // Repeated timeouts: N stalled keys in a row, then is the hook alive?
        println!();
        println!("| repeated 500 ms stalls | sentinel seen after | keys swallowed after |");
        println!("|---|---|---|");
        for n in [3usize, 10, 20] {
            let _ = force_foreground(edit);
            reset_tracker();
            MODE.store(1, SeqCst);
            ht.cmd(CMD_HOOK_ON);
            sleep(150);
            for _ in 0..n {
                STALL_MS.store(500, SeqCst);
                tap_sc(SC_J, false, true);
                sleep(700);
            }
            sleep(500);
            clear_text(edit);
            let m = log_mark();
            tap_sc(SC_F24, false, true);
            for _ in 0..3 {
                tap_sc(SC_K, false, true);
            }
            sleep(400);
            let sentinel = log_since(m)
                .iter()
                .any(|r| r.key == Key::Named(NamedKey::F(24)));
            let swallowed = text_len(edit) == 0;
            ht.cmd(CMD_HOOK_OFF);
            MODE.store(0, SeqCst);
            println!("| {n} | {} | {} |", yn(Some(sentinel)), yn(Some(swallowed)));
        }
        unsafe {
            let _ = PostMessageW(edit.h(), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        let _ = QUIET.load(SeqCst);
        Ok(())
    }

    fn hooks_timeout_reg() -> String {
        let out = Command::new("reg")
            .args([
                "query",
                r"HKCU\Control Panel\Desktop",
                "/v",
                "LowLevelHooksTimeout",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|l| l.contains("LowLevelHooksTimeout"))
                .and_then(|l| l.split_whitespace().last())
                .map(|v| {
                    let n = i64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(-1);
                    format!("{n} ms")
                })
                .unwrap_or_else(|| "unreadable".into()),
            _ => "not set (system default)".into(),
        }
    }

    // ---- scroll areas ------------------------------------------------------------

    #[derive(Debug, Clone, Copy)]
    struct Area {
        w: i32,
        h: i32,
        hs: bool,
        vs: bool,
    }

    fn cached_bool(el: &UIElement, p: UIProperty) -> bool {
        el.get_cached_property_value(p)
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or(false)
    }

    fn live_bool(el: &UIElement, p: UIProperty) -> bool {
        el.get_property_value(p)
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or(false)
    }

    fn area_of(el: &UIElement, cached: bool) -> Area {
        let r = if cached {
            el.get_cached_bounding_rectangle()
        } else {
            el.get_bounding_rectangle()
        }
        .ok();
        let (w, h) = r.map(|r| (r.get_width(), r.get_height())).unwrap_or((0, 0));
        let b = |p| {
            if cached {
                cached_bool(el, p)
            } else {
                live_bool(el, p)
            }
        };
        Area {
            w,
            h,
            hs: b(UIProperty::ScrollHorizontallyScrollable),
            vs: b(UIProperty::ScrollVerticallyScrollable),
        }
    }

    struct Measure {
        findall_ms: f64,
        findall_with_pattern: usize,
        findall_areas: Vec<Area>,
        tree_ms: f64,
        tree_nodes: usize,
        tree_areas: Vec<Area>,
        point_ms: f64,
        point_depth: Option<usize>,
        point_area: Option<Area>,
    }

    fn measure(uia: &UIAutomation, hwnd: Win, point: POINT) -> Result<Measure> {
        let props = [
            UIProperty::BoundingRectangle,
            UIProperty::IsScrollPatternAvailable,
            UIProperty::ScrollHorizontallyScrollable,
            UIProperty::ScrollVerticallyScrollable,
        ];
        // 1. FindAll(Descendants, IsScrollPatternAvailable).
        let req = uia.create_cache_request()?;
        for p in props {
            req.add_property(p)?;
        }
        let t = Instant::now();
        let root = uia.element_from_handle(Handle::from(hwnd.0))?;
        let cond = uia.create_property_condition(
            UIProperty::IsScrollPatternAvailable,
            Variant::from(true),
            None,
        )?;
        let found = root
            .find_all_build_cache(TreeScope::Descendants, &cond, &req)
            .unwrap_or_default();
        let findall_ms = t.elapsed().as_secs_f64() * 1000.0;
        let findall_areas: Vec<Area> = found
            .iter()
            .map(|e| area_of(e, true))
            .filter(|a| a.hs || a.vs)
            .collect();

        // 2. One-call control-view subtree fetch, filtered in memory.
        let req2 = uia.create_cache_request()?;
        for p in props {
            req2.add_property(p)?;
        }
        req2.set_tree_scope(TreeScope::Subtree)?;
        req2.set_tree_filter(uia.get_control_view_condition()?)?;
        let t = Instant::now();
        let mut tree_nodes = 0;
        let mut tree_areas = Vec::new();
        if let Ok(root) = uia.element_from_handle_build_cache(Handle::from(hwnd.0), &req2) {
            let mut stack = vec![root];
            while let Some(el) = stack.pop() {
                tree_nodes += 1;
                if cached_bool(&el, UIProperty::IsScrollPatternAvailable) {
                    let a = area_of(&el, true);
                    if a.hs || a.vs {
                        tree_areas.push(a);
                    }
                }
                if let Ok(kids) = el.get_cached_children() {
                    stack.extend(kids);
                }
            }
        }
        let tree_ms = t.elapsed().as_secs_f64() * 1000.0;

        // 3. ElementFromPoint + walk up to the nearest scrollable ancestor.
        let t = Instant::now();
        let mut point_depth = None;
        let mut point_area = None;
        if let (Ok(mut el), Ok(walker)) = (
            uia.element_from_point(Point::new(point.x, point.y)),
            uia.get_raw_view_walker(),
        ) {
            for d in 0..40 {
                if live_bool(&el, UIProperty::IsScrollPatternAvailable) {
                    let a = area_of(&el, false);
                    if a.hs || a.vs {
                        point_depth = Some(d);
                        point_area = Some(a);
                        break;
                    }
                }
                match walker.get_parent(&el) {
                    Ok(p) => el = p,
                    Err(_) => break,
                }
            }
        }
        let point_ms = t.elapsed().as_secs_f64() * 1000.0;
        Ok(Measure {
            findall_ms,
            findall_with_pattern: found.len(),
            findall_areas,
            tree_ms,
            tree_nodes,
            tree_areas,
            point_ms,
            point_depth,
            point_area,
        })
    }

    fn fmt_areas(v: &[Area]) -> String {
        let mut s: Vec<String> = v
            .iter()
            .map(|a| {
                let axis = match (a.hs, a.vs) {
                    (true, true) => "HV",
                    (true, false) => "H",
                    _ => "V",
                };
                format!("{}x{}{}", (a.w + 5) / 10 * 10, (a.h + 5) / 10 * 10, axis)
            })
            .collect();
        s.sort();
        s.join(" ")
    }

    fn report(label: &str, runs: &[Measure]) {
        let med = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let first = &runs[0];
        let last = runs.last().unwrap();
        println!("== {label} ({} runs)", runs.len());
        println!(
            "  FindAll(ScrollPattern): first {:.0} ms, median {:.0} ms; {} with pattern, {} scrollable: {}",
            first.findall_ms,
            med(runs.iter().map(|m| m.findall_ms).collect()),
            last.findall_with_pattern,
            last.findall_areas.len(),
            fmt_areas(&last.findall_areas)
        );
        println!(
            "  one-call tree:          first {:.0} ms, median {:.0} ms; {} nodes, {} scrollable: {}",
            first.tree_ms,
            med(runs.iter().map(|m| m.tree_ms).collect()),
            last.tree_nodes,
            last.tree_areas.len(),
            fmt_areas(&last.tree_areas)
        );
        println!(
            "  point + walk up:        first {:.0} ms, median {:.0} ms; depth {:?}, area {}",
            first.point_ms,
            med(runs.iter().map(|m| m.point_ms).collect()),
            last.point_depth,
            last.point_area
                .map(|a| fmt_areas(&[a]))
                .unwrap_or("-".into())
        );
        let counts: Vec<usize> = runs.iter().map(|m| m.findall_areas.len()).collect();
        println!("  scrollable count per run (FindAll): {counts:?}");
    }

    fn scroll_areas(args: &[String]) -> Result<()> {
        let uia = UIAutomation::new()?;
        if args.first().map(String::as_str) == Some("--fixtures") {
            let names: Vec<String> = match args.get(1) {
                Some(list) => list.split(',').map(str::to_string).collect(),
                None => ["scroll-areas.html", "dense-list.html", "long-feed.html"]
                    .map(String::from)
                    .to_vec(),
            };
            for name in names.iter().map(String::as_str) {
                let viv = vivaldi(&fixture_url(name), "vivaldi-scroll")?;
                let w = viv.win;
                sleep(6000);
                let _ = find_document(&uia, w);
                let r = w.rect();
                // A point inside the sidebar on scroll-areas, else the window centre.
                let point = if name.starts_with("scroll-") {
                    POINT {
                        x: r.left + 140,
                        y: r.top + 400,
                    }
                } else {
                    POINT {
                        x: (r.left + r.right) / 2,
                        y: (r.top + r.bottom) / 2,
                    }
                };
                let runs: Vec<Measure> = (0..5)
                    .filter_map(|_| measure(&uia, w, point).ok())
                    .collect();
                drop(viv);
                if runs.is_empty() {
                    println!("== {name}: no measurement");
                } else {
                    report(name, &runs);
                }
            }
        } else {
            let secs: u64 = match args {
                [flag, n] if flag == "--wait" => n.parse()?,
                _ => bail!("scroll-areas --fixtures | --wait SECS"),
            };
            println!("Focus the window to measure and leave the mouse over a scroll area; measuring in {secs} s...");
            sleep(secs * 1000);
            let w = foreground();
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            let runs: Vec<Measure> = (0..5).filter_map(|_| measure(&uia, w, p).ok()).collect();
            if runs.is_empty() {
                bail!("no measurement");
            }
            report(&format!("foreground window (class {})", w.class()), &runs);
        }
        Ok(())
    }
}
