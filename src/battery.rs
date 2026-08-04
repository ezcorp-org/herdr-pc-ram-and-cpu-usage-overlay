//! Battery presence and charge state, one backend per platform.
//!
//! [`read`] answers a single question for the renderer: *what should the battery
//! cell show, if anything?* `None` is a first-class answer — a desktop, a
//! server, or a VM has no battery, and the metric has to disappear rather than
//! render a fabricated 0%.
//!
//! Every backend is split in two: an impure half that touches the host (a sysfs
//! walk, a `pmset` child process, a Win32 call) and a pure half that turns the
//! text or the raw fields it produced into a [`Battery`]. Only the impure halves
//! carry `#[cfg]` — the parsers compile, and are unit-tested, on every target,
//! so a mistake in the macOS parser is caught by a Linux test run.

/// Charge state as reported by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

/// A machine-wide battery reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    /// Charge percentage, 0..=100.
    pub percent: f64,
    pub state: State,
}

/// Current battery reading, or `None` when this host has no battery.
///
/// Backends: `/sys/class/power_supply` on Linux, `pmset -g batt` on macOS,
/// `GetSystemPowerStatus` on Windows, and "no battery" everywhere else. A
/// backend that cannot answer — missing files, a failed call, a percentage the
/// OS itself flags as unknown — also returns `None`: hiding the cell is always
/// better than showing a number we invented.
pub fn read() -> Option<Battery> {
    host_read()
}

/// Linux: walk the `power_supply` class.
#[cfg(target_os = "linux")]
fn host_read() -> Option<Battery> {
    sysfs::read_from_root(std::path::Path::new(sysfs::ROOT))
}

/// macOS: ask `pmset`.
#[cfg(target_os = "macos")]
fn host_read() -> Option<Battery> {
    pmset::probe()
}

/// Windows: ask `kernel32`.
#[cfg(windows)]
fn host_read() -> Option<Battery> {
    power_status::probe()
}

/// Every other target (the BSDs, illumos, ..): no battery API we speak, so the
/// caller hides the metric exactly as it would on a desktop.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn host_read() -> Option<Battery> {
    None
}

/// The single gate every backend pushes its percentage through: pin it into the
/// 0..=100 the UI can render, or reject it outright.
///
/// Hosts do lie. A freshly calibrated pack reports `energy_now` above
/// `energy_full` (>100%), a dying one can report a negative charge, and a
/// division by a zeroed capacity yields NaN — which would otherwise sail
/// through `clamp` unchanged and render as `NaN%`. `None` means "nothing worth
/// rendering", which the callers already handle.
fn checked_percent(percent: f64) -> Option<f64> {
    percent.is_finite().then(|| percent.clamp(0.0, 100.0))
}

// ---- Linux: /sys/class/power_supply -----------------------------------------

