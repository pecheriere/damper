//! The lines the shim prints while a build is held (spec §6.3).

use damper_proto::{HoldReason, SessionId};
use std::time::Duration;

/// Printed when a hold starts and every `HOLD_NOTICE` while it lasts.
#[must_use]
pub fn hold_line(reason: HoldReason, waited: Duration, session: &SessionId) -> String {
    format!(
        "damper: holding this build, {reason} (waited {secs}s). \
         Run 'damper status' for details. Session {session}.",
        secs = waited.as_secs()
    )
}

/// Printed when the daemon grants a held session because the hold timed out.
#[must_use]
pub fn timeout_line(waited: Duration, session: &SessionId) -> String {
    let minutes = (waited.as_secs() + 30) / 60;
    format!(
        "damper: held for {minutes}m, running this build with one job. \
         Run 'damper explain {session}' for details."
    )
}

#[cfg(test)]
mod tests {
    use super::{hold_line, timeout_line};
    use damper_proto::{HoldReason, SessionId};
    use std::time::Duration;

    fn sid() -> SessionId {
        SessionId("6731a2f0-7".to_owned())
    }

    #[test]
    fn hold_line_exact_copy() {
        assert_eq!(
            hold_line(
                HoldReason::PressureCritical,
                Duration::from_millis(40_900),
                &sid()
            ),
            "damper: holding this build, memory pressure critical (waited 40s). \
             Run 'damper status' for details. Session 6731a2f0-7."
        );
        assert_eq!(
            hold_line(HoldReason::MemoryNearlyFull, Duration::ZERO, &sid()),
            "damper: holding this build, memory nearly full (waited 0s). \
             Run 'damper status' for details. Session 6731a2f0-7."
        );
    }

    #[test]
    fn timeout_line_exact_copy() {
        assert_eq!(
            timeout_line(Duration::from_secs(600), &sid()),
            "damper: held for 10m, running this build with one job. \
             Run 'damper explain 6731a2f0-7' for details."
        );
        // Rounded to the nearest minute: the daemon's 10-minute timer is
        // measured from its side, so the shim may see a moment less.
        assert_eq!(
            timeout_line(Duration::from_millis(599_400), &sid()),
            "damper: held for 10m, running this build with one job. \
             Run 'damper explain 6731a2f0-7' for details."
        );
    }
}
