//! Sidebar status updater daemon and its enable/disable/toggle controls.
//!
//! The daemon refreshes each space's usage on a cadence, surfacing it either as
//! a "usage" pseudo-agent (agents-panel mode) or as TTL'd display-only metadata
//! (sidebar mode). A pid file under the state dir enforces a single instance;
//! statuses self-clear via their TTL if the daemon dies. `enable`/`disable`/
//! `toggle` spawn or signal that daemon and sweep leftover statuses, and
//! `restore` (herdr's `[[startup]]` hook) brings it back after a herdr or machine
//! restart when an `enabled` marker alongside the pid file says it was wanted.

use std::collections::HashSet;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::collect::{self, PSEUDO_AGENT};
use crate::config::{self, Config, Labels, Mode};
use crate::herdr::{self, Herdr};
use crate::model::Space;
use crate::proc;

/// Panes we have pushed status onto this run, so shutdown can clear them.
#[derive(Debug, Default)]
pub struct Tracked {
    /// Panes carrying our pseudo-agent (released, not TTL'd).
    pub pseudo: HashSet<String>,
    /// Panes carrying TTL'd pane-level metadata tokens (agents-panel mode).
    pub metadata: HashSet<String>,
    /// Workspaces carrying TTL'd workspace-level metadata tokens (sidebar mode →
    /// the spaces card, which renders workspace tokens rather than pane tokens).
    pub workspaces: HashSet<String>,
}

/// PID of a live updater daemon, or `None` (missing pid file / dead process /
/// a pid that no longer belongs to us).
///
/// Reads `<state_dir>/updater.pid` and confirms the pid is live AND really one
/// of our processes ([`is_our_process`] answers both: a vanished pid has no
/// image name to read). That second check matters: the state dir outlives
/// reboots, so an unclean shutdown can leave a pid file pointing at a pid the
/// kernel later recycled for something else — and without it `--enable` would
/// no-op forever against that impostor, leaving the sidebar permanently blank.
pub fn daemon_pid() -> Option<u32> {
    let pid = read_pid_file()?;
    is_our_process(pid).then_some(pid)
}

/// The pid recorded in `<state_dir>/updater.pid`, or `None` if the file is
/// missing, unparseable, or holds a non-positive pid. Says nothing about
/// whether that process is alive — [`daemon_pid`] adds that.
fn read_pid_file() -> Option<u32> {
    let text = std::fs::read_to_string(config::pid_file()).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    (pid > 0).then_some(pid as u32)
}

/// `--daemon`: run the updater loop until signalled, then clear and exit.
///
/// Single-instance via the pid file; a signal-hook thread performs the SIGINT/
/// SIGTERM shutdown (clear tracked statuses + title, unlink pid, `exit(0)`) over
/// its own socket connection so it need not wait on the main loop's sample sleep.
/// The loop samples with a quick first window, then the configured interval, and
/// shuts down after five consecutive failures (herdr server likely gone).
pub fn run_daemon() -> crate::Result<()> {
    if daemon_pid().is_some() {
        return Ok(()); // another updater is already live
    }
    std::fs::create_dir_all(config::state_dir())?;
    std::fs::write(config::pid_file(), format!("{}\n", std::process::id()))?;

    let config = config::load_config();
    let labels = config::load_herdr_labels();

    let mut client = match herdr::connect() {
        Ok(client) => client,
        Err(err) => {
            // Nothing to run without a host connection — don't leave a pid file
            // pointing at a process that is about to exit.
            let _ = std::fs::remove_file(config::pid_file());
            return Err(err);
        }
    };

    let stopping = Arc::new(AtomicBool::new(false));
    let tracked = Arc::new(Mutex::new(Tracked::default()));

    // Signal thread: on the first SIGINT/SIGTERM, win the shutdown race and clear
    // everything via a fresh connection, then exit. The main loop must not
    // re-report after this runs, so it parks once it observes `stopping`.
    // Windows has no equivalent graceful signal: `--disable` terminates the
    // daemon outright there, and the pushed statuses self-clear via their TTL
    // (plus `--disable`'s own sweep).
    #[cfg(unix)]
    {
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])?;
        let stopping = Arc::clone(&stopping);
        let tracked = Arc::clone(&tracked);
        thread::spawn(move || {
            if signals.forever().next().is_some() && !stopping.swap(true, Ordering::SeqCst) {
                shutdown(herdr::connect().ok().as_mut(), &tracked);
            }
        });
    }

    // Windows-only backstop for the read timeout unix gets from the socket.
    let started = std::time::Instant::now();
    let heartbeat = Arc::new(AtomicU64::new(0));
    #[cfg(windows)]
    spawn_watchdog(Arc::clone(&heartbeat), started, config.interval_seconds);

    let daemon_interval_ms = config.interval_seconds * 1000;
    let mut window_ms: u64 = 500; // quick first sample so the sidebar updates immediately
    let mut failures: u32 = 0;
    loop {
        heartbeat.store(started.elapsed().as_secs(), Ordering::SeqCst);
        match collect::snapshot(&mut client, window_ms) {
            Ok(spaces) => {
                if stopping.load(Ordering::SeqCst) {
                    park(); // shutdown ran during the sample window — do not re-report
                }
                {
                    let mut guard = tracked.lock().expect("tracked mutex poisoned");
                    push_statuses(&mut client, &spaces, &config, &labels, &mut guard);
                }
                if config.window_title_totals {
                    set_title_totals(&mut client, &spaces, &labels);
                }
                failures = 0;
            }
            Err(_) => {
                failures += 1;
                if failures >= 5 && !stopping.swap(true, Ordering::SeqCst) {
                    shutdown(Some(&mut client), &tracked); // herdr server likely gone
                }
                thread::sleep(Duration::from_secs(1));
                if stopping.load(Ordering::SeqCst) {
                    park();
                }
            }
        }
        window_ms = daemon_interval_ms;
    }
}

