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
use crate::config::{Config, Labels, DEFAULT_RAM_LABEL};
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

/// Compact absolute RAM: `"<x.x>G"` at/above 1024 MB, else `"<n>M"`
/// — the narrow form the sidebar falls back to.
fn compact_ram(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1}G", mb / 1024.0)
    } else {
        format!("{}M", mb.round() as i64)
    }
}

// ---- narrow metric cells ----------------------------------------------------
//
// The sidebar status, the window title, and the `--icons` preview all draw the
// same few characters, so they all build them here.

/// Separator between the cells of a narrow metric row.
const CELL_SEPARATOR: &str = " · ";

/// The narrow RAM cell — the tier's rendering of RAM as a percent of the
/// machine's total (`ram ░8%`).
///
/// RAM is the one metric that is not simply a percentage. With MemTotal
/// unreadable there is nothing to be a percentage *of*, so the cell falls back
/// to the compact absolute (`ram 1.5G`) the sidebar has always shown there. The
/// tier deliberately contributes nothing in that branch: a gauge glyph measures
/// a level, and drawing one beside an absolute figure would be inventing a
/// reading we do not have.
fn ram_cell(icons: IconSet, label: Option<&str>, mb: f64) -> String {
    ram_cell_of(icons, label, mb, proc::mem_total_mb())
}