/// Linux battery backend: the `power_supply` class in sysfs.
///
/// Compiled on every target — only [`read_from_root`](sysfs::read_from_root)'s
/// caller is Linux-gated — so the filtering, parsing, and aggregation stay under
/// test everywhere. Off Linux nothing but those tests reaches the module, hence
/// the `dead_code` waiver.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod sysfs {
    use super::{checked_percent, Battery, State};
    use std::path::Path;

    /// The class directory every power supply registers under — batteries, AC
    /// adapters, and USB peripherals alike.
    pub const ROOT: &str = "/sys/class/power_supply";

    /// One `power_supply` entry that passed the battery filter.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Supply {
        /// Charge percentage, already clamped to 0..=100.
        pub percent: f64,
        pub state: State,
        /// `(now, full)` from the `energy_*` pair, else the `charge_*` pair,
        /// when both sides are readable and `full` is positive. Kept so a
        /// multi-pack host can aggregate by real capacity instead of averaging
        /// the percentages of differently sized packs.
        pub charge: Option<(f64, f64)>,
    }

    /// Every battery under `root`, folded into one reading.
    ///
    /// `root` is a parameter rather than the [`ROOT`] constant so the tests can
    /// point the real walk at a fixture tree. A missing or unreadable root — a
    /// container, a kernel built without the `power_supply` class — is simply
    /// "no battery".
    pub fn read_from_root(root: &Path) -> Option<Battery> {
        let mut dirs: Vec<_> = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        // `readdir` order is arbitrary; sorting makes `BAT0` decide before
        // `BAT1` so a two-pack host reports the same state on every scan.
        dirs.sort();
        let supplies: Vec<Supply> = dirs.iter().filter_map(|dir| read_supply(dir)).collect();
        aggregate(&supplies)
    }

    /// Read one `/sys/class/power_supply/<name>` directory, or `None` when it is
    /// not a system battery, or reports nothing we can turn into a percentage.
    ///
    /// The `scope` check is load-bearing: a wireless mouse or keyboard registers
    /// here as `type=Battery` with `scope=Device`, and announcing a mouse at 40%
    /// as "the machine's battery" is worse than showing nothing at all.
    pub fn read_supply(dir: &Path) -> Option<Supply> {
        if read_attr(dir, "type")? != "Battery" {
            return None; // AC adapter (`Mains`), USB PD source, ..
        }
        if read_attr(dir, "scope").as_deref() == Some("Device") {
            return None; // a peripheral's battery, not the machine's
        }
        // `energy_*` (µWh) is what a laptop with a smart battery exports; the
        // older `charge_*` (µAh) pair is the fallback. The ratio is
        // dimensionless either way, so the units never need converting.
        let charge = read_pair(dir, "energy_now", "energy_full")
            .or_else(|| read_pair(dir, "charge_now", "charge_full"));
        // `capacity` is the kernel's own already-computed percentage and is
        // preferred: on some packs it is smoothed or vendor-corrected, whereas
        // the raw pair is not.
        let percent = read_number(dir, "capacity")
            .or_else(|| charge.and_then(|(now, full)| ratio_percent(now, full)))?;
        Some(Supply {
            percent: checked_percent(percent)?,
            state: parse_state(read_attr(dir, "status").as_deref().unwrap_or_default()),
            charge,
        })
    }

    /// Fold every battery on the host into the one reading the UI shows.
    ///
    /// Percentage: one pack keeps its own figure, which already honours the
    /// `capacity`-first preference [`read_supply`] applied. Past that, when
    /// *every* pack reports a `(now, full)` pair the totals are summed — the
    /// only correct answer for unequal packs, since a 90% 20 Wh pack beside a
    /// 10% 80 Wh pack is 26%, not 50%. Otherwise the percentages are averaged,
    /// which is all the data allows.
    ///
    /// State: charging wins, because any pack drawing power means the machine is
    /// charging; then discharging, so a full pack beside a draining one does not
    /// read as "Full". With no signal either way the first pack decides (the
    /// walk sorts by name, so that is `BAT0`).
    pub fn aggregate(supplies: &[Supply]) -> Option<Battery> {
        let first = supplies.first()?;
        let percent = match supplies {
            [only] => only.percent,
            many => summed_percent(many).unwrap_or_else(|| average_percent(many)),
        };
        let state = if supplies.iter().any(|s| s.state == State::Charging) {
            State::Charging
        } else if supplies.iter().any(|s| s.state == State::Discharging) {
            State::Discharging
        } else {
            first.state
        };
        Some(Battery {
            percent: checked_percent(percent)?,
            state,
        })
    }

    /// Map the kernel's `status` attribute onto a [`State`].
    ///
    /// These four spellings plus `Unknown` are the complete
    /// `POWER_SUPPLY_STATUS_*` set; anything else — including a missing file,
    /// which arrives here as `""` — is [`State::Unknown`] rather than a guess.
    pub fn parse_state(status: &str) -> State {
        match status {
            "Charging" => State::Charging,
            "Discharging" => State::Discharging,
            "Full" => State::Full,
            "Not charging" => State::NotCharging,
            _ => State::Unknown,
        }
    }

    /// Percentage of the summed capacity, or `None` when any pack is missing its
    /// `(now, full)` pair — mixing a real capacity with a bare percentage would
    /// weight the packs wrong, so the caller falls back to averaging instead.
    fn summed_percent(supplies: &[Supply]) -> Option<f64> {
        let mut now = 0.0;
        let mut full = 0.0;
        for supply in supplies {
            let (pack_now, pack_full) = supply.charge?;
            now += pack_now;
            full += pack_full;
        }
        ratio_percent(now, full)
    }

    /// Mean of the per-pack percentages. Never called with an empty slice —
    /// [`aggregate`] returns early — so the division is safe.
    fn average_percent(supplies: &[Supply]) -> f64 {
        supplies.iter().map(|s| s.percent).sum::<f64>() / supplies.len() as f64
    }

    /// `now / full` as a percentage, or `None` when `full` cannot scale it.
    fn ratio_percent(now: f64, full: f64) -> Option<f64> {
        (full > 0.0).then(|| 100.0 * now / full)
    }

    /// `(now, full)` when both files parse and `full` is positive — a zero or
    /// absent `full` makes the ratio meaningless.
    fn read_pair(dir: &Path, now: &str, full: &str) -> Option<(f64, f64)> {
        let now = read_number(dir, now)?;
        let full = read_number(dir, full)?;
        (full > 0.0).then_some((now, full))
    }

    /// Trimmed contents of `<dir>/<name>`, or `None` when the file is missing,
    /// unreadable, or blank. Sysfs attributes always carry a trailing newline,
    /// so the trim is not optional.
    fn read_attr(dir: &Path, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// `<dir>/<name>` parsed as a *finite* number. `nan` and `inf` both parse
    /// happily as `f64`, and one poisoned pack would otherwise NaN out the sum
    /// for every other pack on the host — so they are rejected at the source.
    fn read_number(dir: &Path, name: &str) -> Option<f64> {
        read_attr(dir, name)?
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
    }
}

