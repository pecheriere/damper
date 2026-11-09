//! Moving shim fallbacks into the event log while the daemon runs (spec
//! §7.1, §7.3): a shim that fell back or fed its FIFO itself mid-build
//! appends to `fallbacks.jsonl`, and `damper log` must show it without
//! waiting for the next daemon start.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::eventlog::EventLog;

/// Ticks between two ingests.
pub(crate) const INGEST_EVERY: u64 = 5;
/// Bytes one ingest reads at most (about 5,000 records), so a large file
/// cannot stall the tick, and with it the heartbeats; the rest waits for the
/// next ingest.
pub(crate) const INGEST_BUDGET: u64 = 1 << 20;

/// The shims' fallback file and the log it is moved into.
pub(crate) struct Fallbacks {
    events: Arc<Mutex<EventLog>>,
    path: PathBuf,
    ticks: u64,
    /// Whether the last ingest failed, so a lasting failure is logged once.
    failing: bool,
}

impl Fallbacks {
    pub(crate) fn new(events: Arc<Mutex<EventLog>>, path: PathBuf) -> Self {
        Self {
            events,
            path,
            ticks: 0,
            failing: false,
        }
    }

    /// Counts a tick; every [`INGEST_EVERY`] ticks returns the blocking
    /// ingest, for the tick's `spawn_blocking` work.
    pub(crate) fn job(&mut self) -> Option<impl FnOnce() -> io::Result<u32> + Send + 'static> {
        self.ticks = self.ticks.wrapping_add(1);
        if !self.ticks.is_multiple_of(INGEST_EVERY) {
            return None;
        }
        let events = Arc::clone(&self.events);
        let path = self.path.clone();
        Some(move || {
            events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .ingest_fallbacks(&path, INGEST_BUDGET)
        })
    }

    /// The error to log for an ingest's result: the first of a run of
    /// failures only.
    pub(crate) fn outcome(&mut self, result: io::Result<u32>) -> Option<String> {
        match result {
            Ok(_) => {
                self.failing = false;
                None
            }
            Err(e) => (!std::mem::replace(&mut self.failing, true))
                .then(|| format!("ingesting shim fallbacks: {e}")),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use damper_proto::EventKind;

    use super::{Fallbacks, INGEST_BUDGET, INGEST_EVERY};
    use crate::eventlog::EventLog;

    #[test]
    fn ingests_every_few_ticks_into_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(EventLog::open(dir.path()).unwrap()));
        let path = dir.path().join("fallbacks.jsonl");
        let mut fallbacks = Fallbacks::new(Arc::clone(&events), path.clone());
        std::fs::write(
            &path,
            "{\"ts_ms\":5,\"type\":\"shim_fallback\",\"reason\":\"fed\",\"cwd\":\"/w\"}\n",
        )
        .unwrap();
        for _ in 1..INGEST_EVERY {
            assert!(fallbacks.job().is_none());
        }
        let job = fallbacks.job().unwrap();
        assert_eq!(job().unwrap(), 1);
        assert!(!path.exists());
        let logged = events.lock().unwrap().read(None).unwrap();
        assert!(matches!(
            &logged[..],
            [e] if matches!(&e.kind, EventKind::ShimFallback { reason, .. } if reason == "fed")
        ));
        assert!(fallbacks.job().is_none());
    }

    #[test]
    fn one_ingest_reads_at_most_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(EventLog::open(dir.path()).unwrap()));
        let path = dir.path().join("fallbacks.jsonl");
        let line = |n: u32| {
            format!(
                "{{\"ts_ms\":{n},\"type\":\"shim_fallback\",\"reason\":\"r\",\"cwd\":\"/w\"}}\n"
            )
        };
        let size = u64::try_from(line(0).len()).unwrap();
        // Twice the budget's worth of records (same length: ts_ms ≥ 10⁴).
        let total = u32::try_from(2 * INGEST_BUDGET / size).unwrap();
        let text: String = (10_000..10_000 + total).map(line).collect();
        std::fs::write(&path, text).unwrap();

        let mut fallbacks = Fallbacks::new(Arc::clone(&events), path);
        let mut runs = Vec::new();
        while runs.iter().sum::<u32>() < total {
            assert!(runs.len() < 4, "runs: {runs:?}");
            for _ in 1..INGEST_EVERY {
                assert!(fallbacks.job().is_none());
            }
            let job = fallbacks.job().unwrap();
            runs.push(job().unwrap());
        }
        let per_run = u32::try_from(INGEST_BUDGET.div_ceil(size)).unwrap();
        assert!(runs.iter().all(|&n| n <= per_run), "runs: {runs:?}");
        assert_eq!(runs.len(), 3, "runs: {runs:?}");
    }

    #[test]
    fn a_lasting_failure_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(EventLog::open(dir.path()).unwrap()));
        let mut fallbacks = Fallbacks::new(events, dir.path().join("f.jsonl"));
        let failed = || Err(io::Error::other("nope"));
        assert!(fallbacks.outcome(failed()).unwrap().contains("nope"));
        assert!(fallbacks.outcome(failed()).is_none());
        assert!(fallbacks.outcome(Ok(0)).is_none());
        assert!(fallbacks.outcome(failed()).is_some());
    }
}