/// `--enable`: record that the updater is wanted and spawn a detached `--daemon`
/// process (spawn is a no-op if one is already running).
pub fn enable_updater() -> crate::Result<()> {
    // Record the intent first, so the `[[startup]]` hook restores the updater
    // after a restart even if the spawn below fails.
    set_enabled(&config::enabled_flag(), true);

    if daemon_pid().is_some() {
        notify("sidebar usage already enabled");
        return Ok(());
    }
    spawn_daemon()?;
    notify("sidebar usage enabled");
    Ok(())
}

/// `--restore`: herdr's `[[startup]]` hook — bring the updater back after a herdr
/// or machine restart, but only if it was enabled when herdr went away.
///
/// Silent by design: a restart is not a user action, so it raises no toast. Both
/// gates are no-ops rather than errors — an updater that was never enabled stays
/// off, and a live one (herdr re-runs startup hooks on every live handoff too) is
/// left alone.
pub fn restore_updater() -> crate::Result<()> {
    if !enabled_flag_set(&config::enabled_flag()) || daemon_pid().is_some() {
        return Ok(());
    }
    spawn_daemon()
}

/// `--disable`: clear the wanted flag, signal the daemon, and sweep any leftover
/// statuses / title.
pub fn disable_updater() -> crate::Result<()> {
    // Clear the intent so the `[[startup]]` hook does not resurrect the updater
    // on the next herdr restart.
    set_enabled(&config::enabled_flag(), false);

    if let Some(pid) = daemon_pid() {
        // Unix: SIGTERM, and the daemon clears its own statuses + title on the
        // way down. Windows: TerminateProcess — abrupt, but the sweep below and
        // the status TTLs cover the cleanup the daemon can no longer do.
        proc::stop_process(pid);
        // A terminated process runs no shutdown, so on Windows nothing would
        // ever unlink the pid file: it outlives the daemon and leaves the
        // recycled-pid check ([`is_our_process`]) as the only thing standing
        // between a stale pid and a permanently no-op `--enable`. Deliberately
        // NOT done on unix — there the daemon unlinks it from its own SIGTERM
        // handler, and removing it out from under a daemon that is still
        // shutting down would let an immediate `--enable` start a second one.
        //
        // Guarded on the file still naming the pid we just stopped: a daemon
        // started in the window between `daemon_pid` and here owns its own file
        // and must keep it, or this would silently break its single-instance
        // guard.
        #[cfg(windows)]
        if read_pid_file() == Some(pid) {
            let _ = std::fs::remove_file(config::pid_file());
        }
    }

    // Belt and braces: sweep every current pane in case the daemon died — release
    // pseudo-agents (no TTL) and clear metadata statuses — then clear the title.
    // If herdr is unavailable, metadata TTLs expire the statuses anyway.
    if let Ok(mut client) = herdr::connect() {
        if let Ok(spaces) = collect::collect_spaces(&mut client) {
            let mut sweep = Tracked::default();
            for sp in &spaces {
                sweep.pseudo.extend(sp.pseudo_panes.iter().cloned());
                sweep.metadata.extend(sp.agent_panes.iter().cloned());
                sweep.metadata.extend(sp.spare_panes.iter().cloned());
                sweep.workspaces.insert(sp.id.clone());
            }
            clear_all(&mut client, &sweep);
        }
        let _ = client.window_title_clear();
    }

    notify("sidebar usage disabled");
    Ok(())
}

