//! Pure decisions about a client: who may connect, which builds are never
//! held, which `Started` pid is believable, and what a decode error may say.

use damper_proto::{MAX_LINE, ProtoError};
use serde_json::error::Category;

/// The name of the parent process whose builds are never held (spec §5.4).
const NEVER_HOLD_PARENT: &str = "rust-analyzer";

/// Only the daemon's own user may connect.
#[must_use]
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// Whether a session is never held, from the name of the shim's real parent
/// process (looked up by the daemon, never taken from the client).
pub(crate) fn never_hold(shim_parent_name: Option<&str>) -> bool {
    shim_parent_name == Some(NEVER_HOLD_PARENT)
}

/// Whether `Started { cargo_pid }` names the shim's own child: cargo's parent
/// must be the shim. `taskpolicy` and `nice` exec into cargo, so they keep
/// the pid the shim spawned and do not change this.
///
/// A cargo that already exited and was reaped has no parent to read; if it
/// is also gone (`cargo_gone`), the report is believable and is accepted so
/// the session closes normally. An unreadable parent on a process that still
/// exists belongs to someone else and is refused.
pub(crate) fn started_allowed(cargo_ppid: Option<u32>, cargo_gone: bool, shim_pid: u32) -> bool {
    match cargo_ppid {
        Some(ppid) => ppid == shim_pid,
        None => cargo_gone,
    }
}

/// Why a line was refused, without echoing anything the client sent
/// (spec §7.3): serde's own text can quote client values, so only the error's
/// category and position are given.
pub(crate) fn decode_error_reason(err: &ProtoError) -> String {
    match err {
        ProtoError::Json(e) => {
            let category = match e.classify() {
                Category::Io => "i/o",
                Category::Syntax => "syntax",
                Category::Data => "data",
                Category::Eof => "end of input",
            };
            // Errors found after parsing (a wrong field type, an unknown
            // message) carry no position.
            if e.line() == 0 {
                format!("malformed message: {category} error")
            } else {
                format!(
                    "malformed message: {category} error at line {}, column {}",
                    e.line(),
                    e.column()
                )
            }
        }
        ProtoError::Version { got } => format!("unsupported protocol version {got}"),
        ProtoError::LineTooLong => format!("protocol line longer than {MAX_LINE} bytes"),
        _ => "malformed message".to_owned(),
    }
}
