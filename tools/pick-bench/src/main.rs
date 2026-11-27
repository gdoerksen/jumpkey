//! pick-bench: black-box latency and resource benchmark for jumpkey.
//!
//! Drives a real `jumpkey.exe` from outside: presses the element-picker
//! hotkey with `SendInput`, times how long until a jumpkey window
//! becomes visible, presses Esc, and reads CPU / memory counters of
//! jumpkey and of the target app. It links nothing from jumpkey, so the
//! same benchmark runs unchanged before and after internal refactors.
//! The contract it relies on is documented in `bench/README.md`.

#[cfg(windows)]
mod check;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod jumpkey;
#[cfg(windows)]
mod output;
#[cfg(windows)]
mod run;
mod scenario;
mod stats;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match cli::parse() {
        Ok(None) => std::process::ExitCode::SUCCESS,
        Ok(Some(opts)) => match if opts.check_mode {
            check::run(opts)
        } else {
            run::run(opts)
        } {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pick-bench: {e:#}");
                std::process::ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("pick-bench: {e:#}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("pick-bench drives jumpkey on Windows only.");
    std::process::exit(1);
}