/// `--toggle`: disable if a daemon is live, else enable.
pub fn toggle_updater() -> crate::Result<()> {
    if daemon_pid().is_some() {
        disable_updater()
    } else {
        enable_updater()
    }
}

/// Push each space's usage status onto a pane, mode-dependent, recording the
/// touched panes in `tracked`.
///
/// agents-panel mode: release any stale pseudo-claims beyond the first, then
/// report the "usage" pseudo-agent (state `idle`) on the space's first pseudo /
/// spare pane; on success that space is done. sidebar mode (and the agents-panel
/// fall-through when the pseudo report fails): release leftover pseudo-agents,
/// then report TTL'd metadata on the first spare pane (else the first agent pane).
pub fn push_statuses(
    client: &mut Herdr,
    spaces: &[Space],
    config: &Config,
    labels: &Labels,
    tracked: &mut Tracked,
) {
    let source = config::plugin_id();
    let ttl_ms = status_ttl_ms(config.interval_seconds);

    for sp in spaces {
        let status = status_line(sp, labels);

        if config.mode == Mode::AgentsPanel {
            // Drop stale claims from earlier runs so a space keeps one entry.
            for extra in sp.pseudo_panes.iter().skip(1) {
                release_pseudo(client, extra, &source);
            }
            let pane = sp.pseudo_panes.first().or_else(|| sp.spare_panes.first());
            if let Some(pane) = pane {
                // 0.7.5: report_agent only claims the identity/entry; the status
                // text rides a named `usage` token pushed onto the same pane, which
                // an `[sidebar.agents]` row renders via `$usage`.
                if client
                    .report_agent(pane, &source, PSEUDO_AGENT, "idle")
                    .is_ok()
                {
                    tracked.pseudo.insert(pane.clone());
                    if client
                        .report_metadata_status(pane, &source, PSEUDO_AGENT, &status, ttl_ms)
                        .is_ok()
                    {
                        tracked.metadata.insert(pane.clone());
                    }
                    continue; // dedicated panel entry covers this space
                }
                // pane just closed — fall through to metadata
            }
        } else {
            // sidebar mode: release pseudo-agents left over from agents-panel mode
            // or pre-v0.5 versions (report-agent entries have no TTL).
            for pane_id in &sp.pseudo_panes {
                release_pseudo(client, pane_id, &source);
            }
            // 0.7.5: the spaces card renders WORKSPACE tokens (`[ui.sidebar.spaces]`
            // `$usage`), not pane tokens — so report at the workspace level.
            if client
                .workspace_report_metadata(&sp.id, &source, PSEUDO_AGENT, &status, ttl_ms)
                .is_ok()
            {
                tracked.workspaces.insert(sp.id.clone());
            }
            continue;
        }

        // agents-panel fall-through: report the pane-level token on a spare/agent
        // pane so the agents panel still shows the space. Three ways in, only
        // the first of which used to be documented:
        //   1. `report_agent` failed — the pseudo pane closed mid-cycle;
        //   2. the space has only agent panes, so there was never a spare to
        //      claim (true since long before the plugin-pane guard);
        //   3. every agent-less pane belongs to another plugin and was filtered
        //      out (see `collect::is_plugin_pane`).
        // In 2 and 3 the space gets no row of its own and its usage rides on an
        // agent's row instead. That is the designed agents-panel layout, not a
        // hijack — `[ui.sidebar.agents]` expands `$usage` on the agent row — and
        // it stays a token report: we never claim a pseudo-AGENT on a pane we
        // do not own, which is the thing that grew a duplicate panel entry.
        // If the space has no agent panes either, `targets` is empty and the
        // space reports nothing this cycle; the next refresh re-evaluates.
        let targets = if !sp.spare_panes.is_empty() {
            &sp.spare_panes[..1]
        } else if !sp.agent_panes.is_empty() {
            &sp.agent_panes[..1]
        } else {
            &[][..]
        };
        for pane_id in targets {
            if client
                .report_metadata_status(pane_id, &source, PSEUDO_AGENT, &status, ttl_ms)
                .is_ok()
            {
                tracked.metadata.insert(pane_id.clone());
            }
        }
    }
}