// ---- macOS: `pmset -g batt` -------------------------------------------------

/// macOS battery backend: the text `pmset -g batt` prints.
///
/// A text parse beats IOKit here. The IOKit route costs a CoreFoundation FFI
/// surface — and the `unsafe` that comes with it — to fetch a value we refresh
/// every few seconds, while `pmset`'s output shape has been stable for over a
/// decade and its parser is testable on any host, including this Linux CI box.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod pmset {
    use super::{checked_percent, Battery, State};

    /// Ask `pmset` for the current reading, or `None` on a Mac with no battery.
    ///
    /// The *no battery* answer is latched in a `OnceLock`: a Mac mini will never
    /// grow a battery mid-session, and without the latch every refresh tick
    /// would fork a `pmset` child just to be told the same thing again. A
    /// present battery is deliberately *not* cached — the percentage is the
    /// whole point of asking.
    ///
    /// Only a command that actually ran can latch. A spawn failure (fork
    /// refused under memory pressure) stays retryable.
    #[cfg(target_os = "macos")]
    pub fn probe() -> Option<Battery> {
        use std::sync::OnceLock;

        static NO_BATTERY: OnceLock<()> = OnceLock::new();
        if NO_BATTERY.get().is_some() {
            return None;
        }
        let output = std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .ok()?;
        let reading = parse(&String::from_utf8_lossy(&output.stdout));
        if reading.is_none() {
            let _ = NO_BATTERY.set(());
        }
        reading
    }

    /// Pull the first present internal battery out of `pmset -g batt` stdout.
    ///
    /// Real output on a laptop is two lines (`<TAB>` stands in for the literal
    /// tab `pmset` prints, which a doc comment cannot carry):
    ///
    /// ```text
    /// Now drawing from 'AC Power'
    ///  -InternalBattery-0 (id=12345)<TAB>97%; charging; 1:23 remaining present: true
    /// ```
    ///
    /// A desktop Mac prints only the `Now drawing from` line — no
    /// `-InternalBattery` entry at all — which is exactly the `None` we want.
    pub fn parse(stdout: &str) -> Option<Battery> {
        stdout
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("-InternalBattery"))
            .find_map(parse_line)
    }

    /// Parse one `-InternalBattery-N` line.
    ///
    /// The line is `;`-separated: `<id> <percent>%`, then ` <state>`, then
    /// ` <time> remaining present: <bool>`. A pack the SMC reports as absent
    /// (`present: false`) is skipped, and so is a line carrying no percentage —
    /// there would be no number to render.
    fn parse_line(line: &str) -> Option<Battery> {
        if line.contains("present: false") {
            return None;
        }
        let mut fields = line.split(';');
        // The id field is `-InternalBattery-0 (id=12345)\t97%`, so the percent
        // is the one whitespace-delimited token ending in `%`.
        let percent = fields
            .next()?
            .split_whitespace()
            .find_map(|token| token.strip_suffix('%')?.parse::<f64>().ok())?;
        Some(Battery {
            percent: checked_percent(percent)?,
            state: parse_state(fields.next().unwrap_or_default()),
        })
    }

    /// Map `pmset`'s state word onto a [`State`]. Matching is case-insensitive
    /// because the same field is printed as `AC attached` but as `charging`.
    fn parse_state(field: &str) -> State {
        match field.trim().to_ascii_lowercase().as_str() {
            "charging" => State::Charging,
            "discharging" => State::Discharging,
            // `finishing charge` is the trickle at the top of a charge cycle;
            // the UI shows it as full rather than inventing a sixth state.
            "charged" | "finishing charge" => State::Full,
            // On AC with the charge deliberately held back (battery health
            // management): plugged in, but not moving.
            "ac attached" => State::NotCharging,
            _ => State::Unknown,
        }
    }
}

// ---- Windows: GetSystemPowerStatus ------------------------------------------

