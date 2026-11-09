//! A per-tick liveness check for pids the kqueue watcher could not take.

use std::collections::HashSet;

use crate::procs::pid_is_gone;

/// Removes from `pids` and returns those that no longer exist.
///
/// See [`pid_is_gone`] for what counts as gone.
pub(crate) fn take_exited(pids: &mut HashSet<u32>) -> Vec<u32> {
    let exited: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|&pid| pid_is_gone(pid))
        .collect();
    for pid in &exited {
        pids.remove(pid);
    }
    exited
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use std::collections::HashSet;
    use std::process::Command;

    use super::take_exited;

    #[test]
    fn exited_pids_are_taken_and_live_ones_kept() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        let me = std::process::id();
        let mut pids = HashSet::from([gone, me, 0]);
        let mut exited = take_exited(&mut pids);
        exited.sort_unstable();
        assert_eq!(exited, [0, gone]);
        assert_eq!(pids, HashSet::from([me]));
    }
}
