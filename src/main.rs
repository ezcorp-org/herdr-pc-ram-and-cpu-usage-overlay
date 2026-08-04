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
//!   --icons           preview every icon tier in this terminal and exit
//!   --enable          start the sidebar status updater daemon
//!   --disable         stop the daemon and clear statuses
//!   --toggle          enable/disable depending on daemon state
//!   --restore         internal: herdr `[[startup]]` hook — re-enable after a
//!                     herdr/machine restart if the updater was enabled
//!   --daemon          internal: run the updater loop (spawned by --enable)
//!
//! Linux and Windows: the `proc` module reads `/proc` on Linux and the Win32
//! process APIs on Windows. herdr injects HERDR_BIN_PATH / HERDR_PLUGIN_*.

mod battery;
mod collect;
mod config;
mod daemon;
mod herdr;
mod icons;
mod model;
// One `proc` module per platform, selected here so every consumer just says
// `proc::`. macOS is carved out of the unix arm because it has no `/proc`;
// the other BSDs stay on the sysfs reader, which is what they had before and
// is closer to right for them than the Darwin libproc backend would be.
#[cfg(all(unix, not(target_os = "macos")))]
mod proc;
#[cfg(target_os = "macos")]
#[path = "proc_macos.rs"]
mod proc;
#[cfg(windows)]
#[path = "proc_windows.rs"]
mod proc;
mod render;

use std::process;

/// Crate-wide fallible result; boxed error keeps the scaffold dependency-light.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Default live-watch refresh window when `--interval` is absent or invalid.
const DEFAULT_INTERVAL_MS: u64 = 2000;

/// Every icon tier with the name `icons = ` takes for it, laddering up by how
/// much each assumes about the user's font.
///
/// The names are the ones the user types into the plugin config, so
/// `icon_tier_names_are_the_ones_the_config_accepts` pins them against
/// [`icons::resolve`] — a preview that prints a name the config would not
/// accept is worse than no preview.
const ICON_TIERS: [(&str, icons::IconSet); 4] = [
    ("text", icons::IconSet::Text),
    ("unicode", icons::IconSet::Unicode),
    ("nerdfont", icons::IconSet::NerdFont),
    ("emoji", icons::IconSet::Emoji),
];

// The one sample reading `--icons` draws in every tier. Fixed rather than
// measured from the host: the preview is about glyphs, and a row that changes
// between runs is a row you cannot compare tiers with.

/// Sample CPU load — low enough to sit on the gauge's first step.
const SAMPLE_CPU: f64 = 26.0;
/// Sample RAM share.
const SAMPLE_RAM: f64 = 8.0;
/// Sample battery: mid-ramp, so the tiers that vary their glyph by charge show
/// a middle one, and charging, so the `+` mark appears.
const SAMPLE_BATTERY: battery::Battery = battery::Battery {
    percent: 74.0,
    state: battery::State::Charging,
};

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

    let config = config::load_config();

    // A pure local preview: it draws sample glyphs and reads two config files,
    // so it must work with herdr down — hence ahead of `connect`.
    if has_flag(&args, "--icons") {
        print_icon_preview(&config, &config::load_herdr_labels());
        return Ok(());
    }

    // Read modes share one socket connection.
    let mut client = herdr::connect()?;
    if has_flag(&args, "--json") {
        return render::run_json(&mut client, &config);
    }

    let labels = config::load_herdr_labels();
    if has_flag(&args, "--once") {
        return render::run_once(&mut client, &labels, &config);
    }

    render::run_interval(&mut client, &labels, &config, interval_ms(&args))
}

/// `--icons`: draw the same sample reading in all four tiers so the user can see
/// which glyphs *their* terminal and font actually produce before choosing one.
///
/// Nothing here can be answered from inside the program: whether a Nerd Font is
/// installed, or whether the terminal draws emoji at one column or two, is
/// visible only to the person looking at the screen. So the preview shows the
/// rows and lets them judge — a tier that comes out as boxes is one to avoid.
///
/// The rows use the user's own herdr `[ui]` labels and go through the same
/// [`render::metric_row`] the sidebar does, so what they see is what they get.
fn print_icon_preview(config: &config::Config, labels: &config::Labels) {
    let current = config.icon_set();
    println!(
        "\n  Icon tiers — {SAMPLE_CPU:.0}% cpu, {SAMPLE_RAM:.0}% ram, \
         {:.0}% battery charging, drawn by each tier:\n",
        SAMPLE_BATTERY.percent,
    );
    for (name, set) in ICON_TIERS {
        let row = render::metric_row(
            set.cpu(&labels.cpu, SAMPLE_CPU),
            set.ram(&labels.ram, SAMPLE_RAM),
            Some(set.battery(&labels.battery, SAMPLE_BATTERY)),
        );
        let marker = if set == current { "   <- current" } else { "" };
        println!("    {name:<10}{row}{marker}");
    }
    println!(
        "\n  text and unicode need no font installed. nerdfont needs a Nerd Font\n  \
         and emoji needs a colour emoji font — if either row above came out as\n  \
         boxes or blanks, that tier is not available here.\n\n  \
         Choose one with `icons = \"<tier>\"` in the plugin's config.toml. The\n  \
         default, `auto`, picks unicode on a UTF-8 locale and text otherwise.\n"
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the --icons preview -------------------------------------------------

    #[test]
    fn icon_tier_names_are_the_ones_the_config_accepts() {
        // The preview tells the user to type these names into `icons = `, so a
        // name the config parser would not recognise is a broken instruction.
        for (name, set) in ICON_TIERS {
            assert_eq!(icons::resolve(Some(name)), set, "{name}");
        }
    }

    #[test]
    fn every_tier_is_previewed() {
        // A tier the user can select but cannot preview is one they would have
        // to try blind, which is the whole problem `--icons` exists to solve.
        let previewed: Vec<icons::IconSet> = ICON_TIERS.iter().map(|&(_, set)| set).collect();
        for set in [
            icons::IconSet::Text,
            icons::IconSet::Unicode,
            icons::IconSet::NerdFont,
            icons::IconSet::Emoji,
        ] {
            assert!(previewed.contains(&set), "{set:?} is missing from --icons");
        }
    }
}