/// Windows battery backend: `kernel32!GetSystemPowerStatus`.
///
/// Hand-declared rather than pulled from `windows-sys`: one struct and one
/// function is less surface than another feature flag, and keeping the binding
/// local lets [`map`](power_status::map) — the half with the actual logic — take
/// plain `u8`s, so it is unit-tested on Linux and macOS too.
#[cfg_attr(not(windows), allow(dead_code))]
mod power_status {
    use super::{checked_percent, Battery, State};

    /// `BatteryFlag` bit meaning the machine has no battery at all.
    const NO_SYSTEM_BATTERY: u8 = 128;
    /// `BatteryFlag` sentinel for "status unknown".
    const FLAG_UNKNOWN: u8 = 255;
    /// `BatteryLifePercent` sentinel for "percentage unknown".
    const PERCENT_UNKNOWN: u8 = 255;
    /// `ACLineStatus`: running on battery.
    const AC_OFFLINE: u8 = 0;
    /// `ACLineStatus`: plugged in.
    const AC_ONLINE: u8 = 1;

    /// Turn the three `SYSTEM_POWER_STATUS` fields we care about into a reading.
    ///
    /// Order matters: `255` (unknown) has the `128` no-battery bit set, so the
    /// unknown sentinel must be ruled out *before* the bit test. Testing the bit
    /// first would report every laptop that momentarily cannot read its battery
    /// as a desktop, and the metric would blink out.
    pub fn map(ac_line: u8, battery_flag: u8, life_percent: u8) -> Option<Battery> {
        let flag_unknown = battery_flag == FLAG_UNKNOWN;
        if !flag_unknown && battery_flag & NO_SYSTEM_BATTERY != 0 {
            return None; // desktop or server — hide the metric
        }
        if life_percent == PERCENT_UNKNOWN {
            return None; // no number to render, so render nothing
        }
        let percent = checked_percent(f64::from(life_percent))?;
        let state = match ac_line {
            // The percentage can still be trustworthy when the flag is not, so
            // this reports the number with an unknown state rather than hiding.
            _ if flag_unknown => State::Unknown,
            AC_OFFLINE => State::Discharging,
            // Win32 has no distinct "charged" line status, so a full pack on AC
            // is Full and anything below it is charging.
            AC_ONLINE if percent >= 100.0 => State::Full,
            AC_ONLINE => State::Charging,
            _ => State::Unknown, // 255 = unknown, and any value Win32 adds later
        };
        Some(Battery { percent, state })
    }

    /// The `SYSTEM_POWER_STATUS` layout from `winbase.h`. Field names keep their
    /// Win32 spelling so the struct can be checked against the docs at a glance,
    /// and all six fields are declared even though three are unread — the layout
    /// has to match what `kernel32` writes.
    #[cfg(windows)]
    #[repr(C)]
    #[allow(non_snake_case, dead_code)]
    struct SystemPowerStatus {
        ACLineStatus: u8,
        BatteryFlag: u8,
        BatteryLifePercent: u8,
        SystemStatusFlag: u8,
        BatteryLifeTime: u32,
        BatteryFullLifeTime: u32,
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    #[allow(non_snake_case)]
    extern "system" {
        /// Fills `status` with the machine's power state; returns 0 on failure.
        fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }

