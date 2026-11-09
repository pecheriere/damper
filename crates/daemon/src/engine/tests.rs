#![expect(clippy::unwrap_used, reason = "test")]

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use damper_core::broker::{Broker, MemorySample};
use damper_core::config::{Machine, load};
use damper_proto::EventKind;
use tokio::sync::mpsc;

use super::fallbacks::{Fallbacks, INGEST_EVERY};
use super::{ConfigWatch, Engine};
use crate::eventlog::EventLog;
use crate::pidwatch::PidWatcher;
use crate::sources::{MemorySource, ProcessSource, Snapshot};

struct FailingMemory;

impl MemorySource for FailingMemory {
    fn sample(&self) -> io::Result<MemorySample> {
        Err(io::Error::other("memory is unreadable"))
    }
}

struct NoProcs;

impl ProcessSource for NoProcs {
    fn snapshot(&self) -> io::Result<Snapshot> {
        Ok(Snapshot::default())
    }
}

#[tokio::test]
async fn polled_pids_are_checked_even_while_memory_reads_fail() {
    let dir = tempfile::tempdir().unwrap();
    let machine = Machine {
        cores: 8,
        memory: 16 << 30,
    };
    let loaded = load(None, machine);
    let (exit_tx, _exits) = mpsc::channel(8);
    let (log, mut events) = mpsc::unbounded_channel();
    let event_log = Arc::new(Mutex::new(EventLog::open(dir.path()).unwrap()));
    let fallbacks_file = dir.path().join("fallbacks.jsonl");
    let mut engine = Engine {
        broker: Broker::new(loaded.effective, loaded.warnings, 0, Instant::now()),
        fifos: HashMap::new(),
        clients: HashMap::new(),
        watcher: PidWatcher::spawn(exit_tx).unwrap(),
        log,
        run_dir: dir.path().to_path_buf(),
        memory: Arc::new(FailingMemory),
        procs: Arc::new(NoProcs),
        config: ConfigWatch {
            path: dir.path().join("config.toml"),
            machine,
            mtime: None,
        },
        memory_failing: false,
        procs_failing: false,
        fill_failing: HashSet::new(),
        polled: HashSet::new(),
        fallbacks: Fallbacks::new(Arc::clone(&event_log), fallbacks_file.clone()),
    };
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let gone = child.id();
    child.wait().unwrap();
    engine.polled.insert(gone);

    engine.tick().await;

    assert!(engine.polled.is_empty(), "the exited pid is still polled");
    let mut saw_memory_error = false;
    while let Ok(kind) = events.try_recv() {
        saw_memory_error |=
            matches!(kind, EventKind::Error { ref message } if message.contains("memory"));
    }
    assert!(saw_memory_error, "the memory failure was not logged");

    // A shim fell back while the daemon runs: within a few ticks its record
    // is in the log, memory failing or not.
    std::fs::write(
        &fallbacks_file,
        "{\"ts_ms\":5,\"type\":\"shim_fallback\",\"reason\":\"fed\",\"cwd\":\"/w\"}\n",
    )
    .unwrap();
    for _ in 1..INGEST_EVERY {
        engine.tick().await;
    }
    assert!(!fallbacks_file.exists(), "the fallbacks were not ingested");
    let logged = event_log.lock().unwrap().read(None).unwrap();
    assert!(
        logged
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ShimFallback { reason, .. } if reason == "fed")),
        "{logged:?}"
    );
}
