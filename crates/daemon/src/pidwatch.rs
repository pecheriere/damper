//! Process exit notification through kqueue (`EVFILT_PROC` / `NOTE_EXIT`).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use nix::errno::Errno;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use tokio::sync::mpsc::Sender;

/// Identifier of the `EVFILT_USER` event that wakes the watcher thread.
const WAKE_IDENT: usize = 1;
const BATCH: usize = 16;

struct Shared {
    kq: Kqueue,
    /// Pids that were already gone when watched; the thread reports them.
    already_dead: Mutex<Vec<u32>>,
    stop: AtomicBool,
}

impl Shared {
    fn wake(&self) -> io::Result<()> {
        let trigger = KEvent::new(
            WAKE_IDENT,
            EventFilter::EVFILT_USER,
            EvFlags::empty(),
            FilterFlag::NOTE_TRIGGER,
            0,
            0,
        );
        self.kq.kevent(&[trigger], &mut [], None)?;
        Ok(())
    }
}

/// Watches processes and reports their pids on a channel once they exit.
///
/// Dropping the watcher asks its thread to stop, which drops the channel's
/// sender. The thread is detached: it is never joined, so it may outlive the
/// watcher briefly.
#[derive(Debug)]
pub struct PidWatcher {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl PidWatcher {
    /// Start the dedicated kqueue thread.
    ///
    /// The thread is detached and not joined. If it dies (a kqueue error it
    /// cannot recover from) it drops its sender, so the exit channel closes
    /// while the watcher is still alive: the receiver seeing `None` before
    /// the watcher is dropped means no further exits will ever be reported
    /// and the caller must treat it as fatal.
    ///
    /// # Errors
    /// Failure to create or arm the kqueue, or to spawn the thread.
    pub fn spawn(exits: Sender<u32>) -> io::Result<Self> {
        let kq = Kqueue::new()?;
        let wake = KEvent::new(
            WAKE_IDENT,
            EventFilter::EVFILT_USER,
            EvFlags::EV_ADD | EvFlags::EV_CLEAR,
            FilterFlag::empty(),
            0,
            0,
        );
        kq.kevent(&[wake], &mut [], None)?;
        let shared = Arc::new(Shared {
            kq,
            already_dead: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("damperd-pidwatch".to_owned())
            .spawn(move || run(&thread_shared, &exits))?;
        Ok(Self { shared })
    }

    /// Report `pid` on the channel when it exits. A pid that is already gone
    /// (`ESRCH`) is reported at once.
    ///
    /// # Errors
    /// Any kqueue error other than `ESRCH`.
    pub fn watch(&self, pid: u32) -> io::Result<()> {
        let ident = usize::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
        let ev = KEvent::new(
            ident,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            0,
        );
        match self.shared.kq.kevent(&[ev], &mut [], None) {
            Ok(_) => Ok(()),
            Err(Errno::ESRCH) => {
                self.shared
                    .already_dead
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(pid);
                Ok(self.shared.wake()?)
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for PidWatcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // If waking fails the thread ends when its channel closes or the
        // kqueue errors; there is nothing more to do from a destructor.
        let _ = self.shared.wake();
    }
}

fn run(shared: &Shared, exits: &Sender<u32>) {
    let mut events = [KEvent::new(
        0,
        EventFilter::EVFILT_PROC,
        EvFlags::empty(),
        FilterFlag::empty(),
        0,
        0,
    ); BATCH];
    loop {
        let n = match shared.kq.kevent(&[], &mut events, None) {
            Ok(n) => n,
            Err(Errno::EINTR) => continue,
            Err(_) => return,
        };
        for ev in &events[..n] {
            if matches!(ev.filter(), Ok(EventFilter::EVFILT_PROC))
                && let Ok(pid) = u32::try_from(ev.ident())
                && exits.blocking_send(pid).is_err()
            {
                return;
            }
        }
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let dead = std::mem::take(
            &mut *shared
                .already_dead
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for pid in dead {
            if exits.blocking_send(pid).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
