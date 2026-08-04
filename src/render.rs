//! Human and JSON rendering plus the `--once` / `--json` / `--interval` run modes.
//!
//! [`render`] builds the coloured multi-line terminal report; [`render_json`]
//! builds the machine-readable payload. The `run_*` helpers drive a
//! [`collect::snapshot`](crate::collect::snapshot) and print the result, with
//! `run_interval` clearing and redrawing each frame.

use std::io::{self, IsTerminal, Write};

use serde::Serialize;
use serde_json::Number;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::collect;
use crate::config::Labels;
use crate::herdr::Herdr;
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

// ---- human render -----------------------------------------------------------

/// Format the per-space CPU/RAM report as a coloured, multi-line string.
pub fn render(spaces: &[Space], labels: &Labels) -> String {
    render_styled(spaces, labels, &Style::detect())
}

/// Colour-parametrised body of [`render`] (split out so tests can force a
/// deterministic no-colour rendering).
fn render_styled(spaces: &[Space], labels: &Labels, style: &Style) -> String {
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
            labels.cpu,
            cpu_str,
            labels.ram,
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
    lines.push(style.dim(&format!(
        "  ── total   {} {:.1}%   {} {}{}",
        labels.cpu,
        total_cpu,
        labels.ram,
        fmt_ram(total_ram),
        total_pct_str,
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
pub fn render_json(spaces: &[Space]) -> String {
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
        })
        .collect();
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string())
}

// ---- run modes --------------------------------------------------------------

/// `--once`: print a single rendered snapshot and return.
pub fn run_once(client: &mut Herdr, labels: &Labels) -> crate::Result<()> {
    let spaces = collect::snapshot(client, SNAPSHOT_WINDOW_MS)?;
    println!("{}", render(&spaces, labels));
    Ok(())
}

/// `--json`: print one JSON snapshot and return.
pub fn run_json(client: &mut Herdr) -> crate::Result<()> {
    let spaces = collect::snapshot(client, SNAPSHOT_WINDOW_MS)?;
    println!("{}", render_json(&spaces));
    Ok(())
}

/// `--interval`: live watch, redrawing every `interval_ms` (first frame quick).
///
/// A background thread restores the cursor and exits on SIGINT/SIGTERM; the
/// main loop hides the cursor, then clears + redraws each frame,
/// widening the CPU window from the quick first frame to `interval_ms`.
pub fn run_interval(client: &mut Herdr, labels: &Labels, interval_ms: u64) -> crate::Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            print!("\x1b[?25h"); // show cursor
            let _ = io::stdout().flush();
            std::process::exit(0);
        }
    });

    let mut out = io::stdout();
    write!(out, "\x1b[?25l")?; // hide cursor
    out.flush()?;

    let mut window_ms = FIRST_FRAME_WINDOW_MS;
    loop {
        // On success, `snapshot` paces the loop via its internal
        // `thread::sleep(window_ms)` inside `measure`; on the error path it
        // returns before `measure`, so this frame has no delay of its own and
        // must sleep the cadence itself to avoid busy-spinning (mirrors the
        // daemon's error-branch sleep).
        let (body, failed) = match collect::snapshot(client, window_ms) {
            Ok(spaces) => (render(&spaces, labels), false),
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

/// Local wall-clock `HH:MM:SS` for the live-watch footer stamp (cosmetic — not
/// part of any output contract).
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

    // ---- fmt_ram: MB below 1024, GB at/above ---------------------------------

    #[test]
    fn fmt_ram_switches_unit_at_1024() {
        assert_eq!(fmt_ram(0.0), "0 MB");
        assert_eq!(fmt_ram(512.4), "512 MB"); // rounds to whole MB
        assert_eq!(fmt_ram(1023.9), "1024 MB"); // still MB below the 1024 gate
        assert_eq!(fmt_ram(1024.0), "1.00 GB");
        assert_eq!(fmt_ram(1536.0), "1.50 GB");
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
        let out = render_styled(&[], &Labels::default(), &plain());
        assert_eq!(out, "  CPU / RAM per space\n\n  No spaces open.");
    }

    #[test]
    fn render_lays_out_marker_branch_and_notes() {
        let mut focused = space("main", true, 5.0, 512.0, 2);
        focused.branch = "feature/x".to_string();
        let out = render_styled(&[focused], &Labels::default(), &plain());
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
        let out = render_styled(&[sp], &Labels::default(), &plain());
        assert!(out.contains("· 3 panes · +2 worktrees"), "{out}");
    }

    #[test]
    fn render_honours_custom_labels() {
        let labels = Labels {
            cpu: "CPU".to_string(),
            ram: "MEM".to_string(),
        };
        let out = render_styled(&[space("s", false, 1.0, 1.0, 1)], &labels, &plain());
        assert!(out.contains("CPU"));
        assert!(out.contains("MEM"));
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

        let out = render_json(&[a, b]);

        // Keys appear in the declared order.
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
    }

    #[test]
    fn json_empty_payload_is_bare_brackets() {
        assert_eq!(render_json(&[]), "[]");
    }
}
