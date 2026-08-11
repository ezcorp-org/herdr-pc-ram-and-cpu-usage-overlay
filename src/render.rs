//! Human and JSON rendering plus the `--once` / `--json` / `--interval` run modes.
//!
//! [`render`] builds the coloured multi-line terminal report; [`render_json`]
//! builds the machine-readable payload. The `run_*` helpers drive a
//! [`collect::snapshot`](crate::collect::snapshot) and print the result, with
//! `run_interval` clearing and redrawing each frame.
//!
//! Every surface's metric cells are assembled here — including the narrow ones
//! the sidebar daemon and the `--icons` preview draw ([`usage_row`] for one
//! space, [`totals_row`] for the whole machine) — so there is one place that
//! decides what a metric looks like and the surfaces cannot drift apart.

use std::io::{self, IsTerminal, Write};

use serde::Serialize;
use serde_json::Number;

use crate::battery::{Battery, State};
use crate::collect;
use crate::config::{Config, Labels, RamDisplay};
use crate::disk::Disk;
use crate::herdr::Herdr;
use crate::icons::IconSet;
use crate::model::Space;
use crate::proc;

/// CPU sample window for the one-shot `--once` / `--json` modes (ms) — short
/// enough that an action returns promptly.
const SNAPSHOT_WINDOW_MS: u64 = 300;

/// Short first-frame window so the live watch draws almost immediately, before
/// switching to full-interval windows.
const FIRST_FRAME_WINDOW_MS: u64 = 400;

// ---- ANSI styling -----------------------------------------------------------

/// ANSI paint gate: colours only when stdout is a TTY and `NO_COLOR` is unset
/// (an empty `NO_COLOR` is treated as absent).
struct Style {
    color: bool,
}

impl Style {
    /// Detect colour support from the live stdout.
    fn detect() -> Self {
        Style {
            color: io::stdout().is_terminal() && crate::config::non_empty_env("NO_COLOR").is_none(),
        }
    }

    /// Wrap `s` in the SGR `code` when colour is enabled, else return it plain.
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }

    /// Colour `s` by CPU load: `>= 80` red, `>= 40` yellow, else green
    /// on the share-of-machine scale.
    fn cpu(&self, v: f64, s: &str) -> String {
        if v >= 80.0 {
            self.red(s)
        } else if v >= 40.0 {
            self.yellow(s)
        } else {
            self.green(s)
        }
    }
}

/// Format RAM `mb` as `"<x.xx> GB"` at/above 1024 MB, else `"<x> MB"`
/// — the wide form used by the terminal report.
fn fmt_ram(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{} MB", mb.round() as i64)
    }
}

/// Compact absolute size for the narrow cells: `512M`, `1.5G`, `231G`, `1.2T`.
///
/// One decimal below 10 and none at or above it, which is the difference between
/// a RAM figure that needs the precision (`1.5G` says something `2G` does not)
/// and a disk figure that does not (`231G` is the number people quote; `231.0G`
/// is two characters of noise in a cell measured in columns).
///
/// One formatter for both metrics rather than two: they are the same job, and a
/// second copy is where the two would start disagreeing about what a gigabyte
/// looks like.
fn compact_size(mb: f64) -> String {
    const STEPS: [(f64, &str); 2] = [(1024.0 * 1024.0, "T"), (1024.0, "G")];
    for (scale, unit) in STEPS {
        if mb >= scale {
            let value = mb / scale;
            return if value >= 10.0 {
                format!("{}{unit}", value.round() as i64)
            } else {
                format!("{value:.1}{unit}")
            };
        }
    }
    format!("{}M", mb.round() as i64)
}

// ---- narrow metric cells ----------------------------------------------------
//
// The sidebar status, the window title, and the `--icons` preview all draw the
// same few characters, so they all build them here.

/// Separator between the cells of a narrow metric row.
const CELL_SEPARATOR: &str = " · ";

/// Everything that decides how a narrow row looks: the naming, the glyph
/// vocabulary, and the form the RAM figure takes.
///
/// Grouped for the same reason [`crate::daemon`]'s `Settings` is — these are one
/// decision, not three, and a caller that refreshed the labels while keeping a
/// stale tier would render a mixture. Passing them as parallel arguments also
/// meant every new presentation knob had to be threaded through five signatures
/// and every test call site; `ram_display` was the one that made that plain.
///
/// Narrow rows only, which is why it carries `ram_display`: the wide terminal
/// report always prints the absolute AND the percent, so it has nothing to
/// switch, and handing it a setting it ignores would invite a future caller to
/// believe it did something.
#[derive(Debug, Clone, Copy)]
pub struct RowStyle<'a> {
    labels: &'a Labels,
    icons: IconSet,
    ram_display: RamDisplay,
}

impl<'a> RowStyle<'a> {
    /// Build a style from its parts.
    ///
    /// `icons` is taken rather than re-derived because resolving the tier is a
    /// once-per-refresh decision for the whole machine — see
    /// [`crate::config::Config::icon_set`].
    pub fn new(labels: &'a Labels, icons: IconSet, ram_display: RamDisplay) -> Self {
        Self {
            labels,
            icons,
            ram_display,
        }
    }

    /// The style this plugin's own config asks for, with the tier already
    /// resolved by the caller.
    pub fn from_config(labels: &'a Labels, icons: IconSet, config: &Config) -> Self {
        Self::new(labels, icons, config.ram_display)
    }
}

/// The narrow RAM cell — the tier's rendering of RAM as a percent of the
/// machine's total (`ram ░8%`), or the compact absolute when `ram_display`
/// says so.
///
/// RAM is the one metric that is not simply a percentage. With MemTotal
/// unreadable there is nothing to be a percentage *of*, so the cell falls back
/// to the compact absolute (`ram 1.5G`) the sidebar has always shown there —
/// and `ram_display = "gb"` picks that same form on purpose, for readers who
/// want the figure rather than its share of this machine. In both absolute
/// branches the tier still names the metric but draws no gauge: a gauge measures
/// a level, and drawing one beside an absolute figure would invent a reading we
/// do not have. Naming and gauging are separate jobs — see
/// [`IconSet::ram_absolute`].
fn ram_cell(icons: IconSet, label: Option<&str>, mb: f64, display: RamDisplay) -> String {
    ram_cell_of(icons, label, mb, display, proc::mem_total_mb())
}

