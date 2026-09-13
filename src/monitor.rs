//! The system monitor this plugin opens when you ask the usage readout for more.
//!
//! The readout answers *how much*; the monitor answers *what is using it*. herdr
//! draws no clickable chrome a plugin can hook — not the tab bar, not the sidebar,
//! not a token row — so the trigger is a plugin action and a keybinding, and this
//! module is only the part that decides *what to run* and runs it.
//!
//! The rule is the one the retired herdr patch
//! (`dotfiles/herdr/patches/0003-sidebar-system-usage-header.patch`) used when a
//! click on its sidebar header opened an overlay, down to the spelling of the
//! config key: an explicit `system_monitor` wins, whitespace-split into argv so
//! `"htop -t"` works, and otherwise the first of [`CANDIDATES`] on `PATH`. That
//! patch no longer applies to herdr 0.9.0 — the file it lived in was deleted —
//! but the rule is what carried the value, and it ports in twenty lines.
//!
//! Split the way every other module here is: [`resolve_with`] and [`found_in`]
//! are pure and tested on every target, and only the two lines that hand the
//! process over to the monitor touch the host.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

/// Monitors tried in order when the user has named none.
///
/// Order is preference, not availability: `btop` reads best, `htop` is the one
/// most machines already have, and `top` is the floor — there on every unix
/// whether anyone installed anything or not, which is what makes the feature
/// work with no setup.
#[cfg(not(windows))]
pub(crate) const CANDIDATES: [&str; 3] = ["btop", "htop", "top"];

/// Windows has no monitor in the box to end the list with — `top` is not a
/// program there and Task Manager is a GUI, not something to run in a pane — so
/// the ladder is the two cross-platform TUIs and an honest error below them.
#[cfg(windows)]
pub(crate) const CANDIDATES: [&str; 2] = ["btop", "htop"];

/// Resolve the monitor and hand this process over to it.
///
/// Never returns on success: on unix the monitor *replaces* this process, and
/// elsewhere we exit with its status. Either way nothing of ours is left between
/// herdr and the monitor.
pub fn run(configured: Option<&str>) -> crate::Result<()> {
    exec(&resolve(configured)?)
}

/// The argv to run, from the config value (or its absence).
fn resolve(configured: Option<&str>) -> crate::Result<Vec<String>> {
    resolve_with(configured, program_on_path)
}

/// [`resolve`] against an injected `PATH` lookup.
///
/// The seam exists so the decision can be tested without touching the real
/// `PATH`: a test that set the environment would be testing what the rest of the
/// suite happens to have installed, and would race every other test thread.
///
/// A configured command is returned **unchecked**. The user named it; if it is
/// not there, `exec` says so with the name they typed, which is a better answer
/// than silently running something else.
fn resolve_with(
    configured: Option<&str>,
    on_path: impl Fn(&str) -> bool,
) -> crate::Result<Vec<String>> {
    let named: Vec<String> = configured
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if !named.is_empty() {
        return Ok(named);
    }
    CANDIDATES
        .iter()
        .find(|candidate| on_path(candidate))
        .map(|candidate| vec![candidate.to_string()])
        .ok_or_else(|| {
            // Name both the programs looked for and the key that overrides them:
            // a bare `btop: command not found` tells nobody that a setting could
            // have pointed this somewhere else.
            format!(
                "no system monitor installed — looked for {} on PATH. \
                 Install one, or name yours with `system_monitor = \"...\"` in \
                 the plugin's config.toml.",
                CANDIDATES.join(", "),
            )
            .into()
        })
}

/// unix: replace this process with the monitor.
///
/// `exec` rather than spawn-and-wait so there is no parent of ours holding the
/// pane's terminal. Resize, `Ctrl-C` and the monitor's own `q` then reach it
/// exactly as they would in any other pane, and herdr closes the overlay when it
/// exits, because the process that exits *is* the one herdr started.
#[cfg(unix)]
fn exec(argv: &[String]) -> crate::Result<()> {
    use std::os::unix::process::CommandExt;
    // Only ever returns on failure, so reaching the next line means it failed.
    let err = Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(start_failed(argv, err))
}

/// Everywhere else (Windows, and anything without `exec`): run it as a child and
/// leave with its status, so the pane still closes when the monitor does.
#[cfg(not(unix))]
fn exec(argv: &[String]) -> crate::Result<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|err| start_failed(argv, err))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// The error for a monitor that would not start, naming the whole command as the
/// user would have to type it to reproduce.
fn start_failed(argv: &[String], err: std::io::Error) -> Box<dyn std::error::Error> {
    format!("cannot start `{}`: {err}", argv.join(" ")).into()
}

/// Whether `program` is on this machine's `PATH`.
fn program_on_path(program: &str) -> bool {
    match std::env::var_os("PATH") {
        Some(paths) => found_in(&paths, program),
        None => false,
    }
}

/// Whether `program` sits in any directory of a `PATH`-shaped value.
///
/// An entry we cannot read simply does not match, so a `PATH` carrying a stale
/// directory still finds a monitor in the next one.
fn found_in(paths: &OsStr, program: &str) -> bool {
    std::env::split_paths(paths).any(|dir| is_program(&dir.join(program)))
}

