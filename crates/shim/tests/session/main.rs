//! End-to-end: the built shim against a scripted fake daemon.
#![expect(clippy::unwrap_used, reason = "test")]

mod flow;
mod harness;
mod signals;
