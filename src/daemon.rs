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
//! One instance per *session*, not per machine. A daemon pushes over the one
//! socket it connected to, so a second herdr session needs a second daemon; the
//! pid file is therefore keyed by session (see [`config::pid_file`]) while the
//! state dir it sits in — and so the `enabled` marker, the plugin config, and the
//! row in herdr's own config — stays shared, because those are one decision for
//! the whole machine. That split is what `--disable` follows: it stands down
//! every session's updater, while `--enable` and `--restore` speak only for the
//! session that ran them.
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
use crate::disk::Disk;
use crate::herdr::{self, Herdr};
use crate::herdr_config::{self, Change};
use crate::icons::IconSet;
use crate::model::Space;
use crate::proc;
use crate::render::{self, RowStyle};

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

impl Settings {
    /// This cycle's presentation, as the one value the row builders take.
    ///
    /// Borrows rather than clones the labels: the style lives inside a single
    /// refresh, and the whole point of [`Settings`] is that there is one copy of
    /// these to disagree about.
    fn row_style(&self) -> RowStyle<'_> {
        RowStyle::from_config(&self.labels, self.icons, &self.config)
    }
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

/// PID of a live updater daemon **for this herdr session**, or `None` (missing
/// pid file / dead process / a pid that no longer belongs to us).
///
/// Reads `<state_dir>/updater-<session>.pid` and confirms the pid is live AND
/// really one of our processes ([`is_our_process`] answers both: a vanished pid
/// has no image name to read). That second check matters: the state dir outlives
/// reboots, so an unclean shutdown can leave a pid file pointing at a pid the
/// kernel later recycled for something else — and without it `--enable` would
/// no-op forever against that impostor, leaving the sidebar permanently blank.
///
/// Per session, not per machine: a daemon serves the one socket it connected to,
/// so another session's live updater is no reason for this one to stand down.
/// See [`config::pid_file`].
pub fn daemon_pid() -> Option<u32> {
    daemon_pid_at(&config::pid_file())
}

/// [`daemon_pid`] for an explicit pid file, so a test can put two sessions'
/// claims side by side.
fn daemon_pid_at(path: &std::path::Path) -> Option<u32> {
    let pid = read_pid_file_at(path)?;
    is_our_process(pid).then_some(pid)
}

/// What one updater's pid file says: the process holding the claim, and the
/// herdr socket it serves.
///
/// The socket rides along because a claim is only useful to another session if
/// it can be acted on, and every action worth taking — clearing a status,
/// clearing a title — goes over that session's own socket. It is `None` for a
/// file written before 1.11.1, which recorded the pid alone.
#[derive(Debug, PartialEq, Eq)]
struct Claim {
    pid: u32,
    socket: Option<std::path::PathBuf>,
}

/// Record this process's claim on `path`: its pid, and the socket it serves.
///
/// Two lines rather than one so the first stays exactly what it always was. The
/// only reader that could be surprised is an OLDER build parsing the whole file
/// as a number — a downgrade, which would read the file as unclaimed and start
/// a second updater in that one session. Both push the same rows, and the next
/// upgrade settles it.
fn write_claim(path: &std::path::Path, socket: Option<&std::path::Path>) -> std::io::Result<()> {
    let socket = socket.map(|s| s.display().to_string()).unwrap_or_default();
    std::fs::write(path, format!("{}\n{socket}\n", std::process::id()))
}

/// The pid recorded in this session's pid file, or `None` if the file is
/// missing, unparseable, or holds a non-positive pid. Says nothing about
/// whether that process is alive — [`daemon_pid`] adds that.
fn read_pid_file() -> Option<u32> {
    read_pid_file_at(&config::pid_file())
}

/// [`read_pid_file`] against an explicit path.
fn read_pid_file_at(path: &std::path::Path) -> Option<u32> {
    Some(read_claim_at(path)?.pid)
}

/// Parse the claim recorded at `path`.
///
/// First line only for the pid, so the socket line below it cannot turn a
/// perfectly good claim into "no updater here" — that answer starts a second
/// daemon. An empty or absent second line is a claim we can identify but not
/// reach, which is what a pre-1.11.1 file is.
fn read_claim_at(path: &std::path::Path) -> Option<Claim> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    // Parsed wide, then bounded, so the accepted set is exactly the pids a pid
    // can be. The old `i32` parse rejected everything above 2^31 — which no
    // Linux pid reaches (`pid_max` caps far below it) but a Windows one may,
    // pids there being a full 32 bits. A daemon whose own pid its own reader
    // threw out would hold a claim nothing could see, and the single-instance
    // guard would be off for that process's whole life.
    let pid: u64 = lines.next()?.trim().parse().ok()?;
    let socket = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(std::path::PathBuf::from);
    (pid > 0 && pid <= u64::from(u32::MAX)).then_some(Claim {
        pid: pid as u32,
        socket,
    })
}

/// Every claim recorded under the state dir, live or not, each with the pid
/// file holding it: this session's, every other session's, and the legacy
/// global one.
///
/// Only `--disable` wants this. Everything else asks about *this* session, via
/// [`daemon_pid`] — an updater that starts, hands over, or stands down on
/// another session's account is the bug this pairing exists to keep out.
///
/// Dead claims come back too, and are worth having: the socket a crashed daemon
/// recorded is the only way left to take back the rows it pushed, and in
/// agents-panel mode those rows carry no TTL to fall back on.
fn recorded_claims() -> Vec<(std::path::PathBuf, Claim)> {
    recorded_claims_among(config::pid_files())
}

/// [`recorded_claims`] over an explicit list of pid files, so a test can supply
/// them instead of the state dir the environment decides.
fn recorded_claims_among(pid_files: Vec<std::path::PathBuf>) -> Vec<(std::path::PathBuf, Claim)> {
    pid_files
        .into_iter()
        .filter_map(|path| Some((path.clone(), read_claim_at(&path)?)))
        .collect()
}