/// [`ram_cell`] with the machine total injected.
///
/// Two callers need that seam. The tests, because the real total is read once
/// and cached for the process, so a test host with a readable `/proc/meminfo`
/// could never reach the fallback branch (and one without it could never reach
/// the percent branch). And the `--icons` preview, which draws a fixed sample
/// reading rather than this machine's — a preview built from the host's real
/// total would show a different row on every machine, and could not show the
/// percent form at all on a host whose total is unreadable.
pub(crate) fn ram_cell_of(
    icons: IconSet,
    label: Option<&str>,
    mb: f64,
    display: RamDisplay,
    mem_total_mb: f64,
) -> String {
    if display == RamDisplay::Percent && mem_total_mb > 0.0 {
        // Same arithmetic and rounding as `proc::ram_pct`, so the Text tier
        // reproduces the pre-icons sidebar byte for byte.
        icons.ram(label, 100.0 * mb / mem_total_mb)
    } else {
        // The tier still names the metric; what it withholds is the gauge. See
        // [`IconSet::ram_absolute`] for why those are two different jobs.
        icons.ram_absolute(label, &compact_size(mb))
    }
}

/// The battery cell, or nothing when there is no reading to show.
///
/// A helper rather than a `map` at each call site so the sidebar row, the window
/// title, and the report's total line cannot disagree about what a battery looks
/// like. `reading` is already `None` when the user turned the metric off — see
/// [`Config::battery_reading`].
fn battery_cell(icons: IconSet, labels: &Labels, reading: Option<Battery>) -> Option<String> {
    reading.map(|reading| icons.battery(labels.battery(), reading))
}

/// One drive's cell — `disk 79% 240G`, or `disk /data 79% 240G` when `mount`
/// names it: the percentage USED, then the size still free.
///
/// The size is formatted here rather than in [`crate::icons`] because how a size
/// is spelled is this module's job and the tier's job is the glyph in front of
/// it. Public so the `--icons` preview draws the same cell every other surface
/// does.
pub fn disk_cell(
    icons: IconSet,
    label: Option<&str>,
    mount: Option<&str>,
    reading: &Disk,
) -> String {
    icons.disk(
        label,
        mount,
        reading.used_percent(),
        &compact_size(reading.free_mb),
    )
}

/// A cell per drive, in the order the user listed them.
///
/// The mount is named only when there is more than one: with a single drive
/// there is nothing to disambiguate, and these rows are measured in columns. Two
/// or more and every cell has to say which drive it is talking about, or the
/// title reads as one number contradicting another.
fn disk_cells(style: RowStyle, disks: &[Disk]) -> Vec<String> {
    let named = disks.len() > 1;
    disks
        .iter()
        .map(|reading| {
            disk_cell(
                style.icons,
                style.labels.disk(),
                named.then_some(reading.name.as_str()),
                reading,
            )
        })
        .collect()
}

/// Join the cells of one narrow row:
/// `cpu ░26% · ram ░8% · bat ▓74% · disk ▒79% 240G`.
///
/// Split from [`usage_row`] so the `--icons` preview can feed it fixed sample
/// percentages — a preview built out of the host's real RAM total would show a
/// different row on every machine — while still going through the one function
/// that decides cell order and separator.
pub fn metric_row(cpu: String, ram: String, battery: Option<String>, disks: Vec<String>) -> String {
    let mut cells = vec![cpu, ram];
    cells.extend(battery); // absent battery: no cell, no trailing separator
    cells.extend(disks); // ..and likewise a drive that could not be read
    cells.join(CELL_SEPARATOR)
}

/// The two per-space cells both narrow rows start with, built once so the row
/// that carries a battery and the row that cannot still agree on the first two.
fn usage_cells(cpu: f64, ram_mb: f64, style: RowStyle) -> (String, String) {
    (
        style.icons.cpu(style.labels.cpu(), cpu),
        ram_cell(style.icons, style.labels.ram(), ram_mb, style.ram_display),
    )
}

/// One space's narrow row — what the sidebar card and the agents panel show.
///
/// Takes no battery and no disk, and that is the point rather than an omission:
/// both are single readings for the whole machine, so repeating either on every
/// space's row says the same number N times and reads as if each space had its
/// own pack — or its own drive. The machine-wide surfaces draw them instead —
/// the window title via [`totals_row`], the terminal report on its total line, and (on a patched build) herdr's own
/// sidebar header. Keeping the parameter off the signature is what stops a
/// future caller from putting it back by accident.
pub fn usage_row(cpu: f64, ram_mb: f64, style: RowStyle) -> String {
    let (cpu_cell, ram) = usage_cells(cpu, ram_mb, style);
    metric_row(cpu_cell, ram, None, Vec::new())
}

/// The all-space totals as a narrow row — [`usage_row`] plus the machine's one
/// battery cell. This is the window title.
///
/// `battery` and `disks` are the readings taken once per refresh cycle by
/// [`Config::battery_reading`] and [`Config::disk_readings`] and passed down,
/// never re-read here.
pub fn totals_row(
    cpu: f64,
    ram_mb: f64,
    style: RowStyle,
    battery: Option<Battery>,
    disks: &[Disk],
) -> String {
    let (cpu_cell, ram) = usage_cells(cpu, ram_mb, style);
    metric_row(
        cpu_cell,
        ram,
        battery_cell(style.icons, style.labels, battery),
        disk_cells(style, disks),
    )
}

// ---- human render -----------------------------------------------------------

/// Format the per-space CPU/RAM report as a coloured, multi-line string.
///
/// `battery` and `disks` land on the total line and nowhere else. Each is one
/// figure for the whole machine, so stamping them onto every space's row would
/// be noise in a report this wide — and worse, would read as if they were
/// per-space.
pub fn render(
    spaces: &[Space],
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
    disks: &[Disk],
) -> String {
    render_styled(spaces, labels, icons, battery, disks, &Style::detect())
}

