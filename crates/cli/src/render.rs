//! Human-readable output for `status`, `log` and `explain`.
//!
//! Event times are relative to a caller-supplied "now" (`5m ago`), so output is
//! deterministic and needs no time-zone handling. Text that originates outside
//! the CLI (working directories, reasons, messages) has control characters
//! replaced, so a hostile path cannot drive the terminal.

use std::fmt::Write as _;
use std::path::Path;

use damper_proto::{
    Event, EventKind, InputsSummary, Pressure, SessionId, SessionState, SessionStatus, Status,
};

/// Shown when the daemon left the oldest events out of a reply.
const TRUNCATED_NOTICE: &str =
    "note: older events were omitted (reply too large); narrow the range with --since.\n";

/// Unicode format (Cf) and line/paragraph separator characters that can
/// reorder or hide text on a terminal. Control characters are caught by
/// [`char::is_control`]; this list covers the rest.
const INVISIBLE: &[(char, char)] = &[
    ('\u{00ad}', '\u{00ad}'),
    ('\u{0600}', '\u{0605}'),
    ('\u{061c}', '\u{061c}'),
    ('\u{06dd}', '\u{06dd}'),
    ('\u{070f}', '\u{070f}'),
    ('\u{0890}', '\u{0891}'),
    ('\u{08e2}', '\u{08e2}'),
    ('\u{180e}', '\u{180e}'),
    ('\u{200b}', '\u{200f}'),
    ('\u{2028}', '\u{202e}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2066}', '\u{206f}'),
    ('\u{feff}', '\u{feff}'),
    ('\u{fff9}', '\u{fffb}'),
    ('\u{110bd}', '\u{110bd}'),
    ('\u{110cd}', '\u{110cd}'),
    ('\u{13430}', '\u{1343f}'),
    ('\u{1bca0}', '\u{1bca3}'),
    ('\u{1d173}', '\u{1d17a}'),
    ('\u{e0001}', '\u{e0001}'),
    ('\u{e0020}', '\u{e007f}'),
];

/// Replaces control and invisible format characters, which could carry
/// terminal escapes or reorder text. Every print site of outside text uses it.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            let invisible = INVISIBLE.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
            if c.is_control() || invisible { '?' } else { c }
        })
        .collect()
}

fn clean_path(path: &Path) -> String {
    clean(&path.to_string_lossy())
}

/// Formats a byte count with one decimal in the largest binary unit.
pub(crate) fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (name, size) in UNITS {
        if bytes >= size {
            let tenths = u128::from(bytes) * 10 / u128::from(size);
            return format!("{}.{} {name}", tenths / 10, tenths % 10);
        }
    }
    format!("{bytes} B")
}

