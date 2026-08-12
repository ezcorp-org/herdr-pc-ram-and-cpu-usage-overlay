//! Helpers shared by the module test suites.
//!
//! One home for the fixtures more than one module needs, so a third suite that
//! wants a scratch directory copies nothing — the two that had their own before
//! this file existed had already drifted apart in naming.

use std::path::PathBuf;

/// Unique scratch dir under the system tmpdir, keyed by `name` + pid + thread id
/// so parallel test threads never collide.
///
/// `name` must be unique across the whole crate, not just within one suite. The
/// thread id separates the suites only while they run on different threads, and
/// `--test-threads=1` — which CI or a bisect may well use — puts every test on
/// one. Two suites that picked the same name would then wipe each other's
/// fixture mid-run, which reads as a flake rather than as the collision it is.
///
/// Removed and recreated on the way in rather than out: a test that fails leaves
/// its fixture behind to look at, and the next run still starts clean.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "space-usage-test-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}