    /// Read the machine's power status, or `None` when the call fails or the
    /// host has no battery.
    #[cfg(windows)]
    pub fn probe() -> Option<Battery> {
        // SAFETY: `SystemPowerStatus` is six integers, so an all-zero bit
        // pattern is a valid value of it.
        let mut status: SystemPowerStatus = unsafe { std::mem::zeroed() };
        // SAFETY: `status` is a live, correctly sized, caller-owned
        // SYSTEM_POWER_STATUS; the call only writes into it.
        if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
            return None; // the call failed — hide rather than guess
        }
        map(
            status.ACLineStatus,
            status.BatteryFlag,
            status.BatteryLifePercent,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // ---- shared: the percentage gate ----------------------------------------

    #[test]
    fn checked_percent_clamps_the_range_and_rejects_non_numbers() {
        assert_eq!(checked_percent(42.5), Some(42.5));
        assert_eq!(checked_percent(0.0), Some(0.0));
        assert_eq!(checked_percent(100.0), Some(100.0));
        // A miscalibrated pack over-reports; a dying one under-reports.
        assert_eq!(checked_percent(137.0), Some(100.0));
        assert_eq!(checked_percent(-4.0), Some(0.0));
        // NaN/inf would render as `NaN%` — nothing to show.
        assert_eq!(checked_percent(f64::NAN), None);
        assert_eq!(checked_percent(f64::INFINITY), None);
        assert_eq!(checked_percent(f64::NEG_INFINITY), None);
    }

    // ---- Linux: pure status + aggregation -----------------------------------

    #[test]
    fn sysfs_state_maps_every_kernel_status() {
        assert_eq!(sysfs::parse_state("Charging"), State::Charging);
        assert_eq!(sysfs::parse_state("Discharging"), State::Discharging);
        assert_eq!(sysfs::parse_state("Full"), State::Full);
        assert_eq!(sysfs::parse_state("Not charging"), State::NotCharging);
        assert_eq!(sysfs::parse_state("Unknown"), State::Unknown);
        // Missing file (empty string), odd casing, and vendor junk all decline
        // to guess rather than pick a plausible-looking state.
        assert_eq!(sysfs::parse_state(""), State::Unknown);
        assert_eq!(sysfs::parse_state("charging"), State::Unknown);
        assert_eq!(sysfs::parse_state("Whatever"), State::Unknown);
    }

    #[test]
    fn sysfs_aggregate_of_nothing_is_no_battery() {
        assert_eq!(sysfs::aggregate(&[]), None);
    }

    #[test]
    fn sysfs_aggregate_passes_a_single_pack_through() {
        let one = supply(63.0, State::Discharging, Some((6300.0, 10000.0)));
        assert_eq!(
            sysfs::aggregate(&[one]),
            Some(Battery {
                percent: 63.0,
                state: State::Discharging,
            })
        );
    }

    #[test]
    fn sysfs_aggregate_sums_energy_across_unequal_packs() {
        // 90% of 20 Wh + 10% of 80 Wh = 26 Wh of 100 Wh. Averaging the
        // percentages would claim 50%, which is the bug this guards.
        let packs = [
            supply(90.0, State::Discharging, Some((18000.0, 20000.0))),
            supply(10.0, State::Discharging, Some((8000.0, 80000.0))),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().percent, 26.0);
    }

    #[test]
    fn sysfs_aggregate_averages_capacity_when_any_pack_lacks_energy() {
        // The second pack exports only `capacity`, so the summed-energy path is
        // off the table for the whole host.
        let packs = [
            supply(80.0, State::Full, Some((8000.0, 10000.0))),
            supply(20.0, State::Full, None),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().percent, 50.0);
    }

    #[test]
    fn sysfs_aggregate_lets_charging_win() {
        let packs = [
            supply(50.0, State::Discharging, None),
            supply(50.0, State::Charging, None),
            supply(50.0, State::Full, None),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().state, State::Charging);
    }

    #[test]
    fn sysfs_aggregate_prefers_discharging_over_a_full_pack() {
        // A topped-up pack beside a draining one must not read as "Full".
        let packs = [
            supply(100.0, State::Full, None),
            supply(30.0, State::Discharging, None),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().state, State::Discharging);
    }

    #[test]
    fn sysfs_aggregate_falls_back_to_the_first_pack_state() {
        let packs = [
            supply(70.0, State::NotCharging, None),
            supply(70.0, State::Unknown, None),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().state, State::NotCharging);
    }

    #[test]
    fn sysfs_aggregate_keeps_a_single_packs_own_percentage() {
        // The pack's `capacity` (42%) disagrees with its raw pair (10%);
        // `read_supply` already picked the kernel's figure, so summing the pair
        // here would quietly undo that choice.
        let one = supply(42.0, State::Full, Some((10.0, 100.0)));
        assert_eq!(sysfs::aggregate(&[one]).unwrap().percent, 42.0);
    }

    #[test]
    fn sysfs_aggregate_survives_a_zero_capacity_pair() {
        // `read_supply` never builds this, but a bad kernel driver would make
        // the sum unusable — fall back to the averaged percentages, not NaN.
        let packs = [
            supply(44.0, State::Full, Some((0.0, 0.0))),
            supply(46.0, State::Full, Some((0.0, 0.0))),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().percent, 45.0);
    }

    #[test]
    fn sysfs_aggregate_clamps_an_over_full_pack() {
        // Post-calibration packs report `energy_now > energy_full`.
        let packs = [
            supply(100.0, State::Full, Some((11000.0, 10000.0))),
            supply(100.0, State::Full, Some((10500.0, 10000.0))),
        ];
        assert_eq!(sysfs::aggregate(&packs).unwrap().percent, 100.0);
    }

    // ---- Linux: the real directory walk over a fixture tree -----------------

    #[test]
    fn sysfs_walk_reads_capacity_and_status_from_a_real_tree() {
        let root = scratch("walk");
        supply_dir(
            &root,
            "BAT0",
            &[("type", "Battery"), ("capacity", "77"), ("status", "Full")],
        );

        assert_eq!(
            sysfs::read_from_root(&root),
            Some(Battery {
                percent: 77.0,
                state: State::Full,
            })
        );
        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_prefers_capacity_then_energy_then_charge() {
        let root = scratch("percent-sources");

        // `capacity` wins even when the energy pair disagrees with it.
        let by_capacity = root.join("by-capacity");
        supply_dir(
            &by_capacity,
            "BAT0",
            &[
                ("type", "Battery"),
                ("capacity", "42"),
                ("energy_now", "10"),
                ("energy_full", "100"),
            ],
        );
        assert_eq!(sysfs::read_from_root(&by_capacity).unwrap().percent, 42.0);

        // No `capacity`: the energy pair beats the charge pair.
        let by_energy = root.join("by-energy");
        supply_dir(
            &by_energy,
            "BAT0",
            &[
                ("type", "Battery"),
                ("energy_now", "3000"),
                ("energy_full", "12000"),
                ("charge_now", "1"),
                ("charge_full", "2"),
            ],
        );
        assert_eq!(sysfs::read_from_root(&by_energy).unwrap().percent, 25.0);

        // Neither: the older `charge_*` pair is the last resort.
        let by_charge = root.join("by-charge");
        supply_dir(
            &by_charge,
            "BAT0",
            &[
                ("type", "Battery"),
                ("charge_now", "1500"),
                ("charge_full", "2000"),
            ],
        );
        assert_eq!(sysfs::read_from_root(&by_charge).unwrap().percent, 75.0);

        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_skips_mains_and_peripheral_batteries() {
        let root = scratch("filter");
        // An AC adapter, a Logitech mouse, and the machine's own pack.
        supply_dir(&root, "AC", &[("type", "Mains"), ("online", "1")]);
        supply_dir(
            &root,
            "hidpp_battery_0",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("capacity", "40"),
                ("status", "Discharging"),
            ],
        );
        supply_dir(
            &root,
            "BAT0",
            &[
                ("type", "Battery"),
                ("capacity", "88"),
                ("status", "Charging"),
            ],
        );

        assert_eq!(
            sysfs::read_from_root(&root),
            Some(Battery {
                percent: 88.0,
                state: State::Charging,
            }),
            "only BAT0 counts — the adapter and the mouse are not the machine",
        );

        // A desktop with a wireless mouse plugged in has NO machine battery:
        // reporting the mouse here is the exact failure the scope filter exists
        // to prevent.
        let peripherals_only = scratch("filter-mouse-only");
        supply_dir(&peripherals_only, "AC", &[("type", "Mains")]);
        supply_dir(
            &peripherals_only,
            "hidpp_battery_0",
            &[("type", "Battery"), ("scope", "Device"), ("capacity", "40")],
        );
        assert_eq!(sysfs::read_from_root(&peripherals_only), None);

        cleanup(&root);
        cleanup(&peripherals_only);
    }

    #[test]
    fn sysfs_walk_sums_two_packs_from_a_real_tree() {
        let root = scratch("multi");
        supply_dir(
            &root,
            "BAT0",
            &[
                ("type", "Battery"),
                ("energy_now", "18000"),
                ("energy_full", "20000"),
                ("status", "Discharging"),
            ],
        );
        supply_dir(
            &root,
            "BAT1",
            &[
                ("type", "Battery"),
                ("energy_now", "8000"),
                ("energy_full", "80000"),
                ("status", "Charging"),
            ],
        );

        assert_eq!(
            sysfs::read_from_root(&root),
            Some(Battery {
                percent: 26.0, // 26000 µWh of 100000 µWh
                state: State::Charging,
            })
        );
        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_skips_malformed_empty_and_missing_files() {
        let root = scratch("malformed");
        // Unparseable capacity with no pair to fall back on.
        supply_dir(&root, "BAT0", &[("type", "Battery"), ("capacity", "n/a")]);
        // Empty `type` file.
        supply_dir(&root, "BAT1", &[("type", ""), ("capacity", "50")]);
        // No `type` file at all.
        supply_dir(&root, "BAT2", &[("capacity", "50")]);
        // A zeroed `energy_full` would be a division by zero.
        supply_dir(
            &root,
            "BAT3",
            &[
                ("type", "Battery"),
                ("energy_now", "10"),
                ("energy_full", "0"),
            ],
        );
        // `nan` parses as an f64 but is not a reading.
        supply_dir(&root, "BAT4", &[("type", "Battery"), ("capacity", "nan")]);
        // A stray plain file where a directory is expected.
        std::fs::write(root.join("stray"), "junk").expect("fixture stray file");

        assert_eq!(sysfs::read_from_root(&root), None);
        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_clamps_both_ends() {
        let root = scratch("clamp");

        let over = root.join("over");
        supply_dir(
            &over,
            "BAT0",
            &[
                ("type", "Battery"),
                ("energy_now", "11000"),
                ("energy_full", "10000"),
            ],
        );
        assert_eq!(sysfs::read_from_root(&over).unwrap().percent, 100.0);

        let under = root.join("under");
        supply_dir(&under, "BAT0", &[("type", "Battery"), ("capacity", "-5")]);
        assert_eq!(sysfs::read_from_root(&under).unwrap().percent, 0.0);

        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_of_a_missing_root_is_no_battery() {
        let root = scratch("absent");
        let missing = root.join("no-such-power-supply-tree");
        assert_eq!(sysfs::read_from_root(&missing), None);
        cleanup(&root);
    }

    #[test]
    fn sysfs_walk_of_an_empty_root_is_no_battery() {
        // A VM registers the class directory but nothing inside it.
        let root = scratch("empty-root");
        assert_eq!(sysfs::read_from_root(&root), None);
        cleanup(&root);
    }

    // ---- the host itself ----------------------------------------------------

    #[test]
    fn read_hides_the_metric_on_a_battery_less_host() {
        let reading = read();

        #[cfg(target_os = "linux")]
        {
            // This build box and our CI runners are VMs: `/sys/class/power_supply`
            // is either absent or empty, so the whole hide path — walk, filter,
            // aggregate, `None` — runs for real right here. A developer running
            // the suite on a laptop has entries in the tree and legitimately
            // gets a reading, so assert whatever the hardware justifies.
            let root = std::path::Path::new(sysfs::ROOT);
            let no_supplies = std::fs::read_dir(root)
                .map(|entries| entries.count() == 0)
                .unwrap_or(true);
            if no_supplies {
                assert_eq!(reading, None, "no power supplies means no battery");
            }
        }

        // Everywhere else all we can assert is that the probe is total: it
        // either declines or returns a renderable number.
        if let Some(battery) = reading {
            assert!((0.0..=100.0).contains(&battery.percent));
        }
    }

    // ---- macOS: the pmset parser --------------------------------------------

    /// The `Now drawing from` header every `pmset -g batt` run prints first.
    const PMSET_HEADER: &str = "Now drawing from 'AC Power'\n";

    #[test]
    fn pmset_parses_a_charging_laptop() {
        let out = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t97%; charging; 1:23 remaining present: true\n"
        );
        assert_eq!(
            pmset::parse(&out),
            Some(Battery {
                percent: 97.0,
                state: State::Charging,
            })
        );
    }

    #[test]
    fn pmset_parses_a_discharging_laptop() {
        let out = "Now drawing from 'Battery Power'\n \
                   -InternalBattery-0 (id=12345)\t41%; discharging; 2:05 remaining present: true\n";
        assert_eq!(
            pmset::parse(out),
            Some(Battery {
                percent: 41.0,
                state: State::Discharging,
            })
        );
    }

    #[test]
    fn pmset_charged_and_finishing_charge_are_both_full() {
        let charged = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t100%; charged; 0:00 remaining present: true\n"
        );
        assert_eq!(
            pmset::parse(&charged),
            Some(Battery {
                percent: 100.0,
                state: State::Full,
            })
        );

        let finishing = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t99%; finishing charge; 0:04 remaining present: true\n"
        );
        assert_eq!(
            pmset::parse(&finishing),
            Some(Battery {
                percent: 99.0,
                state: State::Full,
            })
        );
    }

    #[test]
    fn pmset_ac_attached_is_not_charging() {
        // Battery health management holds the charge: plugged in, not moving.
        // Note the capital `AC` — the match has to be case-insensitive.
        let out = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t80%; AC attached; not charging present: true\n"
        );
        assert_eq!(
            pmset::parse(&out),
            Some(Battery {
                percent: 80.0,
                state: State::NotCharging,
            })
        );
    }

    #[test]
    fn pmset_unknown_state_word_still_reports_the_percentage() {
        let out = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t55%; hibernating; 0:00 remaining present: true\n"
        );
        assert_eq!(
            pmset::parse(&out),
            Some(Battery {
                percent: 55.0,
                state: State::Unknown,
            })
        );
    }

