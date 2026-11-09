//! Finding the real cargo on `PATH` (spec §4).

use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Walks `path_var` in order and returns the first `<entry>/cargo` that is an
/// executable file and is not the shim itself.
///
/// The returned path is not canonicalized: rustup's proxy picks the toolchain
/// from its own name, so the link must never be resolved (spec §4).
#[must_use]
pub fn find_real_cargo(path_var: &OsStr, own_exe: &Path) -> Option<PathBuf> {
    let own = own_exe.canonicalize().ok()?;
    let own_id = own.metadata().ok().map(|m| (m.dev(), m.ino()));
    std::env::split_paths(path_var)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join("cargo"))
        .find(|candidate| is_executable_file(candidate) && !is_same_file(candidate, &own, own_id))
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Whether `candidate` is the shim: the same canonical path (a symlink), or
/// the same `(dev, ino)` (a hardlink or a copy placed by a package manager's
/// linker). Running the shim as its own "real cargo" would recurse forever.
fn is_same_file(candidate: &Path, own_canonical: &Path, own_id: Option<(u64, u64)>) -> bool {
    let by_path = candidate
        .canonicalize()
        .is_ok_and(|resolved| resolved == own_canonical);
    let by_id =
        own_id.is_some_and(|id| candidate.metadata().is_ok_and(|m| (m.dev(), m.ino()) == id));
    by_path || by_id
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn script(path: &Path, mode: u32) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn join(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn finds_next_cargo_skipping_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let own = std::env::current_exe().unwrap();
        symlink(&own, a.join("cargo")).unwrap();
        script(&b.join("cargo"), 0o755);
        assert_eq!(
            find_real_cargo(&join(&[&a, &b]), &own),
            Some(b.join("cargo"))
        );
    }

    #[test]
    fn skips_a_hardlink_of_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        // A hardlink must live on the same filesystem as its target, so link
        // a private copy of the shim made inside the temp dir.
        let own = tmp.path().join("shim");
        fs::copy(std::env::current_exe().unwrap(), &own).unwrap();
        fs::hard_link(&own, a.join("cargo")).unwrap();
        script(&b.join("cargo"), 0o755);
        assert_eq!(
            find_real_cargo(&join(&[&a, &b]), &own),
            Some(b.join("cargo"))
        );
    }

    #[test]
    fn returns_symlink_path_not_target() {
        let tmp = tempfile::tempdir().unwrap();
        let b = tmp.path().join("b");
        fs::create_dir(&b).unwrap();
        script(&b.join("rustup"), 0o755);
        symlink(b.join("rustup"), b.join("cargo")).unwrap();
        let own = std::env::current_exe().unwrap();
        assert_eq!(find_real_cargo(&join(&[&b]), &own), Some(b.join("cargo")));
    }

    #[test]
    fn skips_non_executable_and_empty_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("c"),
        );
        for d in [&a, &b, &c] {
            fs::create_dir(d).unwrap();
        }
        script(&a.join("cargo"), 0o644);
        fs::create_dir(b.join("cargo")).unwrap();
        script(&c.join("cargo"), 0o755);
        let own = std::env::current_exe().unwrap();
        let mut path = OsString::from(":");
        path.push(join(&[&a, &b, &c]));
        assert_eq!(find_real_cargo(&path, &own), Some(c.join("cargo")));
    }

    #[test]
    fn no_cargo_gives_none() {
        let tmp = tempfile::tempdir().unwrap();
        let own = std::env::current_exe().unwrap();
        assert_eq!(find_real_cargo(&join(&[tmp.path()]), &own), None);
    }
}
