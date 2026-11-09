#![expect(clippy::unwrap_used, reason = "test")]

use std::process::Command;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use super::PidWatcher;

#[tokio::test]
async fn pidwatch_reports_exit() {
    let (tx, mut rx) = mpsc::channel(8);
    let watcher = PidWatcher::spawn(tx).unwrap();
    let mut child = Command::new("sleep").arg("0.2").spawn().unwrap();
    watcher.watch(child.id()).unwrap();
    let got = timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
    assert_eq!(got, Some(child.id()));
    child.wait().unwrap();
}

#[tokio::test]
async fn pidwatch_reports_already_dead_pid() {
    let (tx, mut rx) = mpsc::channel(8);
    let watcher = PidWatcher::spawn(tx).unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    watcher.watch(pid).unwrap();
    let got = timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
    assert_eq!(got, Some(pid));
}

#[tokio::test]
async fn pidwatch_reports_already_dead_pid_with_a_full_channel() {
    let (tx, mut rx) = mpsc::channel(1);
    let watcher = PidWatcher::spawn(tx).unwrap();
    let mut pids = Vec::new();
    for _ in 0..3 {
        let mut child = Command::new("true").spawn().unwrap();
        pids.push(child.id());
        child.wait().unwrap();
    }
    for pid in &pids {
        watcher.watch(*pid).unwrap();
    }
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(
            timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    got.sort_unstable();
    pids.sort_unstable();
    assert_eq!(got, pids);
}

#[tokio::test]
async fn pidwatch_thread_ends_when_dropped() {
    let (tx, mut rx) = mpsc::channel(8);
    drop(PidWatcher::spawn(tx).unwrap());
    let got = timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
    assert_eq!(got, None, "sender dropped with the watcher thread");
}