    #[test]
    fn pmset_desktop_output_has_no_battery() {
        // A Mac mini or Studio prints the header and nothing else.
        assert_eq!(pmset::parse(PMSET_HEADER), None);
    }

    #[test]
    fn pmset_absent_pack_is_no_battery() {
        let out = format!(
            "{PMSET_HEADER} -InternalBattery-0 (id=12345)\t0%; discharging; 0:00 remaining present: false\n"
        );
        assert_eq!(pmset::parse(&out), None);
    }

    #[test]
    fn pmset_skips_an_absent_pack_and_takes_the_next_present_one() {
        let out = format!(
            "{PMSET_HEADER}\
             -InternalBattery-0 (id=12345)\t0%; discharging; 0:00 remaining present: false\n\
             -InternalBattery-1 (id=67890)\t64%; discharging; 3:11 remaining present: true\n"
        );
        assert_eq!(
            pmset::parse(&out),
            Some(Battery {
                percent: 64.0,
                state: State::Discharging,
            })
        );
    }

    #[test]
    fn pmset_garbage_input_is_no_battery() {
        assert_eq!(pmset::parse(""), None);
        assert_eq!(pmset::parse("\n\n\t \n"), None);
        assert_eq!(pmset::parse("command not found: pmset"), None);
        // A battery line with no percentage has nothing to render.
        assert_eq!(
            pmset::parse(" -InternalBattery-0 (id=12345)\t; charging; present: true"),
            None
        );
        // ..and neither does one whose percentage is not a number.
        assert_eq!(
            pmset::parse(" -InternalBattery-0 (id=12345)\tNaN%; charging; present: true"),
            None
        );
    }