/// unix: a program is a file sitting at that path.
///
/// Not a permission check. A file on `PATH` that is not executable is a broken
/// install, and `exec` reports it far more clearly than a silent fall-through to
/// the next candidate would.
#[cfg(not(windows))]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

/// Windows: a `PATH` entry holds `btop.exe`, not `btop`, so each executable
/// suffix is tried as well as the bare name.
///
/// `with_extension` is safe for exactly the names that reach here — the
/// [`CANDIDATES`] — because none of them contains a dot. A configured command
/// never comes through this function at all.
#[cfg(windows)]
fn is_program(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    // The value Windows itself falls back to when PATHEXT is unset.
    let suffixes = crate::config::non_empty_env("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    suffixes
        .split(';')
        .filter_map(|suffix| suffix.trim().strip_prefix('.'))
        .any(|suffix| path.with_extension(suffix).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch;

    /// A `PATH` value from directories, in the platform's own syntax.
    fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).expect("joinable paths")
    }

    // ---- what to run ---------------------------------------------------------

    #[test]
    fn a_configured_monitor_wins_over_anything_installed() {
        // Explicit beats detected even when the detected one is right there:
        // naming a monitor is how you get the one you want, not a hint.
        let argv = resolve_with(Some("htop -t"), |_| true).expect("configured");
        assert_eq!(argv, vec!["htop".to_string(), "-t".to_string()]);
    }

    #[test]
    fn a_configured_monitor_is_run_even_when_it_is_not_installed() {
        // The user typed it, so the failure they get names it. Falling back to a
        // different program would hide the typo.
        let argv = resolve_with(Some("mymon"), |_| false).expect("configured");
        assert_eq!(argv, vec!["mymon".to_string()]);
    }

    #[test]
    fn a_blank_setting_reads_as_unset_rather_than_as_a_command() {
        // `system_monitor = ""` and a whitespace-only value are the half-finished
        // edit, not an instruction to run nothing.
        for blank in ["", "   ", "\t"] {
            let argv = resolve_with(Some(blank), |p| p == CANDIDATES[0]).expect("detected");
            assert_eq!(argv, vec![CANDIDATES[0].to_string()], "{blank:?}");
        }
    }

    #[test]
    fn detection_takes_the_first_candidate_present() {
        // The ladder is a preference order, so the last candidate is only reached
        // when nothing above it is installed.
        let last = *CANDIDATES.last().expect("a candidate");
        assert_eq!(
            resolve_with(None, |_| true).expect("detected"),
            vec![CANDIDATES[0].to_string()],
        );
        assert_eq!(
            resolve_with(None, |p| p == last).expect("detected"),
            vec![last.to_string()],
        );
    }

    #[test]
    fn nothing_installed_is_an_error_that_names_the_way_out() {
        // Patch 0003 fell back to a bare `btop` here, because its caller drew the
        // failure. This one is the message the user reads, so it has to carry
        // both what was looked for and the key that overrides it.
        let err = resolve_with(None, |_| false)
            .expect_err("no monitor")
            .to_string();
        for candidate in CANDIDATES {
            assert!(err.contains(candidate), "{candidate} missing from: {err}");
        }
        assert!(err.contains("system_monitor"), "no way out in: {err}");
    }

    // ---- finding it on PATH --------------------------------------------------

    #[test]
    fn a_program_is_found_in_a_path_entry_that_holds_it() {
        let dir = scratch("monitor-path");
        std::fs::write(dir.join("mymon"), "").expect("write");
        assert!(found_in(&path_of(&[&dir]), "mymon"));
        assert!(!found_in(&path_of(&[&dir]), "othermon"));
    }

    #[test]
    fn a_path_entry_that_is_not_there_is_skipped_rather_than_fatal() {
        // A stale directory on PATH is ordinary, and must not stop the search
        // before the entry that does hold the monitor.
        let dir = scratch("monitor-path-stale");
        std::fs::write(dir.join("mymon"), "").expect("write");
        let gone = dir.join("no-such-dir");
        assert!(found_in(&path_of(&[&gone, &dir]), "mymon"));
    }

    #[test]
    fn a_directory_of_that_name_is_not_a_program() {
        let dir = scratch("monitor-path-dir");
        std::fs::create_dir(dir.join("mymon")).expect("mkdir");
        assert!(!found_in(&path_of(&[&dir]), "mymon"));
    }

    #[test]
    fn an_empty_path_finds_nothing() {
        assert!(!found_in(OsStr::new(""), CANDIDATES[0]));
    }

    /// The one branch that exists only on Windows: the name on `PATH` carries an
    /// executable suffix the candidate list does not.
    #[test]
    #[cfg(windows)]
    fn a_windows_executable_is_found_by_its_bare_name() {
        let dir = scratch("monitor-path-pathext");
        std::fs::write(dir.join("mymon.exe"), "").expect("write");
        assert!(found_in(&path_of(&[&dir]), "mymon"));
        assert!(!found_in(&path_of(&[&dir]), "othermon"));
    }
}
