//! End-to-end: the real `damperd`, `damper-shim` and `damper` with
//! `fake-cargo` standing in for cargo.
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test")]

mod fail_open;
mod flows;
mod harness;