/// Release every pseudo-agent and clear every metadata status in `tracked`.
pub fn clear_all(client: &mut Herdr, tracked: &Tracked) {
    let source = config::plugin_id();
    for pane_id in &tracked.pseudo {
        release_pseudo(client, pane_id, &source);
    }
    for pane_id in &tracked.metadata {
        let _ = client.clear_metadata_status(pane_id, &source, PSEUDO_AGENT);
    }
    for workspace_id in &tracked.workspaces {
        let _ = client.workspace_clear_metadata(workspace_id, &source, PSEUDO_AGENT);
    }
}

/// Write the all-space CPU/RAM totals to the client window title.
pub fn set_title_totals(client: &mut Herdr, spaces: &[Space], labels: &Labels) {
    let mut cpu = 0.0;
    let mut ram_mb = 0.0;
    for sp in spaces {
        cpu += sp.cpu;
        ram_mb += sp.ram_mb;
    }
    let title = format!(
        "spaces · {} {}% · {} {}",
        labels.cpu,
        cpu.round() as i64,
        labels.ram,
        ram_display(ram_mb),
    );
    let _ = client.window_title_set(&title);
}

// ---- helpers ----------------------------------------------------------------

/// Re-exec ourselves as a detached `--daemon` process.
///
/// Fully detached so it survives the short-lived `--enable` / `--restore`
/// command herdr spawned it from: a new session (setsid) on unix; no console
/// window and its own process group on Windows. Null stdio on both.
fn spawn_daemon() -> crate::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and the only action taken in the
    // forked child before exec; it starts a new session, detaching the daemon.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    // CREATE_NO_WINDOW (0x0800_0000): no console at all — a herdr hook child
    // with a visible console flashes a window on Windows Terminal hosts.
    // CREATE_NEW_PROCESS_GROUP (0x0000_0200): detaches Ctrl+C delivery from the
    // spawning command's group.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000 | 0x0000_0200);
    cmd.spawn()?; // do not wait — the child outlives us
    Ok(())
}

/// Create (`true`) or remove (`false`) the "updater wanted" marker at `path`.
///
/// Best-effort: the marker only drives restart recovery, so a state dir we cannot
/// write must not fail the enable/disable the user actually asked for.
fn set_enabled(path: &std::path::Path, enabled: bool) {
    if !enabled {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, "1\n");
}

/// Whether the "updater wanted" marker at `path` exists.
fn enabled_flag_set(path: &std::path::Path) -> bool {
    path.exists()
}

/// Whether `pid` is live and one of *our* processes, compared by executable
/// image name (`/proc/<pid>/comm` on Linux, the Toolhelp exe name on Windows)
/// against our own.
///
/// Guards pid reuse behind a stale pid file: a vanished process has no image
/// name to read and so reads as "not ours", which is the safe answer — we then
/// treat the updater as down and start a fresh one.
///
/// Two things this deliberately does NOT promise, both worth knowing before
/// leaning on it:
///
/// - It is not an ownership check. `/proc/<pid>/comm` is world-readable, so
///   another user's process of the same name matches. The old `kill(pid, 0)`
///   probe rejected those with EPERM; it was dropped for a platform-neutral
///   liveness check. Reach is narrow — herdr gives each user its own
///   `HERDR_PLUGIN_STATE_DIR`, so a shared pid file needs the `<tmpdir>`
///   fallback, i.e. the binary run outside herdr.
/// - It cannot tell the daemon apart from our OTHER commands. `--interval`
///   (the dashboard pane) and `--once` run the same executable, so a recycled
///   pid landing on a live dashboard reads as a running daemon and makes
///   `--enable` no-op until that pane closes. Pre-existing, on both platforms.
///
/// An advisory lock held on the pid file for the daemon's lifetime would settle
/// both (`File::try_lock`, stable since 1.89) and is the recommended follow-up.
fn is_our_process(pid: u32) -> bool {
    match (
        proc::process_image_name(pid),
        proc::process_image_name(std::process::id()),
    ) {
        (Some(theirs), Some(ours)) => theirs == ours,
        _ => false,
    }
}

