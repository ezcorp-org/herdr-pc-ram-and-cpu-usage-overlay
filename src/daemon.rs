//! Sidebar status updater daemon and its enable/disable/toggle controls.
//!
//! The daemon refreshes each space's usage on a cadence, surfacing it either as
//! a "usage" pseudo-agent (agents-panel mode) or as TTL'd display-only metadata
//! (sidebar mode). A pid file under the state dir enforces a single instance;
//! statuses self-clear via their TTL if the daemon dies. `enable`/`disable`/
//! `toggle` spawn or signal that daemon and sweep leftover statuses, and
//! `restore` (herdr's `[[startup]]` and `[[events]]` hooks) brings it back after
//! a herdr or machine restart unless the `enabled` marker alongside the pid file
//! says the user turned it off.
//!
//! That marker is tri-state, and the third state is the whole point: absent means
//! *nobody has decided*, which is a fresh install, and a fresh install wants the
//! updater. The old present/absent boolean could not tell that apart from a
//! deliberate `status-disable`, so a new install stayed dark until someone found
//! `status-enable` by hand. The first run also does the one-time setup in
//! [`bootstrap_sidebar`], because on a fresh install there is no earlier moment
//! to do it in.

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

use crate::battery::Battery;
use crate::collect::{self, PSEUDO_AGENT};
use crate::config::{self, Config, Labels, Mode, Wanted};
use crate::herdr::{self, Herdr};
use crate::herdr_config::{self, Change};
use crate::icons::IconSet;
use crate::model::Space;
use crate::proc;
use crate::render;

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

/// Everything the render path reads out of config, re-read together.
///
/// Grouped rather than passed as three loose values so a caller cannot refresh
/// one and keep a stale copy of another — the labels and the icon tier decide
/// the same row between them, and disagreeing copies would render a mixture.
struct Settings {
    config: Config,
    labels: Labels,
    icons: IconSet,
}