    #[test]
    fn pmset_clamps_an_over_hundred_percent_pack() {
        let out =
            format!("{PMSET_HEADER} -InternalBattery-0 (id=12345)\t103%; charged; present: true\n");
        assert_eq!(pmset::parse(&out).unwrap().percent, 100.0);
    }

    // ---- Windows: the SYSTEM_POWER_STATUS mapper ----------------------------

    #[test]
    fn windows_no_system_battery_flag_hides_the_metric() {
        // 128 alone, and 128 OR'd with a charge level, are both "desktop".
        assert_eq!(power_status::map(1, 128, 255), None);
        assert_eq!(power_status::map(1, 128 | 1, 100), None);
        assert_eq!(power_status::map(0, 128 | 8, 50), None);
    }

    #[test]
    fn windows_unknown_flag_reports_the_percentage_with_an_unknown_state() {
        // 255 has the 128 bit set but means "status unknown", not "no battery" —
        // the sentinel must be ruled out before the bit test.
        assert_eq!(
            power_status::map(1, 255, 73),
            Some(Battery {
                percent: 73.0,
                state: State::Unknown,
            })
        );
        // ..and an unknown flag does not turn an offline AC line into a state.
        assert_eq!(power_status::map(0, 255, 73).unwrap().state, State::Unknown);
    }

