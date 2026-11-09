//! Signals during a session: ending the shim before cargo starts, and
//! forwarding them to cargo once it runs (spec §5.3 step 3).

use crate::ignored::SignalSet;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getpgrp, tcgetpgrp};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use signal_hook::low_level::emulate_default_handler;
use std::fs::File;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

/// The signals the shim handles, unless it inherited them as ignored.
const HANDLED: [i32; 4] = [SIGINT, SIGTERM, SIGHUP, SIGQUIT];

/// Where cargo is in its life, shared with the signal thread.
#[derive(Clone, Copy)]
pub enum Phase {
    /// Not started: a signal ends the shim as that signal.
    Waiting,
    /// Running, or exited but not yet reaped: signals go to cargo.
    Running(Pid),
    /// About to be reaped: its pid may soon belong to another process.
    Done,
}

/// The phase, shared between the main and signal threads.
pub type SharedPhase = Arc<Mutex<Phase>>;

/// Handles every signal in [`HANDLED`] that is not in `ignored`. An ignored
/// signal gets no handler, so it stays ignored in the shim and in cargo,
/// which inherits the disposition, and is never forwarded.
///
/// # Errors
/// Registering the handlers or spawning their thread fails.
pub fn watch_signals(phase: SharedPhase, ignored: SignalSet) -> io::Result<()> {
    let wanted: Vec<i32> = HANDLED
        .into_iter()
        .filter(|&sig| !ignored.contains(sig))
        .collect();
    let mut signals = Signals::new(wanted)?;
    thread::Builder::new()
        .name("damper-signals".to_owned())
        .spawn(move || {
            for sig in signals.forever() {
                on_signal(&phase, sig);
            }
        })?;
    Ok(())
}

fn on_signal(phase: &SharedPhase, sig: i32) {
    let guard = lock(phase);
    match *guard {
        // Before cargo starts the shim dies as the signal would kill it;
        // the daemon takes the session's tokens back when the socket closes.
        Phase::Waiting => {
            let _ = emulate_default_handler(sig);
        }
        Phase::Running(pid) => {
            if forwards(sig, in_foreground())
                && let Ok(signal) = Signal::try_from(sig)
            {
                let _ = kill(pid, signal);
            }
        }
        Phase::Done => {}
    }
}

/// Whether `sig` goes on to cargo. A terminal sends SIGINT and SIGQUIT to
/// its whole foreground process group, cargo included, so when the shim is
/// in that group, forwarding would deliver them twice.
#[must_use]
fn forwards(sig: i32, foreground: bool) -> bool {
    !(foreground && (sig == SIGINT || sig == SIGQUIT))
}

/// Whether the shim's process group is its terminal's foreground group.
/// No controlling terminal means no.
fn in_foreground() -> bool {
    let foreground = tcgetpgrp(io::stdin())
        .or_else(|_| tcgetpgrp(io::stderr()))
        .ok()
        .or_else(|| {
            File::open("/dev/tty")
                .ok()
                .and_then(|tty| tcgetpgrp(&tty).ok())
        });
    foreground == Some(getpgrp())
}

/// Locks the phase, even if a thread panicked while holding it.
pub fn lock(phase: &SharedPhase) -> MutexGuard<'_, Phase> {
    phase.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::forwards;
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

    #[test]
    fn terminal_signals_are_forwarded_only_to_a_background_shim() {
        for sig in [SIGINT, SIGQUIT] {
            assert!(!forwards(sig, true), "{sig}");
            assert!(forwards(sig, false), "{sig}");
        }
    }

    #[test]
    fn term_and_hup_are_always_forwarded() {
        for sig in [SIGTERM, SIGHUP] {
            assert!(forwards(sig, true), "{sig}");
            assert!(forwards(sig, false), "{sig}");
        }
    }
}