/// Re-read the plugin config and herdr's labels, and re-resolve the icon tier.
///
/// Called once per refresh so an edit to either config file reaches the sidebar
/// on the next cycle instead of waiting for someone to restart the updater. That
/// matters more than it sounds: herdr's own system-usage header renders from the
/// same `cpu_label` / `ram_label`, and herdr reloads its config on demand, so a
/// daemon holding a startup snapshot of those keys would show the old naming in
/// the rows and the new one in the header — the two drifting apart with no
/// indication why.
///
/// Cost is two small file reads. The font probe behind the tier is cached in a
/// `OnceLock`, so re-resolving does not re-fork `fc-list`.
fn reload_settings() -> Settings {
    let config = config::load_config();
    let labels = config::load_herdr_labels().with_overrides(&config);
    let icons = config.icon_set();
    Settings {
        config,
        labels,
        icons,
    }
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

    // Re-read every cycle rather than once here — see `reload_settings`. The
    // first read also fixes the refresh cadence for the life of the daemon,
    // since that drives the sleep and the status TTL together.
    let mut settings = reload_settings();
    let daemon_interval_ms = settings.config.interval_seconds * 1000;

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
    // The watchdog is sized off the cadence fixed at startup, which is the same
    // one `daemon_interval_ms` uses — the interval is deliberately not re-read
    // per cycle, so this stays correct for the daemon's whole life.
    #[cfg(windows)]
    spawn_watchdog(
        Arc::clone(&heartbeat),
        started,
        settings.config.interval_seconds,
    );

    let mut window_ms: u64 = 500; // quick first sample so the sidebar updates immediately
    let mut failures: u32 = 0;
    loop {
        heartbeat.store(started.elapsed().as_secs(), Ordering::SeqCst);
        // Pick up config edits without an updater restart. Two small file reads
        // per refresh, against a cadence measured in seconds — far cheaper than
        // the `/proc` walk that just ran, and it is what keeps the sidebar rows
        // in step with herdr's own system-usage header: both render from
        // `cpu_label` / `ram_label`, so a stale copy here shows one naming above
        // the other. The interval is deliberately NOT re-read; changing the
        // cadence mid-flight would desynchronise the TTL from the refresh rate.
        settings = reload_settings();
        let (config, labels, icons) = (&settings.config, &settings.labels, settings.icons);

        match collect::snapshot(&mut client, window_ms) {
            Ok(spaces) => {
                if stopping.load(Ordering::SeqCst) {
                    park(); // shutdown ran during the sample window — do not re-report
                }
                {
                    let mut guard = tracked.lock().expect("tracked mutex poisoned");
                    push_statuses(&mut client, &spaces, config, labels, icons, &mut guard);
                }
                // The title is the only surface here that draws a battery, so
                // the read lives under its gate: a machine-wide reading nobody
                // renders is a sysfs walk (or a `pmset` fork) for nothing.
                if config.window_title_totals {
                    set_title_totals(&mut client, &spaces, config, labels, icons);
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

/// `--enable`: record that the updater is wanted, make sure herdr's sidebar
/// draws our token, and spawn a detached `--daemon` process (spawn is a no-op if
/// one is already running).
pub fn enable_updater() -> crate::Result<()> {
    // Record the intent first, so the restore hooks bring the updater back after
    // a restart even if the spawn below fails.
    set_wanted(&config::enabled_flag(), Wanted::Enabled);
    let added = bootstrap_sidebar();

    if daemon_pid().is_some() {
        notify(enabled_message("sidebar usage already enabled", added));
        return Ok(());
    }
    spawn_daemon()?;
    notify(enabled_message("sidebar usage enabled", added));
    Ok(())
}

/// `--restore`: herdr's `[[startup]]` and `[[events]]` hooks — bring the updater
/// up whenever herdr is running and the user has not turned it off.
///
/// Runs on a fresh server start, on a live `herdr update --handoff`, and on
/// `workspace.focused`. That last one exists because `herdr plugin install` does
/// NOT run startup hooks: without it a plugin installed into a running herdr
/// stays inert until the next restart, which is most of what "the plugin does
/// nothing" used to mean.
///
/// Silent by design: none of those are user actions, so none raise a toast.
/// Every gate is a no-op rather than an error — a deliberate `status-disable`
/// stays off, and a live daemon is left alone.
///
/// The first run also does the one-time setup [`enable_updater`] would have
/// done, because on a fresh install there is no earlier moment: nobody has run
/// `status-enable`, and that is precisely the bug.
pub fn restore_updater() -> crate::Result<()> {
    let flag = config::enabled_flag();
    let wanted = config::read_wanted(&flag);
    if !wanted.wants_daemon() {
        return Ok(());
    }
    if wanted == Wanted::Undecided {
        bootstrap_sidebar();
    }
    if daemon_pid().is_some() {
        return Ok(());
    }
    spawn_daemon()
}

/// `--disable`: record that the updater is NOT wanted, take our config row back
/// out, signal the daemon, and sweep any leftover statuses / title.
pub fn disable_updater() -> crate::Result<()> {
    // Record the intent so the restore hooks do not resurrect the updater on the
    // next herdr restart or space switch. This writes an explicit "off" rather
    // than deleting the marker: an absent marker now means "fresh install", and
    // deleting it would make every disable undo itself on the next restart.
    set_wanted(&config::enabled_flag(), Wanted::Disabled);
    // Reversible, as promised: whatever we added to herdr's config comes out.
    if herdr_config::remove_usage_row().is_ok_and(Change::needs_reload) {
        reload_herdr_config();
    }

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
///
/// `icons` is the once-per-refresh presentation input the caller resolved:
/// machine-wide, so taking it as a parameter is what keeps the per-space loop
/// below from re-deriving it once per space.
pub fn push_statuses(
    client: &mut Herdr,
    spaces: &[Space],
    config: &Config,
    labels: &Labels,
    icons: IconSet,
    tracked: &mut Tracked,
) {
    let source = config::plugin_id();
    let ttl_ms = status_ttl_ms(config.interval_seconds);

    for sp in spaces {
        let status = status_line(sp, labels, icons);

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

/// Write the all-space CPU/RAM/battery totals to the client window title.
///
/// Takes the reading off the cycle's `config` rather than a battery argument:
/// this is the one surface the daemon draws it on, so nothing else has to carry
/// the value past a row that will not use it.
pub fn set_title_totals(
    client: &mut Herdr,
    spaces: &[Space],
    config: &Config,
    labels: &Labels,
    icons: IconSet,
) {
    let title = title_totals(spaces, labels, icons, config.battery_reading());
    let _ = client.window_title_set(&title);
}

/// The window title text: `"spaces · "` plus the per-space row's cells summed
/// over every space, and the machine's one battery cell.
///
/// Pure, and split from [`set_title_totals`] so the formatting is testable
/// without a live herdr connection.
fn title_totals(
    spaces: &[Space],
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
) -> String {
    let mut cpu = 0.0;
    let mut ram_mb = 0.0;
    for sp in spaces {
        cpu += sp.cpu;
        ram_mb += sp.ram_mb;
    }
    format!(
        "spaces · {}",
        render::totals_row(cpu, ram_mb, labels, icons, battery),
    )
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

/// Record the user's decision in the marker at `path`.
///
/// Always writes — never deletes. An absent marker is its own state now
/// ([`Wanted::Undecided`], the fresh install), so removing the file to mean
/// "off" would make every `--disable` undo itself at the next restart.
///
/// Best-effort: the marker only drives restart recovery, so a state dir we cannot
/// write must not fail the enable/disable the user actually asked for.
fn set_wanted(path: &std::path::Path, wanted: Wanted) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        if wanted == Wanted::Disabled {
            "0\n"
        } else {
            "1\n"
        },
    );
}

/// One-time setup: make herdr's sidebar draw the token this plugin pushes.
///
/// Returns whether a row was added, which the caller turns into a toast — a
/// plugin that edits your config should say so.
///
/// Guarded by its own marker rather than by the enable/disable one, so this runs
/// at most once. Without that a later `status-enable` would re-add a row the
/// user had deliberately deleted from their own config, which is the behaviour
/// that makes people distrust a tool that writes to their files.
fn bootstrap_sidebar() -> bool {
    let marker = config::bootstrapped_flag();
    if marker.exists() {
        return false;
    }
    let mode = config::load_config().mode;
    // Mark it done only when the edit actually settled the question — written,
    // or already present. An Err leaves the marker alone so the next `--restore`
    // tries again, and those are frequent (every server start, every space
    // focus). Marking regardless would turn one unwritable-config moment into a
    // permanently blank sidebar, recoverable only by deleting a marker file the
    // user has no reason to know exists.
    let Ok(change) = herdr_config::ensure_usage_row(mode) else {
        return false;
    };
    set_wanted(&marker, Wanted::Enabled);
    if change.needs_reload() {
        reload_herdr_config();
        return true;
    }
    false
}

/// Ask herdr to re-read its config so a row we just wrote renders now rather
/// than after the next restart. Best-effort over a throwaway connection.
fn reload_herdr_config() {
    if let Ok(mut client) = herdr::connect() {
        let _ = client.server_reload_config();
    }
}

/// Toast text for `--enable`, naming the config edit when there was one.
///
/// Silence would be the wrong default here: the plugin has just written to a
/// file the user owns, and a `.bak` they never hear about is not a safety net
/// they can use.
fn enabled_message(base: &str, added_row: bool) -> String {
    match added_row {
        false => base.to_string(),
        true => format!("{base} — added a $usage row to herdr's config.toml (backup alongside it)"),
    }
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

/// The per-space status text — `cpu ░26% · ram ░8%` in the Unicode tier.
///
/// No battery: it is one reading for the whole machine, so a copy of it on every
/// space's row would read as if the space had its own. [`title_totals`] and the
/// terminal report's total line are where it belongs — see [`render::usage_row`].
fn status_line(sp: &Space, labels: &Labels, icons: IconSet) -> String {
    render::usage_row(sp.cpu, sp.ram_mb, labels, icons)
}

/// Best-effort release of our pseudo-agent on `pane_id` (a closed pane errors and
/// is ignored — nothing to release).
fn release_pseudo(client: &mut Herdr, pane_id: &str, source: &str) {
    let _ = client.release_agent(pane_id, source, PSEUDO_AGENT);
}

/// Best-effort "Space usage" toast over a throwaway connection.
fn notify(body: impl AsRef<str>) {
    if let Ok(mut client) = herdr::connect() {
        let _ = client.notification_show("Space usage", body.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::State;
    use crate::config::Labels;

    fn space(cpu: f64, ram_mb: f64) -> Space {
        Space {
            cpu,
            ram_mb,
            ..Default::default()
        }
    }

    /// Terse [`Battery`] builder for the cell tests.
    fn bat(percent: f64, state: State) -> Battery {
        Battery { percent, state }
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

    // ---- the sidebar status line --------------------------------------------

    #[test]
    fn status_line_uses_labels_and_rounds_cpu() {
        let labels = Labels::new(Some("CPU"), Some("MEM"), Some("PWR"));
        // The RAM cell depends on the host's MemTotal (percent when readable,
        // compact absolute when not), so assert the CPU rounding + label layout,
        // which are total-independent. `render::ram_cell_of` pins both branches.
        let line = status_line(&space(5.6, 0.0), &labels, IconSet::Text);
        assert!(line.starts_with("CPU 6% · MEM "), "got: {line}");
    }

    #[test]
    fn status_line_rounds_cpu_half_away_from_zero() {
        let labels = Labels::default();
        let line = |cpu| status_line(&space(cpu, 0.0), &labels, IconSet::Text);
        assert!(line(2.5).starts_with("cpu 3%"));
        assert!(line(2.4).starts_with("cpu 2%"));
    }

    #[test]
    fn a_space_row_is_the_machine_row_without_the_battery() {
        // The battery is one reading for the whole machine, so it belongs on the
        // surfaces that draw the machine once — never copied onto each space,
        // where the same number repeated reads as if it were per-space.
        //
        // `status_line` cannot even be handed a battery now, so what is worth
        // pinning is the consequence: the row a space gets is exactly the
        // machine-wide row with the battery cell taken off. Host-independent —
        // whether this box has a pack does not change either side.
        let labels = Labels::default();
        let sp = space(26.0, 0.0);
        let row = status_line(&sp, &labels, IconSet::Unicode);
        let machine = render::totals_row(
            sp.cpu,
            sp.ram_mb,
            &labels,
            IconSet::Unicode,
            Some(bat(74.0, State::Discharging)),
        );

        assert!(!row.contains("bat"), "got: {row}");
        assert_eq!(machine.strip_suffix(" · bat ▓74%"), Some(row.as_str()));
    }

    #[test]
    fn status_line_draws_the_cpu_cell_in_every_tier_and_a_battery_in_none() {
        let labels = Labels::default();
        let sp = space(26.0, 0.0);
        // Head of the row per tier; the RAM cell after it is host-dependent, so
        // `render`'s tests pin the whole row instead.
        let expected = [
            (IconSet::Text, "cpu 26%"),
            (IconSet::Unicode, "cpu ░26%"),
            (IconSet::NerdFont, "\u{f4bc} 26%"),
            (IconSet::Emoji, "💻26%"),
        ];
        for (icons, cpu) in expected {
            let line = status_line(&sp, &labels, icons);
            assert!(line.starts_with(&format!("{cpu} · ")), "{icons:?}: {line}");
            // Each tier names the battery its own way, so check all three marks
            // against every row — a glyph tier smuggling one back in would slip
            // straight past a test that only looked for the word.
            for mark in ["bat", "\u{f241}", "🔋"] {
                assert!(!line.contains(mark), "{icons:?} drew {mark}: {line}");
            }
        }
    }

    // ---- the window title ----------------------------------------------------

    #[test]
    fn title_battery_cell_shows_the_charge_state() {
        // Same charge, three states: a glance at the title has to tell a pack
        // that is filling from one that is draining.
        let labels = Labels::default();
        let line = |state| {
            title_totals(
                &[space(26.0, 0.0)],
                &labels,
                IconSet::Unicode,
                Some(bat(74.0, state)),
            )
        };
        assert!(line(State::Charging).ends_with("bat ▓74%+"));
        assert!(line(State::Discharging).ends_with("bat ▓74%"));
        assert!(line(State::Full).ends_with("bat ▓74%="));
        assert_ne!(line(State::Charging), line(State::Discharging));
    }

    #[test]
    fn title_totals_sums_the_spaces_and_carries_one_battery() {
        let labels = Labels::default();
        let spaces = [space(10.0, 0.0), space(16.4, 0.0)];
        let with = title_totals(
            &spaces,
            &labels,
            IconSet::Text,
            Some(bat(74.0, State::Full)),
        );
        let without = title_totals(&spaces, &labels, IconSet::Text, None);

        // 10.0 + 16.4 = 26.4, rounded once over the total rather than per space.
        assert!(with.starts_with("spaces · cpu 26% · ram "), "got: {with}");
        // One machine-wide cell on the title, `=` for a pack on power.
        assert!(with.ends_with(" · bat 74%="), "got: {with}");
        assert!(!without.contains("bat"), "got: {without}");
        assert_eq!(with.strip_suffix(" · bat 74%="), Some(without.as_str()));
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
    fn a_fresh_install_wants_the_daemon_without_anyone_asking() {
        // THE bug. A fresh install has written no marker, and the old present/
        // absent flag read that as "off" — identical to a deliberate disable —
        // so `--restore` no-opped forever and the sidebar stayed blank until
        // someone found `status-enable` by hand.
        let flag = scratch("fresh").join("nested").join("enabled");
        assert_eq!(config::read_wanted(&flag), Wanted::Undecided);
        assert!(
            config::read_wanted(&flag).wants_daemon(),
            "a fresh install must start itself",
        );
    }

    #[test]
    fn the_marker_round_trips_and_creates_a_missing_state_dir() {
        // Point at a *nested* dir that does not exist yet — writing must create
        // it, mirroring a fresh install whose state dir herdr has not made.
        let flag = scratch("flag").join("nested").join("enabled");

        set_wanted(&flag, Wanted::Enabled);
        assert_eq!(config::read_wanted(&flag), Wanted::Enabled);
        assert!(config::read_wanted(&flag).wants_daemon());

        set_wanted(&flag, Wanted::Disabled);
        assert_eq!(config::read_wanted(&flag), Wanted::Disabled);
        assert!(!config::read_wanted(&flag).wants_daemon());

        // Re-enabling after a disable must actually come back on.
        set_wanted(&flag, Wanted::Enabled);
        assert!(config::read_wanted(&flag).wants_daemon());
    }

    #[test]
    fn disabling_writes_a_marker_rather_than_deleting_one() {
        // The one state that has to survive a restart. Deleting the file to mean
        // "off" would now read back as a fresh install, so every `--disable`
        // would quietly undo itself the next time herdr started.
        let flag = scratch("disable").join("enabled");
        set_wanted(&flag, Wanted::Disabled);
        assert!(flag.exists(), "the off state needs a file of its own");
        assert!(!config::read_wanted(&flag).wants_daemon());
    }

    #[test]
    fn a_marker_from_an_older_version_still_reads_as_enabled() {
        // Versions before 1.8.0 wrote a bare "1" and deleted the file to
        // disable. That "1" has to keep meaning enabled across the upgrade.
        let flag = scratch("legacy").join("enabled");
        std::fs::create_dir_all(flag.parent().unwrap()).unwrap();
        std::fs::write(&flag, "1\n").unwrap();
        assert_eq!(config::read_wanted(&flag), Wanted::Enabled);
    }

    #[test]
    fn the_enable_toast_names_the_config_edit() {
        // A plugin that writes to a file the user owns has to say so; a backup
        // nobody is told about is not a safety net anyone can use.
        let quiet = enabled_message("sidebar usage enabled", false);
        let loud = enabled_message("sidebar usage enabled", true);
        assert_eq!(quiet, "sidebar usage enabled");
        assert!(loud.starts_with(&quiet), "got: {loud}");
        assert!(
            loud.contains("config.toml") && loud.contains("backup"),
            "{loud}"
        );
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