/// Formats seconds as `45s`, `2m05s` or `1h02m`.
pub(crate) fn fmt_secs(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn fmt_age(ts_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(ts_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn pressure(p: Pressure) -> &'static str {
    match p {
        Pressure::Normal => "normal",
        Pressure::Warning => "warning",
        Pressure::Critical => "critical",
    }
}

fn inputs_line(i: &InputsSummary) -> String {
    format!(
        "pressure {}, {}% available, swap {}{}, builds {}, reserve {}",
        pressure(i.pressure),
        i.available_pct,
        fmt_bytes(i.swap_used),
        if i.swap_growing { " (growing)" } else { "" },
        fmt_bytes(i.build_footprint),
        fmt_bytes(i.reserve),
    )
}

fn session_line(s: &SessionStatus) -> String {
    let state = match s.state {
        SessionState::Held { waited_secs } => format!("held {}", fmt_secs(waited_secs)),
        SessionState::Compiling => "compiling".to_owned(),
        SessionState::Idle => "idle".to_owned(),
    };
    format!(
        "  {}  {state}, {} slots, {} tokens, age {}; {} in {}",
        clean(&s.session.0),
        s.slots,
        s.tokens_outstanding,
        fmt_secs(s.age_secs),
        clean(&s.subcommand),
        clean_path(&s.cwd),
    )
}

/// The daemon's status as text.
#[must_use]
pub fn render_status(s: &Status) -> String {
    let mut out = format!(
        "limit {} of {} cores ({}), memory {}\n{}\nholding builds: {}\n",
        s.limit,
        s.cores,
        s.reason,
        fmt_bytes(s.memory),
        inputs_line(&s.inputs),
        if s.holding { "yes" } else { "no" },
    );
    if s.sessions.is_empty() {
        out.push_str("no sessions\n");
    } else {
        let _ = writeln!(out, "sessions ({}):", s.sessions.len());
        for session in &s.sessions {
            out.push_str(&session_line(session));
            out.push('\n');
        }
    }
    if !s.warnings.is_empty() {
        out.push_str("warnings:\n");
        for w in &s.warnings {
            let _ = writeln!(out, "  - {}", clean(w));
        }
    }
    out
}

fn describe(kind: &EventKind) -> String {
    match kind {
        EventKind::LimitChanged {
            from,
            to,
            reason,
            inputs,
        } => format!("limit {from} -> {to} ({reason}); {}", inputs_line(inputs)),
        EventKind::SessionOpened {
            session,
            cwd,
            subcommand,
            shim_pid,
            never_hold,
        } => format!(
            "session {} opened: {} in {} (shim pid {shim_pid}){}",
            clean(&session.0),
            clean(subcommand),
            clean_path(cwd),
            if *never_hold { ", never held" } else { "" },
        ),
        EventKind::SessionStarted { session, cargo_pid } => {
            format!(
                "session {} started cargo (pid {cargo_pid})",
                clean(&session.0)
            )
        }
        EventKind::SessionTokens { session, tokens } => {
            format!("session {} now holds {tokens} tokens", clean(&session.0))
        }
        EventKind::HoldStarted { session, reason } => {
            format!("session {} held: {reason}", clean(&session.0))
        }
        EventKind::HoldReleased {
            session,
            waited_ms,
            timed_out,
        } => format!(
            "session {} released after {}{}",
            clean(&session.0),
            fmt_secs(waited_ms / 1000),
            if *timed_out {
                " (waited the maximum and ran anyway)"
            } else {
                ""
            },
        ),
        EventKind::SessionClosed {
            session,
            tokens_recovered,
            duration_ms,
        } => format!(
            "session {} closed after {}, recovered {tokens_recovered} tokens",
            clean(&session.0),
            fmt_secs(duration_ms / 1000),
        ),
        EventKind::ShimFallback { reason, cwd } => {
            format!("shim fell back in {}: {}", clean_path(cwd), clean(reason))
        }
        EventKind::ConfigLoaded { warnings } if warnings.is_empty() => "config loaded".to_owned(),
        EventKind::ConfigLoaded { warnings } => format!(
            "config loaded with {} warning(s): {}",
            warnings.len(),
            clean(&warnings.join("; ")),
        ),
        EventKind::ClientRejected { reason } => format!("client rejected: {}", clean(reason)),
        EventKind::Error { message } => format!("error: {}", clean(message)),
    }
}

fn event_lines(events: &[Event], now_ms: u64) -> String {
    let mut out = String::new();
    for e in events {
        let _ = writeln!(
            out,
            "{:>8}  {}",
            fmt_age(e.ts_ms, now_ms),
            describe(&e.kind)
        );
    }
    out
}

/// Events, oldest first, with a notice when the daemon truncated the reply.
#[must_use]
pub fn render_events(events: &[Event], now_ms: u64, truncated: bool) -> String {
    let mut out = String::new();
    if truncated {
        out.push_str(TRUNCATED_NOTICE);
    }
    if events.is_empty() {
        out.push_str("No events.\n");
    }
    out.push_str(&event_lines(events, now_ms));
    out
}

/// The story of one session.
#[must_use]
pub fn render_explain(session: &SessionId, events: &[Event], now_ms: u64) -> String {
    if events.is_empty() {
        return format!("No events for session {}.\n", clean(&session.0));
    }
    format!(
        "Session {}:\n{}",
        clean(&session.0),
        event_lines(events, now_ms)
    )
}

#[cfg(test)]
mod tests;