/// [`ram_cell`] with the machine total injected.
///
/// The seam exists for the tests: the real total is read once and cached for the
/// process, so a test host with a readable `/proc/meminfo` could never reach the
/// fallback branch (and one without it could never reach the percent branch).
fn ram_cell_of(icons: IconSet, label: Option<&str>, mb: f64, mem_total_mb: f64) -> String {
    if mem_total_mb > 0.0 {
        // Same arithmetic and rounding as `proc::ram_pct`, so the Text tier
        // reproduces the pre-icons sidebar byte for byte.
        icons.ram(label, 100.0 * mb / mem_total_mb)
    } else {
        // No total to be a percent OF, so the tier contributes nothing: a gauge
        // glyph measures a level, and drawing one beside an absolute figure
        // would invent a reading we do not have.
        format!("{} {}", label.unwrap_or(DEFAULT_RAM_LABEL), compact_ram(mb))
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

/// Join the cells of one narrow row: `cpu ░26% · ram ░8% · bat ▓74%`.
///
/// Split from [`usage_row`] so the `--icons` preview can feed it fixed sample
/// percentages — a preview built out of the host's real RAM total would show a
/// different row on every machine — while still going through the one function
/// that decides cell order and separator.
pub fn metric_row(cpu: String, ram: String, battery: Option<String>) -> String {
    let mut cells = vec![cpu, ram];
    cells.extend(battery); // absent battery: no cell, no trailing separator
    cells.join(CELL_SEPARATOR)
}

/// The two per-space cells both narrow rows start with, built once so the row
/// that carries a battery and the row that cannot still agree on the first two.
fn usage_cells(cpu: f64, ram_mb: f64, labels: &Labels, icons: IconSet) -> (String, String) {
    (
        icons.cpu(labels.cpu(), cpu),
        ram_cell(icons, labels.ram(), ram_mb),
    )
}

/// One space's narrow row — what the sidebar card and the agents panel show.
///
/// Takes no battery, and that is the point rather than an omission: the battery
/// is one reading for the whole machine, so repeating it on every space's row
/// says the same number N times and reads as if each space had its own pack. The
/// machine-wide surfaces draw it instead — the window title via [`totals_row`],
/// the terminal report on its total line, and (on a patched build) herdr's own
/// sidebar header. Keeping the parameter off the signature is what stops a
/// future caller from putting it back by accident.
pub fn usage_row(cpu: f64, ram_mb: f64, labels: &Labels, icons: IconSet) -> String {
    let (cpu_cell, ram) = usage_cells(cpu, ram_mb, labels, icons);
    metric_row(cpu_cell, ram, None)
}

/// The all-space totals as a narrow row — [`usage_row`] plus the machine's one
/// battery cell. This is the window title.
///
/// `battery` is the reading taken once per refresh cycle by
/// [`Config::battery_reading`] and passed down, never re-read here.
pub fn totals_row(
    cpu: f64,
    ram_mb: f64,
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
) -> String {
    let (cpu_cell, ram) = usage_cells(cpu, ram_mb, labels, icons);
    metric_row(cpu_cell, ram, battery_cell(icons, labels, battery))
}

// ---- human render -----------------------------------------------------------

/// Format the per-space CPU/RAM report as a coloured, multi-line string.
///
/// `battery` lands on the total line and nowhere else. It is one number for the
/// whole machine, so stamping the same figure onto every space's row would be
/// noise in a report this wide — and worse, would read as if it were per-space.
pub fn render(
    spaces: &[Space],
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
) -> String {
    render_styled(spaces, labels, icons, battery, &Style::detect())
}

/// Colour-parametrised body of [`render`] (split out so tests can force a
/// deterministic no-colour rendering).
fn render_styled(
    spaces: &[Space],
    labels: &Labels,
    icons: IconSet,
    battery: Option<Battery>,
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
    // battery joins the row rather than looking bolted on.
    let total_battery_str = battery_cell(icons, labels, battery)
        .map(|cell| format!("   {cell}"))
        .unwrap_or_default();
    lines.push(style.dim(&format!(
        "  ── total   {} {:.1}%   {} {}{}{}",
        labels.cpu_word(),
        total_cpu,
        labels.ram_word(),
        fmt_ram(total_ram),
        total_pct_str,
        total_battery_str,
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
/// `battery` is the one reading taken for this snapshot, copied onto every row —
/// see [`JsonSpace::battery_percent`] for why it rides along per space.
pub fn render_json(spaces: &[Space], battery: Option<Battery>) -> String {
    let mem_total = proc::mem_total_mb();
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
        render(&spaces, labels, config.icon_set(), config.battery_reading()),
    );
    Ok(())
}

/// `--json`: print one JSON snapshot and return.
pub fn run_json(client: &mut Herdr, config: &Config) -> crate::Result<()> {
    let spaces = collect::snapshot(client, SNAPSHOT_WINDOW_MS)?;
    println!("{}", render_json(&spaces, config.battery_reading()));
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
        // On success, `snapshot` paces the loop via its internal
        // `thread::sleep(window_ms)` inside `measure`; on the error path it
        // returns before `measure`, so this frame has no delay of its own and
        // must sleep the cadence itself to avoid busy-spinning (mirrors the
        // daemon's error-branch sleep).
        let (body, failed) = match collect::snapshot(client, window_ms) {
            Ok(spaces) => (render(&spaces, labels, icons, battery), false),
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
    fn compact_ram_switches_unit_at_1024() {
        assert_eq!(compact_ram(0.0), "0M");
        assert_eq!(compact_ram(512.6), "513M"); // rounds to whole MB
        assert_eq!(compact_ram(1023.4), "1023M"); // still MB below the gate
        assert_eq!(compact_ram(1024.0), "1.0G");
        assert_eq!(compact_ram(1536.0), "1.5G");
    }

    // ---- narrow metric cells --------------------------------------------------

    #[test]
    fn ram_cell_rounds_exactly_as_ram_pct_does() {
        // The pre-icons sidebar showed `proc::ram_pct`, so the Text tier has to
        // reproduce it byte for byte — these are that function's own cases.
        assert_eq!(ram_cell_of(IconSet::Text, None, 1024.0, 16384.0), "ram 6%");
        // 100 * 250 / 10000 = 2.5 -> 3 (half away from zero).
        assert_eq!(ram_cell_of(IconSet::Text, None, 250.0, 10000.0), "ram 3%");
        // The tier decorates that same number, it does not change it.
        assert_eq!(
            ram_cell_of(IconSet::Unicode, None, 1024.0, 16384.0),
            "ram ░6%",
        );
    }

    #[test]
    fn ram_cell_falls_back_to_the_compact_absolute_without_a_total() {
        // No MemTotal means no scale to be a percentage of. The absolute is
        // shown with the label and *no* gauge: a gauge glyph claims a level, and
        // in this branch we have none to claim.
        assert_eq!(ram_cell_of(IconSet::Unicode, None, 1536.0, 0.0), "ram 1.5G");
        assert_eq!(ram_cell_of(IconSet::Text, None, 512.0, 0.0), "ram 512M");
        // Same for a nonsensical total, which would otherwise divide by zero.
        assert_eq!(ram_cell_of(IconSet::Emoji, None, 0.0, -1.0), "ram 0M");
    }

    #[test]
    fn metric_row_omits_an_absent_battery_and_its_separator() {
        let cells = |battery: Option<&str>| {
            metric_row(
                "A".to_string(),
                "B".to_string(),
                battery.map(str::to_string),
            )
        };
        assert_eq!(cells(None), "A · B"); // no dangling separator
        assert_eq!(cells(Some("C")), "A · B · C");
    }

    #[test]
    fn a_narrow_row_is_cpu_then_ram_then_battery() {
        // The whole row, spelled out with the machine total pinned (1310.72 MB
        // of 16384 MB is 8%) — the row builders read that total from the host.
        let row = metric_row(
            IconSet::Unicode.cpu(None, 26.0),
            ram_cell_of(IconSet::Unicode, None, 1310.72, 16384.0),
            battery_cell(
                IconSet::Unicode,
                &Labels::default(),
                Some(bat(74.0, State::Discharging)),
            ),
        );
        assert_eq!(row, "cpu ░26% · ram ░8% · bat ▓74%");
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
        let out = render_styled(&[], &Labels::default(), IconSet::Unicode, None, &plain());
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
        let out = render_styled(&[sp], &Labels::default(), IconSet::Unicode, None, &plain());
        assert!(out.contains("· 3 panes · +2 worktrees"), "{out}");
    }

    #[test]
    fn render_honours_custom_labels() {
        let labels = Labels::new(Some("CPU"), Some("MEM"), Some("PWR"));
        let out = render_styled(
            &[space("s", false, 1.0, 1.0, 1)],
            &labels,
            IconSet::Text,
            Some(bat(74.0, State::Discharging)),
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
            &plain(),
        );
        let without = render_styled(
            &spaces,
            &Labels::default(),
            IconSet::Unicode,
            None,
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

        let out = render_json(&[a, b], Some(bat(74.0, State::Discharging)));

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
    fn json_battery_pair_is_null_without_a_reading() {
        // A desktop (and a user who set `battery = false`) emits the keys as
        // `null` rather than dropping them — same rule `ram_percent` follows, so
        // a consumer can read the field unconditionally.
        let out = render_json(&[space("w1", true, 1.0, 1.0, 1)], None);
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
            let out = render_json(&[space("w1", true, 0.0, 0.0, 1)], Some(bat(5.0, state)));
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed[0]["battery_state"], key, "{out}");
        }
    }

    #[test]
    fn json_battery_percent_rounds_like_every_other_number() {
        let out = render_json(
            &[space("w1", true, 0.0, 0.0, 1)],
            Some(bat(63.46, State::Full)),
        );
        // 63.46 -> 63.5, and a whole percentage still collapses to an integer.
        assert!(out.contains("\"battery_percent\": 63.5"), "{out}");
        let whole = render_json(
            &[space("w1", true, 0.0, 0.0, 1)],
            Some(bat(100.0, State::Full)),
        );
        assert!(whole.contains("\"battery_percent\": 100,"), "{whole}");
    }

    #[test]
    fn json_empty_payload_is_bare_brackets() {
        assert_eq!(render_json(&[], None), "[]");
    }
}