/// Whether `claim` names an updater that `--disable` should stop.
///
/// `is_live` is a parameter because the real one, [`is_our_process`], looks for
/// a live process running our own image — which no portable test can conjure a
/// second of.
///
/// Our own pid is not an updater, however convincingly a file claims it is.
/// `--disable` runs the same executable the daemon does, so [`is_our_process`]
/// cannot tell them apart, and a stale file the kernel has since recycled onto
/// THIS process would have `--disable` stop itself half way through — after
/// writing the marker and removing the row, before clearing a single status.
fn is_stoppable(claim: &Claim, is_live: impl Fn(u32) -> bool) -> bool {
    claim.pid != std::process::id() && is_live(claim.pid)
}

/// Unlink every pid file that names no live process of ours.
///
/// A daemon releases its own claim on the way out, so these are the ones that
/// never got the chance: a SIGKILL, a crash, a power cut. Left alone they
/// accumulate one per session ever run, and each is a lottery ticket in the
/// recycled-pid draw [`is_our_process`] cannot see through — a pid landing on
/// the user's own dashboard pane reads as an updater. Reaping them is safe by
/// definition: the file names nothing.
fn reap_dead_claims() {
    reap_dead_claims_among(config::pid_files(), is_our_process);
}

/// [`reap_dead_claims`] over an explicit list and liveness test — same seam, and
/// there for the same reason, as [`live_updaters_among`].
fn reap_dead_claims_among(pid_files: Vec<std::path::PathBuf>, is_live: impl Fn(u32) -> bool) {
    for path in pid_files {
        let live = read_claim_at(&path).is_some_and(|claim| is_live(claim.pid));
        if !live {
            let _ = std::fs::remove_file(&path);
        }
    }
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
    // The socket goes in beside the pid so a `--disable` run from another
    // session can clear what this daemon pushed. Nothing else can: it is
    // reached over this socket, and on Windows a terminated daemon runs no
    // shutdown of its own.
    write_claim(&config::pid_file(), herdr::socket_path().ok().as_deref())?;
    // Taken once, before any work: this is the build we are, and the loop
    // compares it against the file on disk to notice being replaced.
    let launched = current_stamp();

    // Re-read every cycle rather than once here — see `reload_settings`. The
    // first read also fixes the refresh cadence for the life of the daemon,
    // since that drives the sleep and the status TTL together.
    let mut settings = reload_settings();
    let daemon_interval_ms = settings.config.interval_seconds * 1000;

    let mut client = match herdr::connect() {
        Ok(client) => client,
        Err(err) => {
            // Nothing to run without a host connection — don't leave a pid file
            // pointing at a process that is about to exit. Guarded: losing the
            // single-instance race and failing to connect are both ordinary, and
            // together they would otherwise erase the winner's claim.
            release_pid_claim();
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
        let config = &settings.config;
        let style = settings.row_style();

        // Rebuilt underneath us? Then this process is the old version and
        // nothing else will ever notice: herdr runs no hook on install or
        // uninstall, and `--restore` — the one thing that fires afterwards —
        // leaves any live daemon alone by design. So the daemon that is being
        // replaced is the only party in a position to act, and it acts on
        // itself. Doing it here rather than from the outside also keeps the
        // single-instance claim honest: one process decides, releases, and
        // hands over, instead of two short-lived hooks racing to kill and spawn.
        //
        // REBUILT, though — not reinstalled. Measured against herdr 0.8.0: a
        // `herdr plugin install` moves the whole checkout aside into a
        // `.tmp-install-*/previous-checkout/` and deletes it, so this process's
        // executable is not replaced at its path but unlinked from under it.
        // `/proc/self/exe` then reads `<path> (deleted)`, `current_stamp` cannot
        // stat it, and an unreadable stamp is deliberately not a change (see
        // `build_changed` — guessing there would retire a healthy updater). A
        // `cargo build` in the same tree, which is the dev-link workflow, does
        // rewrite the file in place and IS caught.
        //
        // What that costs after a reinstall: this daemon keeps running the old
        // build until its herdr server goes away. The new build is not blocked
        // by it — each session's `--restore` claims its own pid file now and
        // starts its own updater — so the sidebar is served by the new code and
        // the old process is redundant rather than in the way. Before per-session
        // claims it was the other way round, and worse: the old daemon held the
        // one claim there was, so a reinstall did not take effect at all until
        // the server restarted. Left as it is because the fix is not obviously
        // safe — standing down on a vanished executable means `spawn_daemon`
        // has no file to spawn either.
        if let Some(launched) = launched.as_deref() {
            if build_changed(launched, current_stamp().as_deref())
                && !stopping.swap(true, Ordering::SeqCst)
            {
                hand_off(Some(&mut client), &tracked);
            }
        }

        match collect::snapshot(&mut client, window_ms) {
            Ok(spaces) => {
                if stopping.load(Ordering::SeqCst) {
                    park(); // shutdown ran during the sample window — do not re-report
                }
                {
                    let mut guard = tracked.lock().expect("tracked mutex poisoned");
                    push_statuses(&mut client, &spaces, config, style, &mut guard);
                }
                // The title is the only surface here that draws the machine-wide
                // metrics, so those reads live under its gate: a battery or a
                // drive nobody renders is a sysfs walk (or a `pmset` fork, or a
                // `statvfs` per drive) for nothing.
                if config.window_title_totals {
                    set_title_totals(&mut client, &spaces, config, style);
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
/// out, signal every session's daemon, and sweep any leftover statuses / title.
pub fn disable_updater() -> crate::Result<()> {
    // Record the intent so the restore hooks do not resurrect the updater on the
    // next herdr restart or space switch. This writes an explicit "off" rather
    // than deleting the marker: an absent marker now means "fresh install", and
    // deleting it would make every disable undo itself on the next restart.
    set_wanted(&config::enabled_flag(), Wanted::Disabled);
    // Reversible, as promised: whatever we added to herdr's config comes out —
    // and taking it out un-does the first-run setup, so let that setup run again.
    if herdr_config::remove_usage_row().is_ok_and(Change::needs_reload) {
        forget_bootstrap();
        reload_herdr_config();
    }

    // Every session's updater, not just this one's. The two things `--disable`
    // has just done — the shared marker and the row it took out of the one
    // config every session renders — are machine-wide, so leaving another
    // session's daemon running would have it push a token into a card that no
    // longer draws one, with nothing left to turn it off but a restart.
    let mine = herdr::socket_path().ok();
    let mut others = Vec::new();
    for (pid_file, claim) in recorded_claims() {
        if is_stoppable(&claim, is_our_process) {
            // Unix: SIGTERM, and the daemon clears its own statuses + title on
            // the way down. Windows: TerminateProcess — abrupt, so the sweep
            // below does that daemon's cleaning for it, over the socket it
            // recorded.
            proc::stop_process(claim.pid);
            release_stopped_claim(&pid_file, claim.pid);
        }
        // Swept either way. A claim whose process is already gone — a crash, a
        // SIGKILL, a session that ended badly — is the case where the rows are
        // certain to still be there and no daemon is left to take them back.
        if claim.socket != mine {
            others.push(claim.socket);
        }
    }

    // Belt and braces: sweep every pane of every session that ever recorded an
    // updater, plus our own — release pseudo-agents (no TTL) and clear metadata
    // statuses, then clear each session's title. If herdr is unavailable,
    // metadata TTLs expire the statuses anyway; the pseudo-agent rows have no
    // TTL, so this is the only thing that takes them back.
    //
    // Ours first, and the toast right behind it. This is the session the user
    // is looking at and the only one known to be answering — the action came in
    // over it — so it is both the sweep that matters and the one that cannot
    // stall. Reporting after the foreign sweeps would hide a finished job
    // behind a stranger's wedged socket.
    reap_dead_claims();
    sweep_sessions(vec![mine]);
    notify("sidebar usage disabled");
    sweep_other_sessions(others);
    Ok(())
}

/// How long `--disable` will wait on sessions other than its own, all together.
///
/// Generous next to the work — a healthy session is one snapshot and a handful
/// of clears — because the deadline is there for a session that has stopped
/// answering, not for a slow one.
const OTHER_SESSION_SWEEP: Duration = Duration::from_secs(10);

/// [`sweep_sessions`] for sessions that are not ours, under one deadline for
/// the lot of them.
///
/// These sockets come out of files, and nothing has shown the servers behind
/// them are alive. A session that has *gone* costs nothing — the connection is
/// refused. The case to bound is a session that connects and then says nothing:
/// unix caps each call at [`crate::herdr`]'s timeout, and Windows caps it at
/// nothing at all, a pipe opened as a `File` having no timeout to set. Either
/// way `--disable` is a command someone is waiting on, and it must not be a
/// command that never returns.
///
/// So the work goes on a thread and the deadline is on the wait, not on any one
/// call. Whatever is unfinished when time is up is abandoned — it dies with the
/// process moments later, and what it would have cleared is a row the metadata
/// TTL takes back anyway.
fn sweep_other_sessions(sockets: Vec<Option<std::path::PathBuf>>) {
    within(OTHER_SESSION_SWEEP, move || sweep_sessions(sockets));
}

/// Run `work` on a thread and wait no longer than `deadline` for it.
///
/// Returns as soon as the work is done, or when the deadline passes, whichever
/// comes first; the thread is left to die with the process. A panic in `work`
/// drops the sender and returns immediately, so a bug in there costs the wait
/// rather than being hidden by it.
///
/// Split out to be tested, which matters more than its four lines suggest. On
/// Windows this is the ONLY bound on the sweep — a named pipe opened as a
/// `File` has no timeout to set, so nothing under it can time out on its own —
/// and CI has no herdr to demonstrate that against. Taking a closure lets the
/// mechanism be pinned on every platform without one.
fn within(deadline: Duration, work: impl FnOnce() + Send + 'static) {
    let (done, wait) = std::sync::mpsc::channel();
    thread::spawn(move || {
        work();
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(deadline);
}

/// Everything one session could be carrying from us, as the set to clear.
///
/// A sweep runs where no record survives of what was actually pushed — another
/// session's daemon kept that in its own memory and is now gone — so it assumes
/// the most and clears it all. Clearing a pane we never touched is a no-op;
/// missing one is a reading that stays on screen.
///
/// A pseudo pane goes in BOTH buckets, and that is the whole reason this is a
/// function of its own. Agents-panel mode puts two things on that one pane —
/// the pseudo-agent that names the row and the token that fills it (see
/// [`push_statuses`]) — and listing it only as a pseudo released the row while
/// leaving the reading behind it, on screen until the token's TTL ran out, in a
/// session the user had just switched the overlay off in.
fn everything_we_could_have_pushed(targets: Vec<(String, collect::PaneRoles)>) -> Tracked {
    let mut sweep = Tracked::default();
    for (workspace, panes) in targets {
        sweep.metadata.extend(panes.pseudo_panes.iter().cloned());
        sweep.pseudo.extend(panes.pseudo_panes);
        sweep.metadata.extend(panes.agent_panes);
        sweep.metadata.extend(panes.spare_panes);
        sweep.workspaces.insert(workspace);
    }
    sweep
}

/// Clear everything this plugin pushed into each named session, skipping any we
/// cannot reach and any we have already done.
///
/// One session per socket, because a status can only be cleared over the
/// connection that set it. On unix the daemons clear up after themselves and
/// this is the belt to that pair of braces; on Windows they are terminated
/// outright and this is the only cleaning that happens at all.
fn sweep_sessions(sockets: Vec<Option<std::path::PathBuf>>) {
    let mut done = HashSet::new();
    for socket in sockets.into_iter().flatten() {
        if !done.insert(socket.clone()) {
            continue;
        }
        let Ok(mut client) = herdr::connect_to(socket) else {
            continue; // session gone, or a pre-1.11.1 claim that recorded none
        };
        if let Ok(targets) = collect::sweep_targets(&mut client) {
            clear_all(&mut client, &everything_we_could_have_pushed(targets));
        }
        let _ = client.window_title_clear();
    }
}

/// `--toggle`: disable if THIS session has a live daemon, else enable.
///
/// Reads this session rather than the machine so the action does what the
/// sidebar in front of you shows: a session with no updater turns one on, even
/// while another session has one running.
///
/// What it reads is per session; what it *does* is whatever the two halves do,
/// and those are not symmetric. Toggling on starts this session's updater, and
/// the others follow at their next `--restore`. Toggling off reaches every
/// session, because there is no such thing as a session-local "off": one marker
/// per user records the decision, and a session that tried to keep its own
/// updater down would have `--restore` bring it back on the next space switch.
/// Say so wherever this is documented — a user with two sessions who toggles one
/// dark and finds both dark is owed the reason.
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
    style: RowStyle,
    tracked: &mut Tracked,
) {
    let source = config::plugin_id();
    let ttl_ms = status_ttl_ms(config.interval_seconds);

    for sp in spaces {
        let status = status_line(sp, style);

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

/// Write the all-space CPU/RAM/battery/disk totals to the client window title.
///
/// Takes the readings off the cycle's `config` rather than as arguments: this is
/// the one surface the daemon draws them on, so nothing else has to carry the
/// values past a row that will not use them.
pub fn set_title_totals(client: &mut Herdr, spaces: &[Space], config: &Config, style: RowStyle) {
    let title = title_totals(
        spaces,
        style,
        config.battery_reading(),
        &config.disk_readings(),
    );
    let _ = client.window_title_set(&title);
}

/// The window title text: `"spaces · "` plus the per-space row's cells summed
/// over every space, then the machine-wide cells — one battery, one per selected
/// drive.
///
/// Pure, and split from [`set_title_totals`] so the formatting is testable
/// without a live herdr connection.
fn title_totals(
    spaces: &[Space],
    style: RowStyle,
    battery: Option<Battery>,
    disks: &[Disk],
) -> String {
    let mut cpu = 0.0;
    let mut ram_mb = 0.0;
    for sp in spaces {
        cpu += sp.cpu;
        ram_mb += sp.ram_mb;
    }
    format!(
        "spaces · {}",
        render::totals_row(cpu, ram_mb, style, battery, disks),
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

/// Forget that first-run setup ever happened, so the next `--enable` or
/// `--restore` does it again.
///
/// Called from exactly one place: [`disable_updater`], and only when it actually
/// removed OUR marked block from herdr's config. Without it the documented pair
/// `status-disable` then `status-enable` left the sidebar permanently blank —
/// the row was gone, the marker still said "already set up", so the second half
/// brought the updater back to push a token nothing rendered. That is the exact
/// "the plugin does nothing" symptom the first-run setup exists to prevent,
/// reachable through two documented actions.
///
/// Deliberately NOT called when the removal was a no-op. A row the *user* wrote
/// carries no marker and is left in place; a row the user *deleted* by hand
/// leaves nothing to remove. In both cases the marker stays, so `status-enable`
/// still never re-adds a row someone took out on purpose — the guarantee the
/// marker was introduced for.
fn forget_bootstrap() {
    forget_bootstrap_at(&config::bootstrapped_flag());
}

/// [`forget_bootstrap`] against an explicit path, so a test can exercise it
/// without the state dir the env decides. Best-effort: an unremovable marker
/// only costs the row, and `status-enable` says so on the toast either way.
fn forget_bootstrap_at(marker: &std::path::Path) {
    let _ = std::fs::remove_file(marker);
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

/// Whether the pid file still names `me`, and so is mine to remove.
///
/// A daemon that hands over spawns its successor, which immediately writes its
/// own pid. Removing the file unconditionally on the way out would take that
/// newcomer's claim with it and leave the next `--restore` free to start a
/// second updater alongside it. `disable_updater` guards its own removal the
/// same way, for the same reason.
fn claims_pid_file(recorded: Option<u32>, me: u32) -> bool {
    recorded == Some(me)
}

/// Give up the claim of a daemon `--disable` just terminated (Windows).
///
/// `TerminateProcess` runs no shutdown handler, so nothing else would ever
/// unlink the pid file: it outlives the daemon and leaves the recycled-pid check
/// ([`is_our_process`]) as the only thing standing between a stale pid and a
/// permanently no-op `--enable`.
///
/// Guarded on the file still naming the pid we stopped: a daemon started in the
/// window between [`recorded_claims`] and here owns its own file and must keep
/// it, or this would silently break its single-instance guard.
#[cfg(windows)]
fn release_stopped_claim(pid_file: &std::path::Path, pid: u32) {
    if claims_pid_file(read_pid_file_at(pid_file), pid) {
        let _ = std::fs::remove_file(pid_file);
    }
}

/// Nothing to do on unix: the daemon unlinks its own pid file from its SIGTERM
/// handler. Removing it out from under a daemon that is still shutting down
/// would let an immediate `--enable` start a second one.
#[cfg(not(windows))]
fn release_stopped_claim(_pid_file: &std::path::Path, _pid: u32) {}

/// Give up the single-instance claim, but only where it still names us.
///
/// Every exit path this process has goes through here, so none of them can undo
/// a claim that has since passed to another daemon — the failure that leaves one
/// updater running with no pid file, and so no way for `--disable` or `--toggle`
/// to ever find it again.
fn release_pid_claim() {
    if claims_pid_file(read_pid_file(), std::process::id()) {
        let _ = std::fs::remove_file(config::pid_file());
    }
}

/// [`stamp`] for the executable this process is running from, or `None` if it
/// cannot be read — see [`build_changed`] for why that answer stands nobody down.
fn current_stamp() -> Option<String> {
    let meta = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(stamp(mtime, meta.len()))
}

/// Identity of one build of our executable.
///
/// Deliberately not the crate version: a reinstall of the *same* version still
/// replaces the binary, and local `cargo build` iterations never bump it at all.
/// Modification time plus length moves on any rebuild that matters, needs no
/// process introspection, and is a plain string on every platform.
fn stamp(mtime_nanos: u128, len: u64) -> String {
    format!("{mtime_nanos}:{len}")
}

/// Whether the executable on disk is no longer the one this daemon launched
/// from — i.e. someone reinstalled or rebuilt underneath us.
///
/// An unreadable `current` is NOT a change. Failing to stat our own executable
/// says nothing about which build is running, and standing down on that guess
/// would blank the sidebar for no reason; the next cycle asks again seconds
/// later.
fn build_changed(launched: &str, current: Option<&str>) -> bool {
    matches!(current, Some(current) if current != launched)
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

/// Clear tracked statuses + title, give up the pid claim, and `exit(0)`.
///
/// Shared by the signal thread (own connection) and the five-failure path (main
/// connection). `client` is `None` only when no socket could be opened, in which
/// case the claim is still given up before exiting. Never returns.
fn shutdown(client: Option<&mut Herdr>, tracked: &Mutex<Tracked>) -> ! {
    release(client, tracked);
    std::process::exit(0);
}

/// Stand down in favour of a replacement built from the binary now on disk.
///
/// The ordering is the whole point. [`release`] unlinks the pid file *before*
/// the replacement is spawned, so the newcomer's single-instance check finds the
/// claim free and takes it. Spawning first would have it look up, see us still
/// holding the pid, and exit immediately — leaving the sidebar served by the
/// build we just decided to retire.
///
/// A spawn that fails costs nothing permanent: `--restore` runs on every
/// `workspace.focused`, so the next space switch starts one.
fn hand_off(client: Option<&mut Herdr>, tracked: &Mutex<Tracked>) -> ! {
    release(client, tracked);
    let _ = spawn_daemon();
    std::process::exit(0);
}

/// Clear every status this run pushed and give up the single-instance claim.
///
/// Shared by [`shutdown`] and [`hand_off`] so the two cannot drift on what
/// "stop being the updater" means — the only difference between them is what
/// happens afterwards.
fn release(client: Option<&mut Herdr>, tracked: &Mutex<Tracked>) {
    if let Some(client) = client {
        if let Ok(tracked) = tracked.lock() {
            clear_all(client, &tracked);
        }
        let _ = client.window_title_clear();
    }
    release_pid_claim();
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
            release_pid_claim();
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
fn status_line(sp: &Space, style: RowStyle) -> String {
    render::usage_row(sp.cpu, sp.ram_mb, style)
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
    use crate::config::{Labels, RamDisplay};
    use crate::testutil::scratch;

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

    /// Terse [`Disk`] builder: free and total in GB, as drives are quoted.
    fn drive(name: &str, free_gb: f64, total_gb: f64) -> Disk {
        Disk {
            name: name.to_string(),
            free_mb: free_gb * 1024.0,
            used_mb: (total_gb - free_gb) * 1024.0,
            total_mb: total_gb * 1024.0,
        }
    }

    // ---- standing down when the binary underneath us is replaced -------------

    #[test]
    fn both_halves_of_the_stamp_can_move_it_on_their_own() {
        // Size rides along with mtime because neither alone is enough: a coarse
        // filesystem clock or a restored timestamp can repeat an mtime, and a
        // rebuild can land on the same length.
        assert_ne!(stamp(42, 500), stamp(42, 505));
        assert_ne!(stamp(42, 500), stamp(43, 500));
        assert_eq!(stamp(42, 500), stamp(42, 500));
    }

    #[test]
    fn a_daemon_gives_up_only_a_claim_that_still_names_it() {
        // Handing over spawns a successor that writes its own pid. If the
        // outgoing daemon then deleted the file regardless, it would erase the
        // newcomer's single-instance claim and let a third daemon start
        // alongside it — which is how two updaters end up pushing the same rows.
        assert!(claims_pid_file(Some(42), 42));
        assert!(!claims_pid_file(Some(43), 42));
        assert!(!claims_pid_file(None, 42));
    }

    #[test]
    fn a_replaced_binary_is_a_changed_build() {
        assert!(build_changed("111:500", Some("222:505")));
    }

    #[test]
    fn the_binary_we_launched_from_is_not_a_changed_build() {
        assert!(!build_changed("222:505", Some("222:505")));
    }

    #[test]
    fn an_unreadable_binary_is_not_a_changed_build() {
        // Statting our own executable can fail — deleted mid-upgrade, an odd
        // mount. Standing down on that guess would blank the sidebar for no
        // reason, and the next cycle asks again seconds later.
        assert!(!build_changed("111:500", None));
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

    /// The plain-text style with default naming — what most of these tests want,
    /// since they are asserting layout rather than glyphs.
    fn text_style(labels: &Labels) -> RowStyle<'_> {
        RowStyle::new(labels, IconSet::Text, RamDisplay::Percent)
    }

    // ---- the sidebar status line --------------------------------------------

    #[test]
    fn status_line_uses_labels_and_rounds_cpu() {
        let labels = Labels::new(Some("CPU"), Some("MEM"), Some("PWR"), Some("DISK"));
        // The RAM cell depends on the host's MemTotal (percent when readable,
        // compact absolute when not), so assert the CPU rounding + label layout,
        // which are total-independent. `render::ram_cell_of` pins both branches.
        let line = status_line(&space(5.6, 0.0), text_style(&labels));
        assert!(line.starts_with("CPU 6% · MEM "), "got: {line}");
    }

    #[test]
    fn the_cycles_style_comes_from_the_cycles_config() {
        // The last link in the chain: `reload_settings` re-reads the config every
        // refresh, and `row_style` is what turns that into the value the row
        // builders take. If it dropped `ram_display` on the floor, editing the
        // config would change nothing on the sidebar and every other test here
        // would still be green — they all build a style by hand.
        let settings = Settings {
            config: Config {
                ram_display: RamDisplay::Absolute,
                ..Config::default()
            },
            labels: Labels::default(),
            icons: IconSet::Text,
        };
        assert_eq!(
            status_line(&space(26.0, 1536.0), settings.row_style()),
            "cpu 26% · ram 1.5G",
        );
    }

    #[test]
    fn status_line_draws_the_absolute_ram_the_config_asked_for() {
        // The sidebar row is where `ram_display` is actually read, and it is the
        // one surface no other test reaches with `Absolute` — the row builders
        // are pinned directly, but nothing proved the daemon hands the setting
        // down. 1536 MB is `1.5G` whatever this host's MemTotal happens to be,
        // which is exactly the point of the setting.
        let labels = Labels::default();
        let style = RowStyle::new(&labels, IconSet::Text, RamDisplay::Absolute);
        assert_eq!(
            status_line(&space(26.0, 1536.0), style),
            "cpu 26% · ram 1.5G"
        );
    }

    #[test]
    fn status_line_rounds_cpu_half_away_from_zero() {
        let labels = Labels::default();
        let line = |cpu| status_line(&space(cpu, 0.0), text_style(&labels));
        assert!(line(2.5).starts_with("cpu 3%"));
        assert!(line(2.4).starts_with("cpu 2%"));
    }

    #[test]
    fn a_space_row_is_the_machine_row_without_the_battery_or_the_disks() {
        // Both are single readings for the whole machine, so they belong on the
        // surfaces that draw the machine once — never copied onto each space,
        // where the same number repeated reads as if it were per-space.
        //
        // `status_line` cannot even be handed either now, so what is worth
        // pinning is the consequence: the row a space gets is exactly the
        // machine-wide row with those cells taken off. Host-independent —
        // whether this box has a pack or a second drive changes neither side.
        let labels = Labels::default();
        let sp = space(26.0, 0.0);
        let style = RowStyle::new(&labels, IconSet::Unicode, RamDisplay::Percent);
        let row = status_line(&sp, style);
        let machine = render::totals_row(
            sp.cpu,
            sp.ram_mb,
            style,
            Some(bat(74.0, State::Discharging)),
            &[drive("/", 240.0, 512.0)],
        );

        assert!(!row.contains("bat"), "got: {row}");
        assert!(!row.contains("disk"), "got: {row}");
        assert_eq!(
            machine.strip_suffix(" · bat ▓74% · disk ▒53% 240G"),
            Some(row.as_str()),
        );
    }

    #[test]
    fn status_line_draws_the_cpu_cell_in_every_tier_and_no_machine_wide_cell() {
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
            let line = status_line(&sp, RowStyle::new(&labels, icons, RamDisplay::Percent));
            assert!(line.starts_with(&format!("{cpu} · ")), "{icons:?}: {line}");
            // Each tier names these its own way, so check every spelling against
            // every row — a glyph tier smuggling one back in would slip straight
            // past a test that only looked for the word.
            for mark in ["bat", "\u{f241}", "🔋", "disk", "\u{f0a0}", "💾"] {
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
        let style = RowStyle::new(&labels, IconSet::Unicode, RamDisplay::Percent);
        let line = |state| title_totals(&[space(26.0, 0.0)], style, Some(bat(74.0, state)), &[]);
        assert!(line(State::Charging).ends_with("bat ▓74%+"));
        assert!(line(State::Discharging).ends_with("bat ▓74%"));
        assert!(line(State::Full).ends_with("bat ▓74%="));
        assert_ne!(line(State::Charging), line(State::Discharging));
    }

    #[test]
    fn title_totals_sums_the_spaces_and_carries_one_battery() {
        let labels = Labels::default();
        let spaces = [space(10.0, 0.0), space(16.4, 0.0)];
        let style = text_style(&labels);
        let with = title_totals(&spaces, style, Some(bat(74.0, State::Full)), &[]);
        let without = title_totals(&spaces, style, None, &[]);

        // 10.0 + 16.4 = 26.4, rounded once over the total rather than per space.
        assert!(with.starts_with("spaces · cpu 26% · ram "), "got: {with}");
        // One machine-wide cell on the title, `=` for a pack on power.
        assert!(with.ends_with(" · bat 74%="), "got: {with}");
        assert!(!without.contains("bat"), "got: {without}");
        assert_eq!(with.strip_suffix(" · bat 74%="), Some(without.as_str()));
    }

    #[test]
    fn the_title_ends_with_each_selected_drive() {
        // The ask, in the surface it was asked for: the top bar says how much
        // room is left, and on which drive when there is more than one.
        let labels = Labels::default();
        let spaces = [space(10.0, 0.0), space(16.4, 0.0)];
        let style = text_style(&labels);
        let title = |disks: &[Disk]| title_totals(&spaces, style, None, disks);

        let one = title(&[drive("/", 240.0, 512.0)]);
        assert!(one.ends_with(" · disk 53% 240G"), "got: {one}");

        let two = title(&[drive("/", 240.0, 512.0), drive("/data", 600.0, 2048.0)]);
        assert!(
            two.ends_with(" · disk / 53% 240G · disk /data 71% 600G"),
            "got: {two}",
        );

        // Nothing readable (or `disk = false`): the title is what it always was,
        // with no gap where a cell would have been.
        assert!(!title(&[]).contains("disk"), "got: {}", title(&[]));
    }

    #[test]
    fn the_title_puts_the_battery_before_the_drives() {
        // One machine-wide reading, then the drives — a fixed order, so the
        // title does not reshuffle when a laptop is plugged in.
        let labels = Labels::default();
        let title = title_totals(
            &[space(26.0, 0.0)],
            text_style(&labels),
            Some(bat(74.0, State::Discharging)),
            &[drive("/", 240.0, 512.0)],
        );
        assert!(
            title.ends_with(" · bat 74% · disk 53% 240G"),
            "got: {title}"
        );
    }

    // ---- restart recovery ---------------------------------------------------

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
    fn removing_our_row_forgets_the_first_run_marker() {
        // `status-disable` takes our row back out of herdr's config. If the
        // marker survived that, `status-enable` would skip first-run setup, the
        // row would stay gone, and the updater would come back to push a token
        // nothing renders — a blank sidebar reached through two documented
        // actions. Absent marker == setup runs again, which is the fix.
        let marker = scratch("forget").join("bootstrapped");
        set_wanted(&marker, Wanted::Enabled);
        assert!(marker.exists(), "first-run setup recorded");

        forget_bootstrap_at(&marker);
        assert!(!marker.exists(), "a later enable must set the row up again");

        // Idempotent: disabling twice, or before anything was ever written, is
        // not an error — nothing here may fail the action the user asked for.
        forget_bootstrap_at(&marker);
        assert!(!marker.exists());
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

    // ---- one updater per session ---------------------------------------------

    /// A pid file in `dir` claiming `pid` for `session_key`, in the two-line
    /// form a 1.11.1 daemon writes.
    fn claim(dir: &std::path::Path, session_key: &str, pid: u32) -> std::path::PathBuf {
        let path = dir.join(config::pid_file_name(session_key));
        std::fs::write(&path, format!("{pid}\n/run/{session_key}.sock\n"))
            .expect("fixture pid file");
        path
    }

    /// The claim `claim` writes, for comparing against what was read back.
    fn claim_of(session_key: &str, pid: u32) -> Claim {
        Claim {
            pid,
            socket: Some(std::path::PathBuf::from(format!("/run/{session_key}.sock"))),
        }
    }

    #[test]
    fn one_sessions_live_updater_does_not_stand_the_next_session_down() {
        // THE bug. herdr gives every session the same plugin state dir but its
        // own socket, and a daemon pushes over the one socket it connected to.
        // A single global claim therefore had the second session's `--restore`
        // find the FIRST session's live updater, stand down, and leave its
        // sidebar showing a `$usage` row with nothing in it — for as long as the
        // other session stayed up.
        let dir = scratch("two-sessions");
        let me = std::process::id();
        let first = claim(&dir, "42c3c964", me);
        let second = dir.join(config::pid_file_name("e60b12c5"));

        assert_eq!(daemon_pid_at(&first), Some(me), "the first session's own");
        assert_eq!(
            daemon_pid_at(&second),
            None,
            "the second session has to start one of its own",
        );
    }

    #[test]
    fn disable_stops_the_live_updaters_but_sweeps_every_session_it_can_name() {
        // What `--disable` acts on. It is one decision for the whole machine —
        // it writes the shared marker and takes our row out of the one config
        // every session renders — so a daemon another session started has to be
        // stopped too, or it keeps pushing into a card that no longer draws it.
        //
        // Three kinds are never stopped. A stale pid, because the state dir
        // outlives reboots and the kernel may since have recycled it onto
        // something else. An unparseable file, because it names nothing. And
        // our own pid: `--disable` runs the same executable the daemon does, so
        // a recycled pid landing HERE would have it stop itself half way
        // through, having written the marker and removed the row but cleared
        // not one status.
        //
        // The stale one is still SWEPT, which is the half that is easy to get
        // wrong: a daemon that crashed left its rows behind and no longer has a
        // process to take them back, and in agents-panel mode those rows have
        // no TTL to fall back on. Its socket is the only way to reach them, and
        // its pid file is the only place that socket is written down.
        let dir = scratch("sweep");
        // Everything is live to this liveness test EXCEPT the pid nothing can
        // be running under, so what the assertions pin is the code's own doing.
        let is_live = |pid: u32| pid != u32::MAX;
        let mine = claim(&dir, "42c3c964", 4242);
        let other_session = claim(&dir, "e60b12c5", 4343);
        let stale = claim(&dir, "deadbeef", u32::MAX);
        let ourselves = claim(&dir, "0badc0de", std::process::id());
        let unparseable = dir.join(config::pid_file_name("garbage"));
        std::fs::write(&unparseable, "not a pid\n").expect("fixture pid file");

        let claims = recorded_claims_among(vec![
            mine.clone(),
            other_session.clone(),
            stale.clone(),
            ourselves.clone(),
            unparseable,
        ]);

        // Every readable claim, so every session's socket is reachable.
        assert_eq!(
            claims,
            vec![
                (mine, claim_of("42c3c964", 4242)),
                (other_session, claim_of("e60b12c5", 4343)),
                (stale, claim_of("deadbeef", u32::MAX)),
                (ourselves, claim_of("0badc0de", std::process::id())),
            ],
        );
        // Of those, only the two live ones that are not us get stopped.
        let stopped: Vec<u32> = claims
            .iter()
            .filter(|(_, claim)| is_stoppable(claim, is_live))
            .map(|(_, claim)| claim.pid)
            .collect();
        assert_eq!(stopped, vec![4242, 4343]);
    }

    /// The Windows-only half of stopping another session's updater: there,
    /// `--disable` terminates the process outright, so nothing inside it ever
    /// unlinks its claim and this has to. Runs only where the code does — the
    /// unix build has no such function, its daemons unlinking their own claim
    /// from the SIGTERM handler.
    ///
    /// Worth having even though it is three lines: it was the one branch in
    /// this change that nothing anywhere executed. CI compiles the Windows arm
    /// and the tests exercised every other part of the path, which is a
    /// combination that reads as covered and is not.
    #[cfg(windows)]
    #[test]
    fn a_terminated_windows_daemon_has_its_claim_unlinked_for_it() {
        let dir = scratch("stopped-claim");
        let mine = claim(&dir, "42c3c964", 4242);
        release_stopped_claim(&mine, 4242);
        assert!(!mine.exists(), "the claim of the pid we stopped goes");

        // A daemon started between listing the claims and stopping them owns
        // its own file. Taking it would break the newcomer's single-instance
        // guard and let a third updater start beside it.
        let newcomer = claim(&dir, "e60b12c5", 4343);
        release_stopped_claim(&newcomer, 4242);
        assert!(newcomer.exists(), "a claim naming someone else stays");
    }

    #[test]
    fn a_deadline_returns_from_work_that_never_does() {
        // The one bound the Windows sweep has. A pipe opened as a `File` has no
        // timeout to set, so if this wait did not come back, `--disable` would
        // not either — a command the user is watching, hung on a session that
        // is nothing to do with them. Runs on every platform, which is the
        // point: CI has no herdr to wedge, and this needs none.
        let started = std::time::Instant::now();
        within(Duration::from_millis(50), || {
            thread::sleep(Duration::from_secs(3600))
        });
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "waited {:?}",
            started.elapsed(),
        );
    }

    #[test]
    fn work_that_finishes_does_not_serve_out_the_deadline() {
        // The other half, and the one a too-eager fix would break: the deadline
        // is a ceiling, not a delay. Every ordinary `--disable` goes through
        // here, so waiting it out would add ten seconds to the common case.
        let started = std::time::Instant::now();
        within(Duration::from_secs(3600), || {});
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "waited {:?}",
            started.elapsed(),
        );
    }

    #[test]
    fn a_session_that_is_not_there_costs_nothing_to_sweep() {
        // A recorded claim outlives the session that wrote it, so most sweeps
        // dial something that has gone. That has to fail immediately rather
        // than eat the deadline — on unix the connection is refused, on Windows
        // the pipe is simply not there to open, and this pins both.
        let dir = scratch("unreachable");
        let started = std::time::Instant::now();
        sweep_sessions(vec![Some(dir.join("herdr.sock")), None]);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "waited {:?}",
            started.elapsed(),
        );
    }

    #[test]
    fn a_sweep_takes_back_both_things_a_pseudo_pane_carries() {
        // Found end-to-end, in agents-panel mode across two sessions: the sweep
        // released the pseudo-agent row and left the reading sitting inside it,
        // because the pane went into the pseudo bucket only. It cleared itself
        // 15 s later when the token's TTL ran out — a stale figure in a session
        // the user had just switched the overlay off in, and in the one mode
        // where nothing else was going to clean up after another session's
        // daemon.
        let sweep = everything_we_could_have_pushed(vec![(
            "w1".to_string(),
            collect::PaneRoles {
                cwd: None,
                pseudo_panes: vec!["w1:p1".to_string()],
                agent_panes: vec!["w1:p2".to_string()],
                spare_panes: vec!["w1:p3".to_string()],
            },
        )]);

        assert!(sweep.pseudo.contains("w1:p1"), "the row is released");
        assert!(
            sweep.metadata.contains("w1:p1"),
            "and the reading inside it is cleared, not left to its TTL",
        );
        // The other two carry a token and no pseudo-agent.
        assert_eq!(sweep.pseudo.len(), 1);
        assert!(sweep.metadata.contains("w1:p2") && sweep.metadata.contains("w1:p3"));
        // Sidebar mode reports at the workspace level, so that has to go too.
        assert!(sweep.workspaces.contains("w1"));
    }

    #[test]
    fn a_claim_carries_the_socket_the_daemon_serves() {
        // The half that makes a cross-session `--disable` able to clean up at
        // all: statuses are cleared over the connection that set them, and on
        // Windows a terminated daemon clears nothing itself. A file from before
        // 1.11.1 names a pid and no socket — still a claim, just one we can
        // stop without being able to tidy after.
        let dir = scratch("claim-format");
        let path = dir.join("updater-42c3c964.pid");

        write_claim(&path, Some(std::path::Path::new("/run/a.sock"))).unwrap();
        assert_eq!(
            read_claim_at(&path),
            Some(Claim {
                pid: std::process::id(),
                socket: Some("/run/a.sock".into()),
            }),
        );

        // A second line that is absent or blank reads as "no socket recorded",
        // never as a broken claim — answering "no updater here" would start a
        // second one.
        for legacy in ["4242\n", "4242", "4242\n\n"] {
            std::fs::write(&path, legacy).unwrap();
            assert_eq!(
                read_claim_at(&path),
                Some(Claim {
                    pid: 4242,
                    socket: None
                }),
                "got: {legacy:?}",
            );
        }

        // The whole pid range, and nothing outside it. Windows pids are a full
        // 32 bits, so the top of the range is an ordinary pid there, not the
        // impossible value it looks like on Linux — and a claim thrown out for
        // being too large is a single-instance guard that never engages.
        for (text, expected) in [
            ("4294967295\n", Some(u32::MAX)),
            ("4294967296\n", None),
            ("0\n", None),
            ("-1\n", None),
            ("\n", None),
            ("", None),
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                read_claim_at(&path).map(|claim| claim.pid),
                expected,
                "got: {text:?}",
            );
        }
    }

    #[test]
    fn reaping_takes_the_claims_that_name_nothing_and_leaves_the_rest() {
        // Left alone these accumulate one per session ever run, and each is a
        // ticket in the recycled-pid draw `is_our_process` cannot see through.
        // Reaping is safe by definition: the file names no live process of ours.
        let dir = scratch("reap");
        let is_live = |pid: u32| pid != u32::MAX;
        let live = claim(&dir, "42c3c964", 4242);
        let dead = claim(&dir, "deadbeef", u32::MAX);
        let unparseable = dir.join(config::pid_file_name("garbage"));
        std::fs::write(&unparseable, "not a pid\n").expect("fixture pid file");

        reap_dead_claims_among(
            vec![live.clone(), dead.clone(), unparseable.clone()],
            is_live,
        );

        assert!(live.exists(), "a live updater keeps its claim");
        assert!(!dead.exists(), "a crashed one's claim goes");
        assert!(!unparseable.exists(), "so does a file naming nothing");
    }
}