    #[test]
    fn windows_unknown_percentage_hides_the_metric() {
        assert_eq!(power_status::map(0, 1, 255), None);
        assert_eq!(power_status::map(1, 8, 255), None);
    }

    #[test]
    fn windows_ac_offline_is_discharging() {
        assert_eq!(
            power_status::map(0, 4, 17),
            Some(Battery {
                percent: 17.0,
                state: State::Discharging,
            })
        );
        // Even a full pack is discharging once the cable is out.
        assert_eq!(
            power_status::map(0, 1, 100).unwrap().state,
            State::Discharging
        );
    }

    #[test]
    fn windows_ac_online_is_charging_until_it_is_full() {
        assert_eq!(
            power_status::map(1, 8, 64),
            Some(Battery {
                percent: 64.0,
                state: State::Charging,
            })
        );
        assert_eq!(
            power_status::map(1, 1, 100),
            Some(Battery {
                percent: 100.0,
                state: State::Full,
            })
        );
    }

    #[test]
    fn windows_unknown_ac_line_is_unknown() {
        assert_eq!(
            power_status::map(255, 1, 30),
            Some(Battery {
                percent: 30.0,
                state: State::Unknown,
            })
        );
    }

    // ---- fixture helpers ----------------------------------------------------

    /// Terse [`sysfs::Supply`] builder for the aggregation tests.
    fn supply(percent: f64, state: State, charge: Option<(f64, f64)>) -> sysfs::Supply {
        sysfs::Supply {
            percent,
            state,
            charge,
        }
    }

    /// Unique scratch dir under the system tmpdir, keyed by test name + pid +
    /// thread id so parallel test threads never collide (the same shape as the
    /// daemon tests' helper).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "space-usage-battery-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Write a fixture power-supply directory: `<root>/<name>/<attr>` holding
    /// `<value>` for every pair, each with the trailing newline the kernel emits
    /// so the readers' trimming is exercised for real.
    fn supply_dir(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("fixture supply dir");
        for (attr, value) in attrs {
            std::fs::write(dir.join(attr), format!("{value}\n")).expect("fixture attribute");
        }
    }

    /// Best-effort removal of a scratch tree once a test is done with it.
    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }
}
