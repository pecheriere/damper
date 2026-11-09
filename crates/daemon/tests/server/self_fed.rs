//! `SelfFed`: tokens a shim fed itself after losing the daemon (ruling R22).

use std::process::{Child, Command};

use damper_proto::{ClientMsg, EventKind};
use tokio::time::{Duration, sleep};

use super::{Conn, open_session, paths, start, wait_for_event};

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn self_fed_before_open_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    conn.send(&ClientMsg::SelfFed { tokens: 3 }).await;
    let message = conn.expect_error_then_close().await;
    assert!(message.contains("before open"), "{message}");
    d.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn self_fed_tokens_count_as_held_after_the_connection_closes() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths(&dir);
    let d = start(&p).await;
    let mut conn = Conn::new(&p.socket).await;
    open_session(&mut conn).await;
    // This test process is the shim; its child plays cargo.
    let cargo = Killed(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    conn.send(&ClientMsg::Started {
        cargo_pid: cargo.0.id(),
    })
    .await;
    // What a shim does on finding the daemon silent: report, then hang up.
    conn.send(&ClientMsg::SelfFed { tokens: 5 }).await;
    drop(conn);
    wait_for_event(
        &p,
        |k| matches!(k, EventKind::ShimFallback { reason, .. } if reason.contains("fed 5 tokens")),
    )
    .await;
    let mut outstanding = 0;
    for _ in 0..200 {
        let status = Conn::new(&p.socket).await.status().await;
        outstanding = status.sessions.first().map_or(0, |s| s.tokens_outstanding);
        if outstanding == 5 {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(outstanding, 5);
    drop(cargo);
    d.stop().await.unwrap();
}