/// Clear tracked statuses + title, unlink the pid file, and `exit(0)`.
///
/// Shared by the signal thread (own connection) and the five-failure path (main
/// connection). `client` is `None` only when no socket could be opened, in which
/// case the pid file is still removed before exiting. Never returns.
fn shutdown(client: Option<&mut Herdr>, tracked: &Mutex<Tracked>) -> ! {
    if let Some(client) = client {
        if let Ok(tracked) = tracked.lock() {
            clear_all(client, &tracked);
        }
        let _ = client.window_title_clear();
    }
    let _ = std::fs::remove_file(config::pid_file());
    std::process::exit(0);
}

/// Idle forever while the signal thread completes its shutdown and `exit(0)`s the
/// whole process; keeps the main loop from re-reporting or racing that exit.
fn park() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// How long a sample may stall before the Windows watchdog gives up on it:
/// generous, because a slow sample is normal and a false trip would kill a
/// healthy updater. Only a host that has stopped answering entirely gets here.
#[cfg(windows)]
const WATCHDOG_GRACE: Duration = Duration::from_secs(300);

/// Windows-only stand-in for the socket read timeout unix sets in
/// [`crate::herdr`].
///
/// A named pipe opened as a `File` has no timeout knob, so a herdr that accepts
/// the connection but never answers parks the sample loop inside `read_line`
/// forever. Nothing recovers from that on its own: the failure counter never
/// advances, so the five-failure shutdown never runs, the pid file is never
/// released, and every later `--enable` / `--restore` sees a live pid and
/// silently no-ops — the sidebar stays blank until someone finds the process by
/// hand. The loop stamps `heartbeat` before each sample; if that stamp stops
/// advancing, drop the pid file and exit so the statuses TTL out and the updater
/// can be enabled again.
#[cfg(windows)]
fn spawn_watchdog(heartbeat: Arc<AtomicU64>, started: std::time::Instant, interval_seconds: u64) {
    // At least the grace period, and always several intervals, so a long
    // configured cadence cannot trip its own watchdog.
    let deadline = WATCHDOG_GRACE
        .as_secs()
        .max(interval_seconds.saturating_mul(5));
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(30));
        let stalled = started
            .elapsed()
            .as_secs()
            .saturating_sub(heartbeat.load(Ordering::SeqCst));
        if stalled >= deadline {
            let _ = std::fs::remove_file(config::pid_file());
            std::process::exit(1);
        }
    });
}

/// Largest `ttl_ms` herdr accepts on `pane.report_metadata` /
/// `workspace.report_metadata` (24 h — `ttl_ms.maximum` in `herdr api schema`).
/// Anything above it is rejected with `invalid_metadata_ttl`.
const MAX_TTL_MS: u64 = 86_400_000;

/// The two bounds live in different modules, so tie them at compile time: the
/// largest interval the config parser will yield must still derive a TTL herdr
/// accepts. Changing either constant alone fails the build.
///
/// It must reference both constants, not a literal — `MAX_TTL_MS` tracks an
/// external herdr API limit and so is the likelier of the two to be edited,
/// which is exactly the edit a hardcoded ceiling would let through.
const _: () = assert!(config::MAX_INTERVAL_SECONDS.saturating_mul(3_000) <= MAX_TTL_MS);

/// Status TTL for one refresh cadence: three intervals, clamped to what herdr
/// will accept.
///
/// Pushes are best-effort (the caller ignores failures), so an over-large TTL
/// would blank the sidebar silently with nothing to point at. `saturating_mul`
/// also keeps an absurd interval from wrapping.
fn status_ttl_ms(interval_seconds: u64) -> u64 {
    interval_seconds.saturating_mul(3_000).min(MAX_TTL_MS)
}

/// The per-space status text: `"<cpu> <n>% · <ram> <pct-or-compact>"`.
fn status_line(sp: &Space, labels: &Labels) -> String {
    format!(
        "{} {}% · {} {}",
        labels.cpu,
        sp.cpu.round() as i64,
        labels.ram,
        ram_display(sp.ram_mb),
    )
}

