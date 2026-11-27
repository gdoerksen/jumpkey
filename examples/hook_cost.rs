//! What does "install the keyboard hook only during a pick" cost?
//! (Roadmap step 6.)
//!
//! Step 6 keeps one long-lived input thread and installs `WH_KEYBOARD_LL`
//! plus `WH_MOUSE_LL` when a pick or mode starts, removing them when it
//! ends. This measures, on a live thread with a message loop:
//!
//! 1. `install`: both `SetWindowsHookExW` calls, as one step.
//! 2. `remove`: both `UnhookWindowsHookEx` calls.
//! 3. `round trip`: main posts a command to the input thread, which
//!    answers with a posted message (how the main loop will ask the
//!    input thread to enter or leave a mode).
//!
//! The hooks pass every event straight on, so the measurement doesn't
//! affect anything typed meanwhile. No input is synthesized.
//!
//! Run: `cargo run --release --example hook_cost [-- <iterations>]`

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("hook_cost measures Windows hooks; run it on Windows.");
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;
    use std::time::Instant;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, HHOOK, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_QUIT,
    };

    const CMD_INSTALL: u32 = WM_APP + 1;
    const CMD_REMOVE: u32 = WM_APP + 2;
    const CMD_PING: u32 = WM_APP + 3;
    const REPLY: u32 = WM_APP + 9;

    unsafe extern "system" fn pass(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        CallNextHookEx(None, code, w, l)
    }

    /// `(install µs, remove µs)` per iteration, timed on the input thread.
    type Timings = Vec<(f64, f64)>;

    fn input_thread(main_tid: u32, ready: mpsc::Sender<u32>, out: mpsc::Sender<Timings>) {
        // SAFETY: hook install/remove and a message loop on this thread.
        unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            let _ = ready.send(GetCurrentThreadId());
            let mut hooks: Option<(HHOOK, HHOOK)> = None;
            let mut timings: Timings = Vec::new();
            let mut install_us = 0.0;
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                match msg.message {
                    CMD_INSTALL => {
                        let t = Instant::now();
                        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(pass), None, 0)
                            .expect("keyboard hook");
                        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(pass), None, 0)
                            .expect("mouse hook");
                        install_us = t.elapsed().as_secs_f64() * 1e6;
                        hooks = Some((k, m));
                        let _ = PostThreadMessageW(main_tid, REPLY, WPARAM(0), LPARAM(0));
                    }
                    CMD_REMOVE => {
                        let t = Instant::now();
                        if let Some((k, m)) = hooks.take() {
                            let _ = UnhookWindowsHookEx(k);
                            let _ = UnhookWindowsHookEx(m);
                        }
                        timings.push((install_us, t.elapsed().as_secs_f64() * 1e6));
                        let _ = PostThreadMessageW(main_tid, REPLY, WPARAM(0), LPARAM(0));
                    }
                    CMD_PING => {
                        let _ = PostThreadMessageW(main_tid, REPLY, WPARAM(0), LPARAM(0));
                    }
                    _ => {}
                }
            }
            let _ = out.send(timings);
        }
    }

    fn wait_reply() {
        let mut msg = MSG::default();
        // SAFETY: plain message loop on the main thread.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == REPLY {
                    return;
                }
            }
        }
    }

    fn summary(name: &str, mut v: Vec<f64>) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        println!(
            "{name:<12} n={:<5} p50 {:>7.1} µs   p95 {:>7.1} µs   p99 {:>7.1} µs   max {:>7.1} µs",
            v.len(),
            p(0.5),
            p(0.95),
            p(0.99),
            v[v.len() - 1]
        );
    }

    pub fn main() {
        let n: usize = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);
        // SAFETY: plain thread id / message queue setup.
        let main_tid = unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            GetCurrentThreadId()
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let join = std::thread::spawn(move || input_thread(main_tid, ready_tx, out_tx));
        let tid = ready_rx.recv().expect("input thread started");
        let post = |m: u32| unsafe {
            PostThreadMessageW(tid, m, WPARAM(0), LPARAM(0)).expect("post to input thread");
        };

        let mut cycle = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            post(CMD_INSTALL);
            wait_reply();
            post(CMD_REMOVE);
            wait_reply();
            cycle.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let mut rtt = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            post(CMD_PING);
            wait_reply();
            rtt.push(t.elapsed().as_secs_f64() * 1e6);
        }
        post(WM_QUIT);
        let timings = out_rx.recv().expect("timings");
        let _ = join.join();

        println!("hook_cost: {n} iterations (both low-level hooks per install)");
        summary("install", timings.iter().map(|t| t.0).collect());
        summary("remove", timings.iter().map(|t| t.1).collect());
        summary("enter+leave", cycle);
        summary("round trip", rtt);
    }
}
