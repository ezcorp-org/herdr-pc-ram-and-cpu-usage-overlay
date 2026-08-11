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
mod disk;
mod herdr;
mod herdr_config;
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
/// Sample RAM figure, and the machine total it is a share of.
///
/// The pair is chosen so `SAMPLE_RAM_MB` is exactly [`SAMPLE_RAM`] percent of
/// `SAMPLE_MEM_TOTAL_MB` (1536 of 19200). That is what lets the preview draw the
/// real cell for whichever `ram_display` is configured — `8%` or `1.5G` — and
/// have both be the same reading rather than two unrelated samples.
const SAMPLE_RAM_MB: f64 = 1536.0;
const SAMPLE_MEM_TOTAL_MB: f64 = SAMPLE_RAM_MB * 100.0 / SAMPLE_RAM;
/// Sample battery: mid-ramp, so the tiers that vary their glyph by charge show
/// a middle one, and charging, so the `+` mark appears.
const SAMPLE_BATTERY: battery::Battery = battery::Battery {
    percent: 74.0,
    state: battery::State::Charging,
};

/// Sample drive: 272 GB used and 240 GB free of a 512 GB disk, so the preview
/// shows both figures a real cell carries — 53% used, `240G` left — at a size
/// people recognise.
///
/// A function rather than a `const` only because the reading names its mount.
fn sample_disk() -> disk::Disk {
    disk::Disk {
        name: "/".to_string(),
        free_mb: 240.0 * 1024.0,
        used_mb: 272.0 * 1024.0,
        total_mb: 512.0 * 1024.0,
    }
}

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
        print_icon_preview(
            &config,
            &config::load_herdr_labels().with_overrides(&config),
        );
        return Ok(());
    }

    // Read modes share one socket connection.
    let mut client = herdr::connect()?;
    if has_flag(&args, "--json") {
        return render::run_json(&mut client, &config);
    }

    let labels = config::load_herdr_labels().with_overrides(&config);
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
/// The rows use the user's own labels, the configured `ram_display`, and the
/// same [`render::metric_row`] every surface does, so what they see is what they
/// get. All four metrics are drawn — a space's row is the first two cells, and
/// the battery and disk join them on the window title and the report's total
/// line.
fn print_icon_preview(config: &config::Config, labels: &config::Labels) {
    let current = config.icon_set();
    let disk = sample_disk();
    println!(
        "\n  Icon tiers — {SAMPLE_CPU:.0}% cpu, {} ram, \
         {:.0}% battery charging, {:.0}% disk used, drawn by each tier:\n",
        // Named the way the configured `ram_display` will name it in the rows.
        render::ram_cell_of(
            icons::IconSet::Text,
            Some(""),
            SAMPLE_RAM_MB,
            config.ram_display,
            SAMPLE_MEM_TOTAL_MB,
        ),
        SAMPLE_BATTERY.percent,
        disk.used_percent(),
    );
    for (name, set) in ICON_TIERS {
        let row = render::metric_row(
            set.cpu(labels.cpu(), SAMPLE_CPU),
            // The real cell, not `set.ram`: `ram_display = "gb"` changes what a
            // RAM cell looks like, and a preview that showed a percent the rows
            // will never draw would be worse than no preview.
            render::ram_cell_of(
                set,
                labels.ram(),
                SAMPLE_RAM_MB,
                config.ram_display,
                SAMPLE_MEM_TOTAL_MB,
            ),
            Some(set.battery(labels.battery(), SAMPLE_BATTERY)),
            vec![render::disk_cell(set, labels.disk(), None, &disk)],
        );
        let marker = if set == current { "   <- current" } else { "" };
        println!("    {name:<10}{row}{marker}");
    }
    println!(
        "\n  text and unicode need no font installed. nerdfont needs a Nerd Font\n  \
         and emoji needs a colour emoji font — if a row above came out as boxes\n  \
         or blanks, that tier is not available here.\n\n  \
         Choose one with `icons = \"<tier>\"` in the plugin's config.toml. The\n  \
         default, `auto`, uses a Nerd Font when it finds one installed and plain\n  \
         text otherwise.\n"
    );
    print_one_setting_hint(current, config);
}

/// Explain — and print — the edits that keep herdr's own sidebar header in step
/// with these rows.
///
/// Worth the extra paragraph because the failure it prevents is invisible until
/// you look at the sidebar: on a patched build the whole-machine system-usage
/// header renders from herdr's `cpu_label` / `ram_label`, so setting only the
/// plugin's `icons` leaves that header spelling `cpu` in words directly above a
/// row of glyphs. Those two keys are the one place that changes both.
///
/// Two blocks, and which file each goes in is the point rather than a detail:
/// herdr knows `cpu_label` and `ram_label` and nothing else here, so a
/// `battery_label` or `disk_label` pasted into its `[ui]` earns an
/// `unknown config key` line in the log on every reload. Those two live in the
/// plugin's own config, which is the only thing that draws them.
fn print_one_setting_hint(current: icons::IconSet, config: &config::Config) {
    let indent = |snippet: String| -> String {
        snippet
            .lines()
            .map(|line| format!("      {line}\n"))
            .collect()
    };
    // A plugin-side label silently wins over the herdr block we are about to
    // print, so saying nothing would send the user to edit a file that cannot
    // take effect.
    let overridden = [
        ("cpu_label", config.cpu_label.is_some()),
        ("ram_label", config.ram_label.is_some()),
    ]
    .iter()
    .filter(|(_, set)| *set)
    .map(|(key, _)| *key)
    .collect::<Vec<_>>();
    let note = if overridden.is_empty() {
        String::new()
    } else {
        format!(
            "  NOTE: {} already set in the plugin's config.toml, which wins over\n  \
             herdr's. Remove it there for the block below to reach these rows.\n\n",
            overridden.join(" and "),
        )
    };
    println!(
        "  One setting for both: herdr's own sidebar system-usage header reads\n  \
         `cpu_label` / `ram_label` from ITS config, and this plugin honours the\n  \
         same keys — an explicit label replaces the tier's glyph rather than\n  \
         stacking with it. Set them once and the header and these rows agree.\n\n\
         {note}  \
         For the tier above, put this in herdr's config.toml:\n\n\
         {herdr_block}\n  \
         ..and this in the plugin's config.toml — herdr draws neither a battery\n  \
         nor a disk, so it has no key for either, and `icons = \"text\"` simply\n  \
         says the labels are doing the naming now:\n\n\
         {plugin_block}",
        herdr_block = indent(icons::herdr_ui_snippet(current)),
        plugin_block = indent(icons::plugin_config_snippet(current)),
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
