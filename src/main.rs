//! Space Usage — CPU / RAM per herdr space (workspace).
//!
//! For every workspace herdr reports, we find each
//! pane's shell process (via the herdr socket), walk that PID's `/proc` subtree,
//! and sum CPU% (from utime+stime deltas over a sample window, normalized across
//! all CPU cores) and RSS memory. Results are grouped by space.
//!
//! Modes (argv flags):
//!   --once            print a single snapshot and exit (used by the action)
//!   --interval N      live watch, refreshing every N seconds (used by the pane)
//!   --json            emit machine-readable JSON and exit
//!   --enable          start the sidebar status updater daemon
//!   --disable         stop the daemon and clear statuses
//!   --toggle          enable/disable depending on daemon state
//!   --restore         internal: herdr `[[startup]]` hook — re-enable after a
//!                     herdr/machine restart if the updater was enabled
//!   --daemon          internal: run the updater loop (spawned by --enable)
//!
//! Linux-only: relies on `/proc`. herdr injects HERDR_BIN_PATH / HERDR_PLUGIN_*.

mod collect;
mod config;
mod daemon;
mod herdr;
mod model;
mod proc;
mod render;

use std::process;

/// Crate-wide fallible result; boxed error keeps the scaffold dependency-light.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Default live-watch refresh window when `--interval` is absent or invalid.
const DEFAULT_INTERVAL_MS: u64 = 2000;

fn main() {
    if let Err(err) = run() {
        eprintln!("space-usage: {err}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Daemon / control modes manage their own socket connection internally.
    if has_flag(&args, "--daemon") {
        return daemon::run_daemon();
    }
    if has_flag(&args, "--enable") {
        return daemon::enable_updater();
    }
    if has_flag(&args, "--disable") {
        return daemon::disable_updater();
    }
    if has_flag(&args, "--toggle") {
        return daemon::toggle_updater();
    }
    if has_flag(&args, "--restore") {
        return daemon::restore_updater();
    }

    // Read modes share one socket connection.
    let mut client = herdr::connect()?;
    if has_flag(&args, "--json") {
        return render::run_json(&mut client);
    }

    let labels = config::load_herdr_labels();
    if has_flag(&args, "--once") {
        return render::run_once(&mut client, &labels);
    }

    render::run_interval(&mut client, &labels, interval_ms(&args))
}

/// True if `flag` appears anywhere in `args`.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Parse `--interval N` (seconds) into milliseconds, falling back to the default
/// for a missing, non-numeric, or non-positive value.
fn interval_ms(args: &[String]) -> u64 {
    args.iter()
        .position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|&n| n > 0.0)
        .map(|n| (n * 1000.0) as u64)
        .unwrap_or(DEFAULT_INTERVAL_MS)
}