/// Colour-parametrised body of [`render`] (split out so tests can force a
/// deterministic no-colour rendering).
fn render_styled(
    spaces: &[Space],
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
    disks: &[Disk],
    style: &Style,
) -> String {
    let mut lines: Vec<String> = vec![style.bold("  CPU / RAM per space"), String::new()];
    if spaces.is_empty() {
        lines.push(style.dim("  No spaces open."));
        return lines.join("\n");
    }

    let mut total_cpu = 0.0;
    let mut total_ram = 0.0;
    for sp in spaces {
        total_cpu += sp.cpu;
        total_ram += sp.ram_mb;

        let marker = if sp.focused {
            style.green("●")
        } else {
            style.dim("○")
        };
        let branch = if sp.branch.is_empty() {
            "(no branch)"
        } else {
            &sp.branch
        };
        let cpu_cell = format!("{:.1}%", sp.cpu);
        let cpu_str = style.cpu(sp.cpu, &format!("{cpu_cell:>6}"));
        let ram_cell = format!("{:>8}", fmt_ram(sp.ram_mb));
        let pct = proc::ram_pct(sp.ram_mb);
        let pct_str = if pct.is_empty() {
            String::new()
        } else {
            style.dim(&format!(" ({pct})"))
        };

        let mut notes = vec![format!(
            "· {} pane{}",
            sp.pane_count,
            if sp.pane_count == 1 { "" } else { "s" }
        )];
        if let Some(worktrees) = &sp.worktree_labels {
            notes.push(format!(
                "· +{} worktree{}",
                worktrees.len(),
                if worktrees.len() == 1 { "" } else { "s" }
            ));
        }

        lines.push(format!("  {} {}", marker, style.bold(&sp.label)));
        lines.push(format!("      {}", style.dim(branch)));
        lines.push(format!(
            "      {} {}   {} {}{}   {}",
            labels.cpu_word(),
            cpu_str,
            labels.ram_word(),
            ram_cell,
            pct_str,
            style.dim(&notes.join(" ")),
        ));
        lines.push(String::new());
    }

    let total_pct = proc::ram_pct(total_ram);
    let total_pct_str = if total_pct.is_empty() {
        String::new()
    } else {
        format!(" ({total_pct})")
    };
    // Three spaces is the gap between the total line's other cells, so the
    // machine-wide cells join the row rather than looking bolted on. The report
    // is wide, so they use the percent form of the RAM cell's style regardless.
    let machine_cells: String = battery_cell(icons, labels, battery)
        .into_iter()
        .chain(disk_cells(
            RowStyle::new(labels, icons, RamDisplay::Percent),
            disks,
        ))
        .map(|cell| format!("   {cell}"))
        .collect();
    lines.push(style.dim(&format!(
        "  ── total   {} {:.1}%   {} {}{}{}",
        labels.cpu_word(),
        total_cpu,
        labels.ram_word(),
        fmt_ram(total_ram),
        total_pct_str,
        machine_cells,
    )));

    lines.join("\n")
}

// ---- JSON payload -----------------------------------------------------------

/// One entry of the `--json` payload. Field declaration order IS the emitted key
/// order (serde preserves it) and is the payload's public contract, so do not
/// reorder or rename.
#[derive(Serialize)]
struct JsonSpace {
    workspace_id: String,
    label: String,
    branch: String,
    focused: bool,
    panes: usize,
    processes: usize,
    cpu_percent: Number,
    ram_mb: Number,
    /// `null` when `/proc/meminfo` MemTotal is unreadable.
    ram_percent: Option<Number>,
    /// Present only for spaces that folded in worktree children; an absent
    /// value omits the key entirely rather than emitting `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    includes_worktrees: Option<Vec<String>>,
    /// Machine-wide battery charge, repeated on every row — the payload's top
    /// level is an array of spaces, and adding a wrapper object to hold one
    /// machine-wide field would break every existing consumer. `null` on a host
    /// with no battery (and when the user set `battery = false`, which reads the
    /// same way from here: there is nothing to report).
    ///
    /// Trailing, like [`Self::battery_state`], so the keys every existing
    /// consumer already reads keep their positions.
    battery_percent: Option<Number>,
    /// Charge state as a lowercase string (`charging`, `discharging`, `full`,
    /// `not_charging`, `unknown`), or `null` alongside a `null` percentage.
    battery_state: Option<String>,
    /// Space on each selected drive, machine-wide and so repeated on every row
    /// for the reason [`Self::battery_percent`] gives. `[]` when the user turned
    /// the metric off, and likewise when no selected drive could be read — an
    /// array that is always present is one a consumer can read unconditionally.
    disks: Vec<JsonDisk>,
}

/// One drive inside a [`JsonSpace`].
///
/// Every field is named for exactly what it holds, because "disk percent" is
/// the one figure on this payload a reader can take two ways: `used_percent` is
/// what `df` prints under `Use%`, and `used_mb + free_mb` is smaller than
/// `total_mb` on a unix filesystem by the blocks reserved for root — which is
/// why the percentage is not `100 - free/total`.
#[derive(Serialize, Clone)]
struct JsonDisk {
    name: String,
    used_percent: Number,
    free_mb: Number,
    used_mb: Number,
    total_mb: Number,
}

/// The wire spelling of a charge state: lowercase, `snake_case`, and stable.
///
/// Exhaustive on purpose — a new [`State`] variant must fail the build here
/// rather than silently serialize as something a consumer has never seen.
fn battery_state_key(state: State) -> &'static str {
    match state {
        State::Charging => "charging",
        State::Discharging => "discharging",
        State::Full => "full",
        State::NotCharging => "not_charging",
        State::Unknown => "unknown",
    }
}

/// Round to one decimal, then collapse a whole result to an integer so the
/// payload renders `12` rather than `12.0`.
fn json_num_1dp(x: f64) -> Number {
    let rounded = (x * 10.0).round() / 10.0;
    if rounded.is_finite() && rounded.fract() == 0.0 {
        Number::from(rounded as i64)
    } else {
        Number::from_f64(rounded).unwrap_or_else(|| Number::from(0))
    }
}

/// Serialize spaces to the `--json` payload (array of per-space objects), 2-space
/// indented. No trailing newline.
///
/// `battery` and `disks` are the readings taken once for this snapshot, copied
/// onto every row — see [`JsonSpace::battery_percent`] for why they ride along
/// per space.
pub fn render_json(spaces: &[Space], battery: Option<Battery>, disks: &[Disk]) -> String {
    let mem_total = proc::mem_total_mb();
    let json_disks: Vec<JsonDisk> = disks
        .iter()
        .map(|d| JsonDisk {
            name: d.name.clone(),
            used_percent: json_num_1dp(d.used_percent()),
            free_mb: json_num_1dp(d.free_mb),
            used_mb: json_num_1dp(d.used_mb),
            total_mb: json_num_1dp(d.total_mb),
        })
        .collect();
    let payload: Vec<JsonSpace> = spaces
        .iter()
        .map(|s| JsonSpace {
            workspace_id: s.id.clone(),
            label: s.label.clone(),
            branch: s.branch.clone(),
            focused: s.focused,
            panes: s.pane_count,
            processes: s.proc_count,
            cpu_percent: json_num_1dp(s.cpu),
            ram_mb: json_num_1dp(s.ram_mb),
            ram_percent: (mem_total > 0.0).then(|| json_num_1dp(100.0 * s.ram_mb / mem_total)),
            includes_worktrees: s.worktree_labels.clone(),
            battery_percent: battery.map(|b| json_num_1dp(b.percent)),
            battery_state: battery.map(|b| battery_state_key(b.state).to_string()),
            disks: json_disks.clone(),
        })
        .collect();
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string())
}