/// RAM as a percent-of-total string, falling back to the compact absolute form
/// when `/proc/meminfo` is unreadable.
fn ram_display(mb: f64) -> String {
    let pct = proc::ram_pct(mb);
    if pct.is_empty() {
        compact_ram(mb)
    } else {
        pct
    }
}

/// Compact absolute RAM: `"<x.x>G"` at/above 1024 MB, else `"<n>M"`
/// — the narrow form used by sidebar statuses.
fn compact_ram(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1}G", mb / 1024.0)
    } else {
        format!("{}M", mb.round() as i64)
    }
}

/// Best-effort release of our pseudo-agent on `pane_id` (a closed pane errors and
/// is ignored — nothing to release).
fn release_pseudo(client: &mut Herdr, pane_id: &str, source: &str) {
    let _ = client.release_agent(pane_id, source, PSEUDO_AGENT);
}

/// Best-effort "Space usage" toast over a throwaway connection.
fn notify(body: &str) {
    if let Ok(mut client) = herdr::connect() {
        let _ = client.notification_show("Space usage", body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Labels;

    fn space(cpu: f64, ram_mb: f64) -> Space {
        Space {
            cpu,
            ram_mb,
            ..Default::default()
        }
    }

    #[test]
    fn status_ttl_is_three_intervals_clamped_to_herdr_ceiling() {
        assert_eq!(status_ttl_ms(5), 15_000);
        assert_eq!(status_ttl_ms(1), 3_000);
        // The largest interval that still fits: 28_800 * 3_000 == 86_400_000.
        assert_eq!(status_ttl_ms(28_800), MAX_TTL_MS);
        assert_eq!(status_ttl_ms(28_801), MAX_TTL_MS);
        // Saturating, so an absurd interval clamps instead of wrapping.
        assert_eq!(status_ttl_ms(u64::MAX), MAX_TTL_MS);
    }

    #[test]
    fn compact_ram_switches_unit_at_1024() {
        assert_eq!(compact_ram(0.0), "0M");
        assert_eq!(compact_ram(512.6), "513M"); // rounds to whole MB
        assert_eq!(compact_ram(1023.4), "1023M"); // still MB below the gate
        assert_eq!(compact_ram(1024.0), "1.0G");
        assert_eq!(compact_ram(1536.0), "1.5G");
    }

    #[test]
    fn status_line_uses_labels_and_rounds_cpu() {
        let labels = Labels {
            cpu: "CPU".to_string(),
            ram: "MEM".to_string(),
        };
        // No /proc/meminfo total in most CI: ram_display falls back to compact.
        // Assert the CPU rounding + label layout, which are total-independent.
        let line = status_line(&space(5.6, 0.0), &labels);
        assert!(line.starts_with("CPU 6% · MEM "), "got: {line}");
    }

    #[test]
    fn status_line_rounds_cpu_half_away_from_zero() {
        let labels = Labels::default();
        assert!(status_line(&space(2.5, 0.0), &labels).starts_with("cpu 3%"));
        assert!(status_line(&space(2.4, 0.0), &labels).starts_with("cpu 2%"));
    }

    // ---- restart recovery ---------------------------------------------------

    /// Unique scratch dir under the system tmpdir, keyed by test name + pid so
    /// parallel test threads never collide.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "space-usage-test-{name}-{}-{:?}",
            std::process::id(),
            thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn enabled_flag_round_trips_and_creates_missing_state_dir() {
        // Point at a *nested* dir that does not exist yet — enabling must create
        // it, mirroring a fresh install whose state dir herdr has not made.
        let flag = scratch("flag").join("nested").join("enabled");
        assert!(
            !enabled_flag_set(&flag),
            "absent before anything is written"
        );

        set_enabled(&flag, true);
        assert!(enabled_flag_set(&flag), "set after --enable");

        set_enabled(&flag, false);
        assert!(!enabled_flag_set(&flag), "cleared after --disable");

        // Clearing an already-absent flag is a no-op, not an error.
        set_enabled(&flag, false);
        assert!(!enabled_flag_set(&flag));
    }

    #[test]
    fn our_own_pid_is_recognised_as_ours() {
        assert!(is_our_process(std::process::id()));
    }

    #[test]
    fn vanished_pid_is_not_ours() {
        // No image name to read for a dead pid — the stale-pid-file case must
        // read as "not ours" so the caller starts a fresh daemon.
        assert!(!is_our_process(u32::MAX));
    }
}
