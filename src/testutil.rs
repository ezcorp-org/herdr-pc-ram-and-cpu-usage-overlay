//! Helpers shared by the module test suites.
//!
//! One home for the fixtures more than one module needs, so a third suite that
//! wants a scratch directory copies nothing — the two that had their own before
//! this file existed had already drifted apart in naming.

use std::path::PathBuf;

/// Unique scratch dir under the system tmpdir, keyed by `name` + pid + thread id
/// so parallel test threads never collide.
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