// ---- run modes --------------------------------------------------------------

/// `--once`: print a single rendered snapshot and return.
pub fn run_once(client: &mut Herdr, labels: &Labels, config: &Config) -> crate::Result<()> {
    let spaces = collect::snapshot(client, SNAPSHOT_WINDOW_MS)?;
    println!(
        "{}",
        render(
            &spaces,
            labels,
            config.icon_set(),
            config.battery_reading(),
            &config.disk_readings(),
        ),
    );
    Ok(())
}

/// `--json`: print one JSON snapshot and return.
pub fn run_json(client: &mut Herdr, config: &Config) -> crate::Result<()> {
    let spaces = collect::snapshot(client, SNAPSHOT_WINDOW_MS)?;
    println!(
        "{}",
        render_json(&spaces, config.battery_reading(), &config.disk_readings()),
    );
    Ok(())
}

/// `--interval`: live watch, redrawing every `interval_ms` (first frame quick).
///
/// A SIGINT/SIGTERM hook (a console ctrl handler on Windows) restores the
/// cursor and exits; the main loop hides the cursor, then clears + redraws each
/// frame, widening the CPU window from the quick first frame to `interval_ms`.
pub fn run_interval(
    client: &mut Herdr,
    labels: &Labels,
    config: &Config,
    interval_ms: u64,
) -> crate::Result<()> {
    install_quit_hook()?;

    let mut out = io::stdout();
    write!(out, "\x1b[?25l")?; // hide cursor
    out.flush()?;

    // The tier cannot change while we run, so it is resolved once; the charge
    // does change, so it is re-read once per frame — but only once, not once
    // per space.
    let icons = config.icon_set();
    let mut window_ms = FIRST_FRAME_WINDOW_MS;
    loop {
        let battery = config.battery_reading();
        let disks = config.disk_readings();
        // On success, `snapshot` paces the loop via its internal
        // `thread::sleep(window_ms)` inside `measure`; on the error path it
        // returns before `measure`, so this frame has no delay of its own and
        // must sleep the cadence itself to avoid busy-spinning (mirrors the
        // daemon's error-branch sleep).
        let (body, failed) = match collect::snapshot(client, window_ms) {
            Ok(spaces) => (render(&spaces, labels, icons, battery, &disks), false),
            Err(err) => (
                format!("{} {err}", Style::detect().red("  herdr unavailable:")),
                true,
            ),
        };
        let footer = Style::detect().dim(&format!(
            "  refreshing every {}s · {} · ctrl-c to quit",
            interval_ms as f64 / 1000.0,
            local_time_string(),
        ));
        write!(out, "\x1b[2J\x1b[H{body}\n\n{footer}\n")?;
        out.flush()?;
        if failed {
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
        window_ms = interval_ms;
    }
}

/// Show the cursor again and exit — the shared body of both quit hooks.
fn restore_cursor_and_exit() -> ! {
    print!("\x1b[?25h"); // show cursor
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// Restore the cursor on SIGINT/SIGTERM via a signal-hook thread.
#[cfg(unix)]
fn install_quit_hook() -> crate::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            restore_cursor_and_exit();
        }
    });
    Ok(())
}

/// Restore the cursor on Ctrl+C / console close via a console ctrl handler
/// (Windows delivers these on their own thread, so exiting from it is fine).
#[cfg(windows)]
fn install_quit_hook() -> crate::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn on_ctrl(_ctrl_type: u32) -> i32 {
        restore_cursor_and_exit();
    }
    // SAFETY: registering a handler with a 'static function pointer.
    if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Local wall-clock `HH:MM:SS` for the live-watch footer stamp (cosmetic — not
/// part of any output contract).
#[cfg(unix)]
fn local_time_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    // SAFETY: `localtime_r` fills the caller-owned `tm`; `secs` is a valid time_t.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Local wall-clock `HH:MM:SS` via `GetLocalTime` (already timezone-adjusted).
#[cfg(windows)]
fn local_time_string() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: `GetLocalTime` fills the caller-owned SYSTEMTIME.
    unsafe { GetLocalTime(&mut st) };
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style { color: false }
    }

    fn space(label: &str, focused: bool, cpu: f64, ram_mb: f64, panes: usize) -> Space {
        Space {
            id: label.to_string(),
            label: label.to_string(),
            focused,
            pane_count: panes,
            cpu,
            ram_mb,
            ..Default::default()
        }
    }

    /// Terse [`Battery`] builder for the cell tests.
    fn bat(percent: f64, state: State) -> Battery {
        Battery { percent, state }
    }

    /// Terse [`Disk`] builder: free and total in GB, which is how drives are
    /// quoted, converted to the MB the type stores. `used` is the rest of the
    /// disk, so these fixtures have no root reserve and `used% = 100 - free%`
    /// exactly — the reserve is `disk`'s business and is tested there.
    fn drive(name: &str, free_gb: f64, total_gb: f64) -> Disk {
        Disk {
            name: name.to_string(),
            free_mb: free_gb * 1024.0,
            used_mb: (total_gb - free_gb) * 1024.0,
            total_mb: total_gb * 1024.0,
        }
    }

    // ---- fmt_ram / compact_ram: MB below 1024, GB at/above -------------------

    #[test]
    fn fmt_ram_switches_unit_at_1024() {
        assert_eq!(fmt_ram(0.0), "0 MB");
        assert_eq!(fmt_ram(512.4), "512 MB"); // rounds to whole MB
        assert_eq!(fmt_ram(1023.9), "1024 MB"); // still MB below the 1024 gate
        assert_eq!(fmt_ram(1024.0), "1.00 GB");
        assert_eq!(fmt_ram(1536.0), "1.50 GB");
    }

    #[test]
    fn compact_size_switches_unit_at_1024() {
        assert_eq!(compact_size(0.0), "0M");
        assert_eq!(compact_size(512.6), "513M"); // rounds to whole MB
        assert_eq!(compact_size(1023.4), "1023M"); // still MB below the gate
        assert_eq!(compact_size(1024.0), "1.0G");
        assert_eq!(compact_size(1536.0), "1.5G");
    }

    #[test]
    fn compact_size_drops_the_decimal_once_the_figure_is_big() {
        // RAM is the reason for the decimal (`1.5G` says what `2G` cannot); a
        // disk is the reason for dropping it (`231.0G` is two columns of noise
        // in a cell measured in columns).
        assert_eq!(compact_size(9.9 * 1024.0), "9.9G");
        assert_eq!(compact_size(10.0 * 1024.0), "10G");
        assert_eq!(compact_size(231.4 * 1024.0), "231G");
        // ..and a drive big enough to leave GB behind.
        assert_eq!(compact_size(1024.0 * 1024.0), "1.0T");
        assert_eq!(compact_size(18.0 * 1024.0 * 1024.0), "18T");
    }

    // ---- narrow metric cells --------------------------------------------------

    #[test]
    fn ram_cell_rounds_exactly_as_ram_pct_does() {
        // The pre-icons sidebar showed `proc::ram_pct`, so the Text tier has to
        // reproduce it byte for byte — these are that function's own cases.
        assert_eq!(
            ram_cell_of(IconSet::Text, None, 1024.0, RamDisplay::Percent, 16384.0),
            "ram 6%"
        );
        // 100 * 250 / 10000 = 2.5 -> 3 (half away from zero).
        assert_eq!(
            ram_cell_of(IconSet::Text, None, 250.0, RamDisplay::Percent, 10000.0),
            "ram 3%"
        );
        // The tier decorates that same number, it does not change it.
        assert_eq!(
            ram_cell_of(IconSet::Unicode, None, 1024.0, RamDisplay::Percent, 16384.0),
            "ram ░6%",
        );
    }

    #[test]
    fn ram_cell_falls_back_to_the_compact_absolute_without_a_total() {
        // No MemTotal means no scale to be a percentage of. The absolute is
        // shown with *no* gauge: a gauge claims a level, and in this branch we
        // have none to claim.
        assert_eq!(
            ram_cell_of(IconSet::Unicode, None, 1536.0, RamDisplay::Percent, 0.0),
            "ram 1.5G"
        );
        assert_eq!(
            ram_cell_of(IconSet::Text, None, 512.0, RamDisplay::Percent, 0.0),
            "ram 512M"
        );
        // Same for a nonsensical total, which would otherwise divide by zero.
        // The glyph here is the metric's NAME, not a gauge, so it stays.
        assert_eq!(
            ram_cell_of(IconSet::Emoji, None, 0.0, RamDisplay::Percent, -1.0),
            "🧠0M"
        );
    }

    #[test]
    fn ram_display_absolute_ignores_a_readable_total() {
        // `ram_display = "gb"` asks for the figure itself, so a perfectly
        // readable MemTotal must not turn it back into a percent.
        let cell = |icons| ram_cell_of(icons, None, 1536.0, RamDisplay::Absolute, 16384.0);
        assert_eq!(cell(IconSet::Text), "ram 1.5G");
        assert_eq!(cell(IconSet::Unicode), "ram 1.5G");
        assert_eq!(
            ram_cell_of(IconSet::Text, None, 512.0, RamDisplay::Absolute, 16384.0),
            "ram 512M"
        );
    }

    #[test]
    fn the_absolute_cell_keeps_each_tier_naming_but_drops_the_gauge() {
        // The bug this pins: an absolute cell that named itself `ram` in EVERY
        // tier put the word `ram` next to a glyph `cpu` cell in the same row —
        // precisely the drift the tiers exist to prevent. Naming and gauging are
        // separate jobs, and only the gauge needs a level to measure.
        let cell = |icons| ram_cell_of(icons, None, 1536.0, RamDisplay::Absolute, 16384.0);
        assert_eq!(cell(IconSet::Text), "ram 1.5G");
        // Unicode names with the word and drops ONLY its gauge glyph.
        assert_eq!(cell(IconSet::Unicode), "ram 1.5G");
        assert_eq!(cell(IconSet::NerdFont), "\u{efc5} 1.5G");
        assert_eq!(cell(IconSet::Emoji), "🧠1.5G");

        // The row that showed the bug, spelled out: both cells speak the same
        // language. Asserted whole rather than as "no `ram` anywhere", because
        // the row is what the user reads and a substring check would pass on a
        // row that had gone wrong some other way.
        let labels = Labels::default();
        let row = |icons| {
            usage_row(
                26.0,
                1536.0,
                RowStyle::new(&labels, icons, RamDisplay::Absolute),
            )
        };
        assert_eq!(row(IconSet::NerdFont), "\u{f4bc} 26% · \u{efc5} 1.5G");
        assert_eq!(row(IconSet::Emoji), "💻26% · 🧠1.5G");
        assert_eq!(row(IconSet::Text), "cpu 26% · ram 1.5G");
    }

    #[test]
    fn an_empty_ram_label_leaves_just_the_figure_in_the_absolute_cell() {
        // The plugin config's "name nothing": no word and no stray leading
        // space, whether the absolute was chosen or fallen back to.
        assert_eq!(
            ram_cell_of(
                IconSet::Text,
                Some(""),
                1536.0,
                RamDisplay::Absolute,
                16384.0
            ),
            "1.5G"
        );
        assert_eq!(
            ram_cell_of(IconSet::Text, Some(""), 1536.0, RamDisplay::Percent, 0.0),
            "1.5G"
        );
        // "Name nothing" beats the tier's naming in every tier, glyph ones
        // included — otherwise the emoji would sneak back in as the name.
        for icons in [
            IconSet::Text,
            IconSet::Unicode,
            IconSet::NerdFont,
            IconSet::Emoji,
        ] {
            assert_eq!(
                ram_cell_of(icons, Some(""), 1536.0, RamDisplay::Absolute, 16384.0),
                "1.5G",
                "{icons:?}"
            );
        }
    }

    #[test]
    fn a_row_with_empty_labels_and_absolute_ram_is_just_the_figures() {
        // cpu_label = "" / ram_label = "" / ram_display = "gb" in the plugin
        // config: bare numbers, still separated — the compact row this feature
        // exists for.
        let labels = Labels::new(Some(""), Some(""), None, None);
        let row = usage_row(
            26.0,
            1536.0,
            RowStyle::new(&labels, IconSet::Text, RamDisplay::Absolute),
        );
        assert_eq!(row, "26% · 1.5G");
    }

    #[test]
    fn a_style_built_from_config_carries_that_configs_ram_form() {
        // The wiring, not the rendering. Every other test hands the row builders
        // a `RamDisplay` literal, so a `from_config` that read the wrong field —
        // or quietly hardcoded the default — would still pass all of them and
        // leave `ram_display = "gb"` doing nothing on a real machine.
        //
        // Asserted on the fields rather than on rendered output because the
        // percent branch needs a readable MemTotal, and a host without one would
        // render both forms identically and pass either way.
        let labels = Labels::default();
        for ram_display in [RamDisplay::Percent, RamDisplay::Absolute] {
            let config = Config {
                ram_display,
                ..Config::default()
            };
            let style = RowStyle::from_config(&labels, IconSet::NerdFont, &config);
            assert_eq!(style.ram_display, ram_display);
            // The tier is the caller's, resolved once per refresh, not re-derived
            // from the config here.
            assert_eq!(style.icons, IconSet::NerdFont);
        }
    }

    #[test]
    fn metric_row_omits_an_absent_battery_and_its_separator() {
        let cells = |battery: Option<&str>| {
            metric_row(
                "A".to_string(),
                "B".to_string(),
                battery.map(str::to_string),
                Vec::new(),
            )
        };
        assert_eq!(cells(None), "A · B"); // no dangling separator
        assert_eq!(cells(Some("C")), "A · B · C");
    }

    #[test]
    fn a_narrow_row_is_cpu_then_ram_then_battery_then_disks() {
        // The whole row, spelled out with the machine total pinned (1310.72 MB
        // of 16384 MB is 8%) — the row builders read that total from the host.
        let labels = Labels::default();
        let row = metric_row(
            IconSet::Unicode.cpu(None, 26.0),
            ram_cell_of(
                IconSet::Unicode,
                None,
                1310.72,
                RamDisplay::Percent,
                16384.0,
            ),
            battery_cell(
                IconSet::Unicode,
                &labels,
                Some(bat(74.0, State::Discharging)),
            ),
            disk_cells(
                RowStyle::new(&labels, IconSet::Unicode, RamDisplay::Percent),
                &[drive("/", 240.0, 512.0)],
            ),
        );
        assert_eq!(row, "cpu ░26% · ram ░8% · bat ▓74% · disk ▒53% 240G");
    }

    // ---- disk cells ----------------------------------------------------------

    /// The style the narrow surfaces use, with the percent RAM form.
    fn unicode_style(labels: &Labels) -> RowStyle<'_> {
        RowStyle::new(labels, IconSet::Unicode, RamDisplay::Percent)
    }

    #[test]
    fn a_disk_cell_shows_the_used_percent_then_the_free_size() {
        // The percentage is USED, like the cpu and ram cells beside it, so the
        // row reads one way. The size is what is LEFT — the figure you act on,
        // and unmistakable for a percentage because it carries a unit.
        let cell = |icons| disk_cell(icons, None, None, &drive("/", 240.0, 512.0));
        assert_eq!(cell(IconSet::Text), "disk 53% 240G");
        assert_eq!(cell(IconSet::Unicode), "disk ▒53% 240G");
        assert_eq!(cell(IconSet::NerdFont), "\u{f0a0} 53% 240G");
        assert_eq!(cell(IconSet::Emoji), "💾53% 240G");
    }

    #[test]
    fn a_nearly_full_drive_reads_alarming() {
        // The reading the metric exists for: 12 GB left of 512 GB. A full gauge
        // and a high percentage, exactly as a pegged cpu would read — which is
        // why the cell shows used rather than free.
        assert_eq!(
            disk_cell(IconSet::Unicode, None, None, &drive("/", 12.0, 512.0)),
            "disk █98% 12G",
        );
    }

    #[test]
    fn one_drive_is_unnamed_and_several_name_themselves() {
        let labels = Labels::default();
        let style = RowStyle::new(&labels, IconSet::Text, RamDisplay::Percent);
        let one = disk_cells(style, &[drive("/", 240.0, 512.0)]);
        assert_eq!(one, vec!["disk 53% 240G"], "nothing to disambiguate");

        let many = disk_cells(
            style,
            &[drive("/", 240.0, 512.0), drive("/data", 600.0, 2048.0)],
        );
        assert_eq!(
            many,
            vec!["disk / 53% 240G", "disk /data 71% 600G"],
            "two cells that do not say which drive is which are two numbers \
             contradicting each other",
        );
    }

    #[test]
    fn disk_cells_of_nothing_is_no_cells() {
        // `disk = false`, and a host where no selected drive answered, arrive
        // here the same way — and neither leaves a dangling separator.
        let labels = Labels::default();
        assert_eq!(
            disk_cells(unicode_style(&labels), &[]),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn a_custom_disk_label_replaces_the_tier_naming() {
        let labels = Labels::new(None, None, None, Some("free"));
        let style = RowStyle::new(&labels, IconSet::NerdFont, RamDisplay::Percent);
        assert_eq!(
            disk_cells(style, &[drive("/", 240.0, 512.0)]),
            vec!["free 53% 240G"],
        );
        // ..and an empty one names nothing at all, with no stray leading space.
        let bare = Labels::new(None, None, None, Some(""));
        let bare_style = RowStyle::new(&bare, IconSet::NerdFont, RamDisplay::Percent);
        assert_eq!(
            disk_cells(bare_style, &[drive("/", 240.0, 512.0)]),
            vec!["53% 240G"],
        );
    }

    #[test]
    fn metric_row_appends_a_cell_per_drive_after_the_battery() {
        let row = |disks: &[&str]| {
            metric_row(
                "A".to_string(),
                "B".to_string(),
                Some("C".to_string()),
                disks.iter().map(|d| d.to_string()).collect(),
            )
        };
        assert_eq!(row(&[]), "A · B · C"); // no drive read: no cell
        assert_eq!(row(&["D"]), "A · B · C · D");
        assert_eq!(row(&["D", "E"]), "A · B · C · D · E");
        // A machine with no battery still gets its drives, with no gap where the
        // battery would have been.
        assert_eq!(
            metric_row(
                "A".to_string(),
                "B".to_string(),
                None,
                vec!["D".to_string()],
            ),
            "A · B · D",
        );
    }

    // ---- render: the machine-wide drives live on the total line --------------

    #[test]
    fn render_puts_every_drive_on_the_total_line_only() {
        let spaces = [
            space("a", true, 1.0, 1.0, 1),
            space("b", false, 2.0, 2.0, 1),
        ];
        let out = render_styled(
            &spaces,
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[drive("/", 240.0, 512.0), drive("/data", 600.0, 2048.0)],
            &plain(),
        );
        let total = out
            .split('\n')
            .next_back()
            .expect("a total line")
            .to_string();

        assert!(total.contains("disk / ▒53% 240G"), "total: {total}");
        assert!(total.contains("disk /data ▓71% 600G"), "total: {total}");
        // Two spaces, one machine: the drives are named once, on the one line
        // that is about the whole machine.
        assert_eq!(out.matches("disk").count(), 2, "{out}");
    }

    #[test]
    fn render_orders_the_total_line_battery_then_drives() {
        let out = render_styled(
            &[space("a", true, 1.0, 1.0, 1)],
            &Labels::default(),
            IconSet::Text,
            Some(bat(74.0, State::Discharging)),
            &[drive("/", 240.0, 512.0)],
            &plain(),
        );
        let total = out
            .split('\n')
            .next_back()
            .expect("a total line")
            .to_string();
        assert!(
            total.ends_with("   bat 74%   disk 53% 240G"),
            "total: {total}",
        );
    }

    #[test]
    fn render_without_a_drive_is_the_report_unchanged() {
        // Additive, exactly like the battery: a host where nothing could be read
        // — or a user who set `disk = false` — gets the report as it was.
        let spaces = [space("a", true, 1.0, 1.0, 1)];
        let report = |disks: &[Disk]| {
            render_styled(
                &spaces,
                &Labels::default(),
                IconSet::Unicode,
                None,
                disks,
                &plain(),
            )
        };
        assert_eq!(
            report(&[drive("/", 240.0, 512.0)]).strip_suffix("   disk ▒53% 240G"),
            Some(report(&[]).as_str()),
        );
    }

    // ---- Style: gating + CPU thresholds --------------------------------------

    #[test]
    fn style_paints_only_when_colour_enabled() {
        assert_eq!(plain().bold("x"), "x");
        let colour = Style { color: true };
        assert_eq!(colour.bold("x"), "\x1b[1mx\x1b[0m");
        assert_eq!(colour.dim("x"), "\x1b[2mx\x1b[0m");
    }

    #[test]
    fn style_cpu_colour_thresholds() {
        let c = Style { color: true };
        assert_eq!(c.cpu(80.0, "H"), "\x1b[31mH\x1b[0m"); // >= 80 red
        assert_eq!(c.cpu(79.9, "M"), "\x1b[33mM\x1b[0m"); // >= 40 yellow
        assert_eq!(c.cpu(40.0, "M"), "\x1b[33mM\x1b[0m");
        assert_eq!(c.cpu(39.9, "L"), "\x1b[32mL\x1b[0m"); // else green
        assert_eq!(c.cpu(0.0, "L"), "\x1b[32mL\x1b[0m");
    }

    // ---- render: empty + populated -------------------------------------------

    #[test]
    fn render_empty_spaces() {
        let out = render_styled(
            &[],
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[],
            &plain(),
        );
        assert_eq!(out, "  CPU / RAM per space\n\n  No spaces open.");
    }

    #[test]
    fn render_lays_out_marker_branch_and_notes() {
        let mut focused = space("main", true, 5.0, 512.0, 2);
        focused.branch = "feature/x".to_string();
        let out = render_styled(
            &[focused],
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[],
            &plain(),
        );
        let lines: Vec<&str> = out.split('\n').collect();

        assert_eq!(lines[0], "  CPU / RAM per space");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "  ● main"); // focused marker + bold label
        assert_eq!(lines[3], "      feature/x"); // branch line
                                                 // cpu padded to width 6 ("5.0%" -> "  5.0%"), pane count singular/plural.
        assert!(lines[4].contains("cpu   5.0%"), "cpu cell: {}", lines[4]);
        assert!(lines[4].contains("· 2 panes"), "notes: {}", lines[4]);
        assert_eq!(lines[5], ""); // blank between space and total
        assert!(lines[6].starts_with("  ── total"), "total: {}", lines[6]);
    }

    #[test]
    fn render_unfocused_uses_no_branch_and_singular_pane() {
        let out = render_styled(
            &[space("s", false, 0.0, 0.0, 1)],
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[],
            &plain(),
        );
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[2], "  ○ s"); // unfocused marker
        assert_eq!(lines[3], "      (no branch)");
        assert!(lines[4].contains("· 1 pane") && !lines[4].contains("panes"));
    }

    #[test]
    fn render_shows_worktree_note() {
        let mut sp = space("repo", false, 0.0, 0.0, 3);
        sp.worktree_labels = Some(vec!["wt-a".to_string(), "wt-b".to_string()]);
        let out = render_styled(
            &[sp],
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[],
            &plain(),
        );
        assert!(out.contains("· 3 panes · +2 worktrees"), "{out}");
    }

    #[test]
    fn render_honours_custom_labels() {
        let labels = Labels::new(Some("CPU"), Some("MEM"), Some("PWR"), None);
        let out = render_styled(
            &[space("s", false, 1.0, 1.0, 1)],
            &labels,
            IconSet::Text,
            Some(bat(74.0, State::Discharging)),
            &[],
            &plain(),
        );
        assert!(out.contains("CPU"));
        assert!(out.contains("MEM"));
        assert!(out.contains("PWR 74%"), "{out}");
    }

    // ---- render: the machine-wide battery lives on the total line ------------

    #[test]
    fn render_puts_the_battery_on_the_total_line_only() {
        let spaces = [
            space("a", true, 1.0, 1.0, 1),
            space("b", false, 2.0, 2.0, 1),
        ];
        let out = render_styled(
            &spaces,
            &Labels::default(),
            IconSet::Unicode,
            Some(bat(74.0, State::Discharging)),
            &[],
            &plain(),
        );
        let lines: Vec<&str> = out.split('\n').collect();
        let total = lines.last().expect("a total line");

        assert!(total.contains("bat ▓74%"), "total: {total}");
        // Two spaces, one battery: a machine-wide number copied onto every row
        // would read as if each space had its own pack.
        assert_eq!(out.matches("bat").count(), 1, "{out}");
    }

    #[test]
    fn render_without_a_battery_is_the_report_unchanged() {
        // The cell is additive — a battery-less host gets byte-for-byte the
        // report this plugin printed before the metric existed.
        let spaces = [space("a", true, 1.0, 1.0, 1)];
        let with = render_styled(
            &spaces,
            &Labels::default(),
            IconSet::Unicode,
            Some(bat(74.0, State::Discharging)),
            &[],
            &plain(),
        );
        let without = render_styled(
            &spaces,
            &Labels::default(),
            IconSet::Unicode,
            None,
            &[],
            &plain(),
        );
        assert_eq!(with.strip_suffix("   bat ▓74%"), Some(without.as_str()));
    }

    // ---- json: number shape + field ordering ---------------------------------

    #[test]
    fn json_num_collapses_whole_and_rounds_to_one_dp() {
        assert_eq!(serde_json::to_string(&json_num_1dp(12.0)).unwrap(), "12");
        assert_eq!(serde_json::to_string(&json_num_1dp(0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&json_num_1dp(100.0)).unwrap(), "100");
        assert_eq!(serde_json::to_string(&json_num_1dp(5.14)).unwrap(), "5.1");
        assert_eq!(serde_json::to_string(&json_num_1dp(5.16)).unwrap(), "5.2");
    }

    #[test]
    fn json_field_order_and_conditional_worktrees() {
        let mut a = space("w1", true, 12.0, 100.0, 2);
        a.branch = "main".to_string();
        a.proc_count = 7;
        a.worktree_labels = Some(vec!["child".to_string()]);
        let b = space("w2", false, 0.0, 0.0, 1); // no worktrees

        let out = render_json(
            &[a, b],
            Some(bat(74.0, State::Discharging)),
            &[drive("/", 240.0, 512.0)],
        );

        // Keys appear in the declared order. The battery pair is appended at the
        // END: every key an existing consumer reads keeps the position it had.
        let order = [
            "workspace_id",
            "label",
            "branch",
            "focused",
            "panes",
            "processes",
            "cpu_percent",
            "ram_mb",
            "ram_percent",
            "includes_worktrees",
            "battery_percent",
            "battery_state",
            "disks",
        ];
        let mut last = 0;
        for key in order {
            let at = out
                .find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(at >= last, "key {key} out of order");
            last = at;
        }

        // First object collapses cpu 12.0 -> 12 and carries the worktree array.
        assert!(out.contains("\"cpu_percent\": 12,"), "{out}");
        assert!(out.contains("\"includes_worktrees\": ["), "{out}");
        assert!(out.contains("\"child\""), "{out}");

        // Second object omits includes_worktrees entirely.
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed[1].get("includes_worktrees").is_none());
        // ram_percent is always present (number or null), never dropped.
        assert!(parsed[0].get("ram_percent").is_some());
        assert!(parsed[1].get("ram_percent").is_some());
        // The battery is machine-wide, so every row carries the same reading —
        // there is no per-space wrapper to hang it off without breaking the
        // top-level array every consumer already parses.
        for row in [&parsed[0], &parsed[1]] {
            assert_eq!(row["battery_percent"], 74.0);
            assert_eq!(row["battery_state"], "discharging");
        }
    }

    #[test]
    fn json_carries_every_drive_with_named_figures() {
        let out = render_json(
            &[space("w1", true, 1.0, 1.0, 1)],
            None,
            &[drive("/", 240.0, 512.0), drive("/data", 600.0, 2048.0)],
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let disks = parsed[0]["disks"].as_array().expect("a disks array");

        assert_eq!(disks.len(), 2, "{out}");
        assert_eq!(disks[0]["name"], "/");
        assert_eq!(disks[0]["free_mb"], 245_760.0); // 240 GB
        assert_eq!(disks[0]["used_mb"], 278_528.0); // 272 GB
        assert_eq!(disks[0]["total_mb"], 524_288.0); // 512 GB
                                                     // USED percent, matching the cells — 272 of 512.
        assert_eq!(disks[0]["used_percent"], 53.1);
        // Order follows the user's selection, so a consumer can index it.
        assert_eq!(disks[1]["name"], "/data");
        assert_eq!(disks[1]["used_percent"], 70.7);
    }

    #[test]
    fn json_disks_is_an_empty_array_not_a_missing_key() {
        // `disk = false`, and a host where nothing could be read, both emit `[]`
        // — a key a consumer can read unconditionally, the rule the rest of this
        // payload already follows.
        let out = render_json(&[space("w1", true, 1.0, 1.0, 1)], None, &[]);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["disks"], serde_json::json!([]), "{out}");
        assert!(out.contains("\"disks\": []"), "{out}");
    }

    #[test]
    fn json_battery_pair_is_null_without_a_reading() {
        // A desktop (and a user who set `battery = false`) emits the keys as
        // `null` rather than dropping them — same rule `ram_percent` follows, so
        // a consumer can read the field unconditionally.
        let out = render_json(&[space("w1", true, 1.0, 1.0, 1)], None, &[]);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed[0]["battery_percent"].is_null(), "{out}");
        assert!(parsed[0]["battery_state"].is_null(), "{out}");
        assert!(out.contains("\"battery_percent\": null"), "{out}");
    }

    #[test]
    fn json_battery_state_is_lowercase_for_every_state() {
        // The wire spelling is a contract: consumers match on these strings.
        let expected = [
            (State::Charging, "charging"),
            (State::Discharging, "discharging"),
            (State::Full, "full"),
            (State::NotCharging, "not_charging"),
            (State::Unknown, "unknown"),
        ];
        for (state, key) in expected {
            assert_eq!(battery_state_key(state), key);
            let out = render_json(
                &[space("w1", true, 0.0, 0.0, 1)],
                Some(bat(5.0, state)),
                &[],
            );
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed[0]["battery_state"], key, "{out}");
        }
    }

    #[test]
    fn json_battery_percent_rounds_like_every_other_number() {
        let out = render_json(
            &[space("w1", true, 0.0, 0.0, 1)],
            Some(bat(63.46, State::Full)),
            &[],
        );
        // 63.46 -> 63.5, and a whole percentage still collapses to an integer.
        assert!(out.contains("\"battery_percent\": 63.5"), "{out}");
        let whole = render_json(
            &[space("w1", true, 0.0, 0.0, 1)],
            Some(bat(100.0, State::Full)),
            &[],
        );
        assert!(whole.contains("\"battery_percent\": 100,"), "{whole}");
    }

    #[test]
    fn json_empty_payload_is_bare_brackets() {
        assert_eq!(render_json(&[], None, &[]), "[]");
    }
}
