//! Plugin config + herdr `[ui]` label loading, and env/state path resolution.
//!
//! - [`load_config`] parses `$HERDR_PLUGIN_CONFIG_DIR/config.toml` (flat
//!   `key = value` lines).
//! - [`load_herdr_labels`] reads `cpu_label` / `ram_label` / `battery_label`
//!   from herdr's OWN `[ui]` section so per-space rows match the patched
//!   sidebar header.
//! - The path helpers resolve the herdr-injected env (`HERDR_PLUGIN_*`) with the
//!   same `<tmpdir>/<id>` fallbacks the runtime uses.

use std::path::PathBuf;

use crate::battery::{self, Battery};
use crate::disk::{self, Disk};
use crate::icons::{self, IconSet};

/// Status-surfacing strategy (plugin `config.toml` `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A "usage" pseudo-agent per space in the agents panel.
    AgentsPanel,
    /// Display-only metadata rendered inside the spaces card. The default since
    /// 1.8.0 — herdr has drawn the spaces card from configurable token rows
    /// since 0.7.5, which the manifest already requires, so this needs no
    /// patched build and puts the reading on the surface people look at.
    Sidebar,
}

impl Mode {
    /// herdr config table whose rows render this mode's `$usage` token, and the
    /// rows herdr uses when that table is absent.
    ///
    /// Both halves live here because a caller that knew one without the other
    /// would write a table header with the wrong defaults under it — silently
    /// dropping `branch` and `git_status` off every space card.
    pub fn sidebar_table(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Mode::Sidebar => (
                "ui.sidebar.spaces",
                &[
                    r#"["state_icon", "workspace"]"#,
                    r#"["branch", "git_status"]"#,
                ],
            ),
            Mode::AgentsPanel => (
                "ui.sidebar.agents",
                &[r#"["state_icon", "workspace", "tab"]"#, r#"["agent"]"#],
            ),
        }
    }
}

/// How the narrow RAM cell renders its number (plugin `config.toml`
/// `ram_display`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamDisplay {
    /// Percent of the machine's total, falling back to the compact absolute
    /// form when the total is unreadable.
    Percent,
    /// Always the compact absolute form (`513M` / `1.5G`), even when a percent
    /// could be computed — for readers who want the figure itself, not its
    /// share of whatever this machine happens to have.
    Absolute,
}

/// Plugin user config from `$HERDR_PLUGIN_CONFIG_DIR/config.toml`.
#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub interval_seconds: u64,
    pub window_title_totals: bool,
    /// Whether to show the battery cell at all. On by default; a host with no
    /// battery hides it regardless (see [`Config::battery_reading`]).
    ///
    /// The cell it gates is the machine-wide one — the window title, the
    /// report's total line, and the JSON pair. Per-space rows carry no battery
    /// on any setting; see [`crate::render::usage_row`].
    pub battery: bool,
    /// Glyph tier name as the user typed it — [`crate::icons::resolve`] is what
    /// gives it meaning, so an unknown value auto-detects instead of failing
    /// here.
    pub icons: String,
    /// How the narrow RAM cell renders: percent of the machine's total (the
    /// default), or always the compact absolute form.
    pub ram_display: RamDisplay,
    /// Naming for the battery cell, overriding herdr's `[ui] battery_label`.
    ///
    /// Battery lives here rather than only in herdr's config because herdr has
    /// no battery of its own to label: `battery_label` is not a key it knows, so
    /// putting it in herdr's `[ui]` makes `herdr server reload-config` report
    /// `unknown config key ui.battery_label; ignoring key` on every reload.
    /// Harmless but noisy, and needless — nothing outside this plugin renders a
    /// battery, so there is no second surface to keep in step.
    ///
    /// Names the battery wherever the plugin draws it: the window title, the
    /// report's total line, and the `--icons` preview.
    ///
    /// Unlike the herdr-side keys, an EMPTY value here is not read as unset —
    /// see [`parse_config`].
    pub battery_label: Option<String>,
    /// Per-key overrides for herdr's `[ui] cpu_label` / `ram_label`.
    ///
    /// Unset (the default) keeps herdr's config the single source of truth for
    /// these two, which is what keeps a patched build's system-usage header and
    /// these rows agreeing. That default is the one to leave alone unless you
    /// want the two surfaces to disagree.
    ///
    /// Setting one here is for the case herdr's config cannot express: naming
    /// what THIS plugin draws without touching a key herdr also reads. On a
    /// stock build herdr accepts `ui.cpu_label` / `ui.ram_label` but draws no
    /// system-usage header of its own, so those keys reach nothing but this
    /// plugin anyway — and some users would rather keep every plugin-only
    /// setting in the plugin's own file, next to `icons` and `battery_label`,
    /// than spread them across two configs. The trade is explicit: a plugin
    /// override names only what this plugin draws, so a header (if you later
    /// run a patched build) will not follow it.
    ///
    /// Unlike the herdr-side keys, an EMPTY value here is not read as unset —
    /// see [`parse_config`].
    pub cpu_label: Option<String>,
    pub ram_label: Option<String>,
    /// Whether to show free disk space at all. On by default, like the battery,
    /// and hidden the same way when nothing can be read.
    pub disk: bool,
    /// Which drives get a cell, in the order they are drawn.
    ///
    /// Mount points as a person writes them (`/`, `/home`, `C:`) — the reading
    /// is taken for whatever filesystem the path lands on, so any directory
    /// inside a mount names it just as well. Defaults to the root filesystem,
    /// which is the drive people mean when they say "my disk"; anything else is
    /// a deliberate choice and so has to be written down.
    pub disks: Vec<String>,
    /// Naming for the disk cells, in this file rather than herdr's for the same
    /// reason as [`Config::battery_label`]: herdr has no disk of its own to
    /// label, so `disk_label` is not a key it knows.
    pub disk_label: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Sidebar,
            interval_seconds: 5,
            window_title_totals: true,
            battery: true,
            icons: "auto".to_string(),
            ram_display: RamDisplay::Percent,
            battery_label: None,
            cpu_label: None,
            ram_label: None,
            disk: true,
            disks: default_disks(),
            disk_label: None,
        }
    }
}

/// The drive shown when the user has named none: the root filesystem.
///
/// On Windows that is whichever letter this install booted from
/// (`%SystemDrive%`, normally `C:`) rather than a hardcoded `C:` — a machine
/// that boots from `D:` would otherwise be told about a drive it may not have.
fn default_disks() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![non_empty_env("SystemDrive").unwrap_or_else(|| "C:".to_string())]
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

impl Config {
    /// The glyph tier this config selects.
    ///
    /// Resolve once per refresh and pass the result down: the answer depends on
    /// the locale environment, which cannot change while the process runs, so
    /// re-resolving per space would be pure repetition.
    pub fn icon_set(&self) -> IconSet {
        icons::resolve(Some(&self.icons))
    }

    /// The machine-wide battery reading for one refresh cycle, or `None` when
    /// this host has no battery or the user turned the metric off.
    ///
    /// The `battery = false` gate lives here, at the *read*, rather than at each
    /// place a cell is drawn. An opted-out user then pays nothing for the metric
    /// — no sysfs walk on Linux, no `pmset` child process on macOS — and every
    /// surface downstream (sidebar, window title, terminal report, JSON) is off
    /// by construction instead of by four separate checks that could drift.
    /// Turning the metric off therefore looks exactly like a desktop to the
    /// renderers, which is the honest answer: there is no reading to show.
    ///
    /// Call this ONCE per refresh and pass the `Option<Battery>` down. A battery
    /// is one value for the whole machine, so calling it per space would re-walk
    /// sysfs (or fork `pmset`) once per space to be told the same thing.
    pub fn battery_reading(&self) -> Option<Battery> {
        self.battery.then(battery::read).flatten()
    }

    /// This refresh cycle's free-space readings, one per selected drive that
    /// answered, or empty when the user turned the metric off.
    ///
    /// The `disk = false` gate lives here for the same reason the battery's
    /// does: an opted-out user pays no syscall at all, and every surface
    /// downstream sees the same empty list a host with no readable drive
    /// produces. Call ONCE per refresh and pass the result down — these are
    /// machine-wide figures, so taking them per space would re-stat every drive
    /// once per space to be told the same thing.
    pub fn disk_readings(&self) -> Vec<Disk> {
        match self.disk {
            true => disk::read(&self.disks),
            false => Vec::new(),
        }
    }
}

/// Default naming for each metric when herdr's `[ui]` config sets none.
pub const DEFAULT_CPU_LABEL: &str = "cpu";
pub const DEFAULT_RAM_LABEL: &str = "ram";
pub const DEFAULT_BATTERY_LABEL: &str = "bat";
pub const DEFAULT_DISK_LABEL: &str = "disk";

/// CPU / RAM / battery label tokens sourced from herdr's `[ui]` config.
///
/// Each is `None` until herdr's config actually names it. That distinction is
/// load-bearing rather than tidiness: an explicit label *replaces* an icon
/// tier's glyph (see [`crate::icons`]), so the renderer has to know whether the
/// user chose a word or whether it is merely looking at a default. Inferring
/// that by comparing against the default string cannot tell `cpu_label = "cpu"`
/// apart from an unset key — which silently made the two behave differently for
/// no reason a user could see.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    cpu: Option<String>,
    ram: Option<String>,
    battery: Option<String>,
    /// Read from herdr's `[ui]` like cpu and ram, and overridden by the
    /// plugin's own `disk_label`.
    ///
    /// Stock herdr draws no disk and so knows no such key — setting it there is
    /// harmless but earns an `unknown config key` line per reload. It is read
    /// anyway because a build whose sidebar header *does* draw free space (the
    /// patched build this plugin is developed against) names it with exactly
    /// this key, and then one setting keeps the header and these cells saying
    /// the same word. That is the same bargain `cpu_label` and `ram_label`
    /// already make.
    disk: Option<String>,
}

impl Labels {
    /// Build a set of labels directly, bypassing herdr's config file.
    ///
    /// `None` means "herdr named nothing for this metric", which is what lets an
    /// icon tier supply its own naming. Test-only: production always arrives
    /// here through [`parse_herdr_labels`], and an unused constructor on a
    /// public type is an invitation to construct one some other way.
    #[cfg(test)]
    pub fn new(
        cpu: Option<&str>,
        ram: Option<&str>,
        battery: Option<&str>,
        disk: Option<&str>,
    ) -> Self {
        Self {
            cpu: cpu.map(str::to_string),
            ram: ram.map(str::to_string),
            battery: battery.map(str::to_string),
            disk: disk.map(str::to_string),
        }
    }

    /// The label herdr's config set for this metric, or `None` when it set none.
    /// Feed these to the icon tier, which decides how an unnamed metric is drawn.
    pub fn cpu(&self) -> Option<&str> {
        self.cpu.as_deref()
    }

    pub fn ram(&self) -> Option<&str> {
        self.ram.as_deref()
    }

    pub fn battery(&self) -> Option<&str> {
        self.battery.as_deref()
    }

    pub fn disk(&self) -> Option<&str> {
        self.disk.as_deref()
    }

    /// Apply the plugin config's own label overrides on top of herdr's.
    ///
    /// Each key wins independently, so overriding one label does not detach the
    /// others from herdr's config — see [`Config::battery_label`] and
    /// [`Config::cpu_label`] for why each override exists. Returning `Self`
    /// keeps the load-then-override pair a single expression at each call site,
    /// so no caller can load the labels and forget the overrides.
    pub fn with_overrides(mut self, config: &Config) -> Self {
        if let Some(label) = &config.cpu_label {
            self.cpu = Some(label.clone());
        }
        if let Some(label) = &config.ram_label {
            self.ram = Some(label.clone());
        }
        if let Some(label) = &config.battery_label {
            self.battery = Some(label.clone());
        }
        if let Some(label) = &config.disk_label {
            self.disk = Some(label.clone());
        }
        self
    }

    /// The word to print on surfaces that always spell one out regardless of
    /// tier — the full-width terminal report's columns and its total line.
    ///
    /// An empty label (the plugin config's "name nothing") falls back to the
    /// default word here: the report's columns need a word to head them, and a
    /// blank one would leave a bare figure floating in a wide table.
    pub fn cpu_word(&self) -> &str {
        match self.cpu() {
            Some(word) if !word.is_empty() => word,
            _ => DEFAULT_CPU_LABEL,
        }
    }

    pub fn ram_word(&self) -> &str {
        match self.ram() {
            Some(word) if !word.is_empty() => word,
            _ => DEFAULT_RAM_LABEL,
        }
    }
}

/// Default plugin id when herdr does not inject `HERDR_PLUGIN_ID`.
const DEFAULT_PLUGIN_ID: &str = "ez-corp.space-usage";

/// Upper bound on `interval_seconds` (8 h).
///
/// The daemon gives every status a TTL of three intervals, and herdr rejects a
/// `ttl_ms` above 24 h — so past this the sidebar would silently stay blank
/// because every report was refused. Clamping here rather than in the TTL is
/// deliberate: capping only the TTL would let it fall *below* the refresh
/// interval, so statuses would blink out between pushes. It also keeps the
/// millisecond arithmetic well clear of overflow.
///
/// [`crate::daemon`] ties this to herdr's ceiling with a compile-time assert.
pub(crate) const MAX_INTERVAL_SECONDS: u64 = 28_800;

/// Load the plugin's own `config.toml`, returning defaults if it is absent.
pub fn load_config() -> Config {
    match std::fs::read_to_string(config_dir().join("config.toml")) {
        Ok(text) => parse_config(&text),
        Err(_) => Config::default(), // no config file — defaults
    }
}

/// Load `cpu_label` / `ram_label` / `battery_label` from herdr's `[ui]` config
/// section.
pub fn load_herdr_labels() -> Labels {
    match std::fs::read_to_string(herdr_config_path()) {
        Ok(text) => parse_herdr_labels(&text),
        Err(_) => Labels::default(), // no herdr config readable — defaults
    }
}

/// Plugin id (`HERDR_PLUGIN_ID`, else `ez-corp.space-usage`).
pub fn plugin_id() -> String {
    non_empty_env("HERDR_PLUGIN_ID").unwrap_or_else(|| DEFAULT_PLUGIN_ID.to_string())
}

/// Durable state dir (`HERDR_PLUGIN_STATE_DIR`, else `<tmpdir>/<id>`).
pub fn state_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(plugin_id()))
}

/// User config dir (`HERDR_PLUGIN_CONFIG_DIR`, else `<tmpdir>/<id>-config`).
pub fn config_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("{}-config", plugin_id())))
}

/// Updater single-instance pid file for the herdr session this process talks to
/// (`<state_dir>/updater-<session>.pid`).
///
/// Keyed by session because the state dir is not: herdr gives each user one
/// `HERDR_PLUGIN_STATE_DIR` per plugin and every session shares it, while each
/// session runs its own server on its own socket. A single global pid file
/// therefore made the updater one-per-*machine* rather than one-per-session —
/// the second session's `--restore` found the first session's live daemon,
/// stood down, and left its sidebar blank, since a daemon can only push to the
/// one socket it is connected to. See [`crate::herdr::session_key`].
pub fn pid_file() -> PathBuf {
    state_dir().join(pid_file_name(&crate::herdr::session_key()))
}

/// The pid file name one session key claims.
pub(crate) fn pid_file_name(session_key: &str) -> String {
    format!("updater-{session_key}.pid")
}

/// Pid file written by versions before 1.11.1, when the updater was
/// one-per-machine.
///
/// Still swept by `--disable` so an upgrade cannot strand the daemon that was
/// running at the time. Deliberately NOT honoured as a single-instance claim:
/// it says nothing about *which* session's daemon holds it, and treating it as
/// this session's would put every other session back where it started until
/// that daemon happened to retire.
const LEGACY_PID_FILE: &str = "updater.pid";

/// Every session's updater pid file under the state dir, the legacy one
/// included, sorted so callers behave the same run to run.
///
/// `--disable` is one decision for the whole machine — it writes the shared
/// marker and takes our row out of the one config every session renders — so it
/// has to reach the daemons other sessions started, not just this session's.
pub fn pid_files() -> Vec<PathBuf> {
    pid_files_in(&state_dir())
}

/// [`pid_files`] against an explicit dir, so a test can exercise it without the
/// state dir the env decides. A dir we cannot read yields none, which is the
/// same answer a dir with no updater in it gives.
fn pid_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_pid_file(path))
        .collect();
    files.sort();
    files
}

/// Whether `path` names an updater pid file — this scheme's or the legacy one.
fn is_pid_file(path: &std::path::Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => {
            name == LEGACY_PID_FILE || (name.starts_with("updater-") && name.ends_with(".pid"))
        }
        None => false,
    }
}

/// Marker recording what the user has *decided* about the updater
/// (`<state_dir>/enabled`).
///
/// The pid file says whether a daemon is live *right now*; this says what the
/// user wants. `--restore` (the manifest `[[startup]]` and `[[events]]` hooks)
/// reads it so the updater comes back after a herdr or machine restart.
///
/// Three states, not two — see [`Wanted`]. The absent case is the fresh install,
/// and it means "wanted", which is what makes the plugin work out of the box.
pub fn enabled_flag() -> PathBuf {
    state_dir().join("enabled")
}

/// Marker recording that we have run first-time setup (`<state_dir>/bootstrapped`).
///
/// Separate from [`enabled_flag`] because the two answer different questions and
/// are written at different times: "does the user want the updater" versus "have
/// we already offered to edit herdr's config". Folding them together would make
/// a later `status-enable` re-add a `$usage` row the user had deliberately taken
/// out of their own config.
pub fn bootstrapped_flag() -> PathBuf {
    state_dir().join("bootstrapped")
}

/// What the user has decided about the updater.
///
/// The old marker was a plain present/absent boolean, which conflated "never
/// asked for" with "asked to be off" — so a fresh install, which has written
/// nothing, looked identical to a deliberate `status-disable` and stayed dark
/// until someone found `status-enable` by hand. That was the bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Nothing written yet: a fresh install. Treated as wanted, and first-run
    /// setup still has to happen.
    Undecided,
    /// `status-enable` was run.
    Enabled,
    /// `status-disable` was run. The one state that keeps the updater down
    /// across restarts.
    Disabled,
}

impl Wanted {
    /// Whether the updater should be running. Only an explicit `Disabled` says no
    /// — "never decided" defaults to on, which is what makes a fresh install
    /// render without a manual step.
    pub fn wants_daemon(self) -> bool {
        self != Wanted::Disabled
    }
}

/// Read the decision marker at `path`.
///
/// Absent file → [`Wanted::Undecided`]. A `0` (the marker `--disable` writes) →
/// [`Wanted::Disabled`]. Anything else, including the bare `1` older versions
/// wrote, → [`Wanted::Enabled`]: an unreadable or garbled marker resolves to the
/// state the user is more likely to want, and one that self-heals on the next
/// enable/disable.
pub fn read_wanted(path: &std::path::Path) -> Wanted {
    match std::fs::read_to_string(path) {
        Err(_) => Wanted::Undecided,
        Ok(text) if text.trim() == "0" => Wanted::Disabled,
        Ok(_) => Wanted::Enabled,
    }
}

// ---- env / path resolution --------------------------------------------------

/// Read `name` from the environment, treating unset AND empty as absent — herdr
/// injects an empty string for a value it has no answer for.
pub(crate) fn non_empty_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// User home directory from `$HOME`, or an empty path when unset.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Config base: `%APPDATA%` on Windows (where the herdr beta keeps its config
/// and socket), else `$XDG_CONFIG_HOME` if set (and non-empty), else
/// `~/.config`.
pub(crate) fn config_home() -> PathBuf {
    #[cfg(windows)]
    if let Some(appdata) = non_empty_env("APPDATA") {
        return PathBuf::from(appdata);
    }
    non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// Path to herdr's OWN `config.toml` (`<config_home>/herdr/config.toml`).
pub(crate) fn herdr_config_path() -> PathBuf {
    config_home().join("herdr").join("config.toml")
}

// ---- pure parsers (hand-rolled, no `toml` crate) ----------------------------

/// Parse the plugin's flat `config.toml` text into a [`Config`], starting from
/// the documented defaults.
///
/// Recognised keys: `mode` (`agents-panel` | `sidebar`), `interval_seconds`
/// (numeric `>= 1`), `window_title_totals`, `battery` and `disk` (`false` only
/// when they equal the literal `false`, any other value is truthy), `disks` (a
/// comma-separated drive list), `icons` (a tier name kept verbatim for
/// [`crate::icons::resolve`]), `ram_display` (`percent` | `gb` | `absolute`,
/// case-insensitive), and the four label overrides — `cpu_label`, `ram_label`,
/// `battery_label`, `disk_label` — where an empty value means "name nothing"
/// rather than unset. Unknown keys are ignored.
fn parse_config(text: &str) -> Config {
    let mut cfg = Config::default();
    for line in text.split('\n') {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = parse_kv_line(line) else {
            continue;
        };
        match key {
            "mode" if value == "sidebar" => cfg.mode = Mode::Sidebar,
            "mode" if value == "agents-panel" => cfg.mode = Mode::AgentsPanel,
            // Accept any numeric >= 1, clamped to `MAX_INTERVAL_SECONDS`. The
            // struct stores whole seconds, so a fractional value is truncated —
            // the daemon only ever uses this as a coarse cadence.
            "interval_seconds" => {
                if let Ok(n) = value.parse::<f64>() {
                    if n >= 1.0 {
                        // A huge float saturates rather than wrapping, so the
                        // `min` still lands on the cap.
                        cfg.interval_seconds = (n as u64).min(MAX_INTERVAL_SECONDS);
                    }
                }
            }
            "window_title_totals" => cfg.window_title_totals = value != "false",
            // `gb` is the name users reach for ("show it in GB"); `absolute` is
            // what the setting actually does, since the cell stays in MB below
            // a gigabyte. Both spellings land on the same behaviour, and case is
            // folded because `icons` in this same file already forgives it —
            // one file that accepts `Emoji` but not `GB` is a trap, not a rule.
            "ram_display" => match value.trim().to_ascii_lowercase().as_str() {
                "gb" | "absolute" => cfg.ram_display = RamDisplay::Absolute,
                "percent" => cfg.ram_display = RamDisplay::Percent,
                // Unknown spelling: keep the default rather than fail, matching
                // how `mode` and `icons` treat a value they do not recognise.
                _ => {}
            },
            // EMPTY is a deliberate "name nothing" for all three, NOT unset.
            //
            // This is the opposite of the herdr-side rule, and the difference is
            // in what ships: herdr ships these keys as blank commented templates,
            // so a blank there is usually someone uncommenting a line they have
            // not filled in yet. Nothing ships them in the plugin's own file, so
            // a blank here can only be the deliberate bare number that
            // [`crate::icons::labelled`] renders.
            "battery_label" => cfg.battery_label = Some(value.to_string()),
            "cpu_label" => cfg.cpu_label = Some(value.to_string()),
            "ram_label" => cfg.ram_label = Some(value.to_string()),
            "battery" => cfg.battery = value != "false",
            "disk_label" => cfg.disk_label = non_empty(value),
            "disk" => cfg.disk = value != "false",
            // A blank list reads as unset — the same rule an empty label
            // follows. `disks = ""` is the obvious first edit when you are
            // trying to *change* the drives, and honouring it literally would
            // silently drop the metric with no clue why; `disk = false` is how
            // you turn it off.
            "disks" => {
                if let Some(drives) = parse_disks(value) {
                    cfg.disks = drives;
                }
            }
            // Stored raw: naming the tiers in two places would let the parser
            // and `icons::resolve` disagree about what `Nerd-Font` means.
            "icons" => cfg.icons = value.to_string(),
            _ => {}
        }
    }
    cfg
}

/// Parse herdr's OWN `config.toml` text for `cpu_label` / `ram_label` /
/// `battery_label`, reading them ONLY inside the `[ui]` section — not
/// `[ui.toast]` or any other table.
fn parse_herdr_labels(text: &str) -> Labels {
    let mut labels = Labels::default();
    let mut in_ui = false;
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = section_name(line) {
            in_ui = section.trim() == "ui"; // [ui] only, not [ui.toast] etc.
            continue;
        }
        if !in_ui {
            continue;
        }
        // An EMPTY label reads as unset, not as "name nothing".
        //
        // herdr's own config ships these keys as commented-out templates with
        // empty quotes and a note naming the glyph to paste:
        //
        //     # cpu_label = ""   #  nf-oct-cpu
        //
        // Uncommenting one without filling it in is the obvious first move, and
        // honouring the blank literally would silently strip the naming off
        // every row — leaving bare percentages and no clue why. Treating it as
        // unset keeps the tier's own naming, which is the recoverable answer.
        // This also matches `non_empty_env`, which reads an empty environment
        // value as absent for the same reason.
        match parse_kv_line(line) {
            Some(("cpu_label", value)) => labels.cpu = non_empty(value),
            Some(("ram_label", value)) => labels.ram = non_empty(value),
            Some(("battery_label", value)) => labels.battery = non_empty(value),
            Some(("disk_label", value)) => labels.disk = non_empty(value),
            _ => {}
        }
    }
    labels
}

/// [`parse_config`] for tests in other modules — [`crate::icons`] checks that
/// the config block its `--icons` preview tells people to paste actually parses
/// back into the settings it claims to set.
#[cfg(test)]
pub fn parse_config_for_test(text: &str) -> Config {
    parse_config(text)
}

/// [`parse_herdr_labels`] for tests in other modules, for the same reason as
/// [`parse_config_for_test`].
#[cfg(test)]
pub fn parse_herdr_labels_for_test(text: &str) -> Labels {
    parse_herdr_labels(text)
}

/// `Some(owned)` for a non-empty string, `None` for an empty one.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Split a `disks = "/, /home"` value into drives, or `None` when it names none.
///
/// Comma-separated rather than a TOML array because the parser here is a flat
/// `key = value` reader by design (see [`parse_kv_line`]) — a real array would
/// mean a TOML dependency for one setting. Blanks are dropped and repeats
/// collapse, so a trailing comma or a doubled entry costs nothing rather than
/// drawing the same drive twice.
fn parse_disks(value: &str) -> Option<Vec<String>> {
    let mut drives: Vec<String> = Vec::new();
    for drive in value.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        if !drives.iter().any(|seen| seen == drive) {
            drives.push(drive.to_string());
        }
    }
    (!drives.is_empty()).then_some(drives)
}

/// Section name inside a leading `[...]` table header (the `[^\]]+` up to the
/// first `]`), or `None` when the line is not a table header.
fn section_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let inner = &rest[..rest.find(']')?];
    (!inner.is_empty()).then_some(inner)
}

/// Split one flat `key = value` line into `(key, value)`, unquoted and with any
/// inline `#` comment removed.
///
/// Deliberately naive, matching the subset of TOML these config files use: the
/// key is one or more ASCII letters/underscores, and the value is everything
/// after the FIRST `=`, handed to [`value_of`].
fn parse_kv_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
        return None;
    }
    value_of(value.trim()).map(|value| (key, value))
}

/// The value of a `key = value` right-hand side.
///
/// **Quoted**: everything between the opening quote and the next matching one.
/// Whatever follows is discarded, which is what makes an inline comment work.
/// This is not cosmetic — herdr's own `config.toml` documents its keys with
/// trailing comments, e.g.
///
/// ```toml
/// cpu_label = ""   #  nf-oct-cpu
/// ```
///
/// and the previous parser handed back `"   #  nf-oct-cpu` as the label, which
/// then rendered verbatim into the sidebar. Anyone following herdr's own
/// documented example got garbage.
///
/// An explicit `""` survives as an empty value rather than being rejected: for a
/// label that is a meaningful setting — "name nothing, just show the number".
///
/// **Unquoted**: everything up to the first `#`, trimmed, and non-empty. One
/// stray trailing quote is still forgiven, matching the older lenient
/// behaviour so a half-quoted value keeps working.
fn value_of(rest: &str) -> Option<&str> {
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let body = &rest[quote.len_utf8()..];
        return Some(match body.find(quote) {
            Some(end) => &body[..end],
            // Unterminated: fall back to lenient stripping rather than dropping
            // a setting the user plainly meant.
            None => strip_quotes(body),
        });
    }
    let bare = strip_quotes(rest.split('#').next().unwrap_or("").trim());
    (!bare.is_empty()).then_some(bare)
}

/// Remove at most one leading and one trailing quote (`"` or `'`), independently
/// — the `str.replace(/^["']|["']$/g, '')` behaviour (mismatched quotes and a
/// lone quote both collapse rather than erroring).
fn strip_quotes(s: &str) -> &str {
    let is_quote = |c: char| c == '"' || c == '\'';
    let s = s.strip_prefix(is_quote).unwrap_or(s);
    s.strip_suffix(is_quote).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- plugin config: parse_config ----------------------------------------

    #[test]
    fn config_empty_text_yields_documented_defaults() {
        let cfg = parse_config("");
        // Sidebar since 1.8.0: a fresh install writes no config at all, so this
        // default IS what a new user gets, and the spaces card is the surface
        // they are looking at when they say the plugin shows nothing.
        assert_eq!(cfg.mode, Mode::Sidebar);
        assert_eq!(cfg.interval_seconds, 5);
        assert!(cfg.window_title_totals);
        assert!(cfg.battery, "the battery cell is on unless opted out");
        assert_eq!(cfg.icons, "auto");
        assert!(cfg.disk, "the disk cell is on unless opted out");
        // The drive people mean when they say "my disk", and the one every
        // machine has.
        assert_eq!(cfg.disks, default_disks());
        assert_eq!(cfg.disks.len(), 1, "one cell by default: {:?}", cfg.disks);
        assert_eq!(cfg.disk_label, None);
    }

    #[test]
    fn the_default_drive_is_the_root_filesystem() {
        let disks = default_disks();
        if cfg!(windows) {
            // Whatever this install booted from — a machine on `D:` must not be
            // told about a `C:` it may not have.
            assert!(disks[0].ends_with(':'), "{disks:?}");
        } else {
            assert_eq!(disks, vec!["/".to_string()]);
        }
    }

    #[test]
    fn config_mode_only_accepts_known_values() {
        assert_eq!(parse_config("mode = sidebar").mode, Mode::Sidebar);
        assert_eq!(parse_config("mode = agents-panel").mode, Mode::AgentsPanel);
        // Unknown value leaves the default untouched.
        assert_eq!(parse_config("mode = bogus").mode, Mode::Sidebar);
    }

    #[test]
    fn each_mode_names_the_table_that_renders_it() {
        // The two modes render from different herdr tables. Writing the `$usage`
        // row into the wrong one produces exactly the symptom we are fixing —
        // everything works and nothing appears.
        let (spaces, spaces_rows) = Mode::Sidebar.sidebar_table();
        let (agents, agents_rows) = Mode::AgentsPanel.sidebar_table();
        assert_eq!(spaces, "ui.sidebar.spaces");
        assert_eq!(agents, "ui.sidebar.agents");
        assert_ne!(spaces, agents);
        // The defaults carried alongside each name are herdr's own, so appending
        // a table cannot silently drop the rows a user already had.
        assert!(spaces_rows.iter().any(|row| row.contains("git_status")));
        assert!(agents_rows.iter().any(|row| row.contains("agent")));
    }

    #[test]
    fn config_quotes_are_stripped_from_values() {
        assert_eq!(parse_config("mode = \"sidebar\"").mode, Mode::Sidebar);
        assert_eq!(parse_config("mode = 'sidebar'").mode, Mode::Sidebar);
        // Mismatched leading/trailing quotes are stripped independently.
        assert_eq!(parse_config("mode = \"sidebar'").mode, Mode::Sidebar);
    }

    #[test]
    fn config_interval_seconds_gates_on_ge_one() {
        assert_eq!(parse_config("interval_seconds = 12").interval_seconds, 12);
        assert_eq!(parse_config("interval_seconds = \"7\"").interval_seconds, 7);
        // Below 1, zero, non-numeric, and empty-after-quotes keep the default 5.
        assert_eq!(parse_config("interval_seconds = 0").interval_seconds, 5);
        assert_eq!(parse_config("interval_seconds = -3").interval_seconds, 5);
        assert_eq!(parse_config("interval_seconds = fast").interval_seconds, 5);
    }

    #[test]
    fn config_interval_seconds_is_clamped_to_the_ttl_ceiling() {
        // herdr caps `ttl_ms` at 24 h and the daemon asks for three intervals,
        // so anything past the cap would have every report refused.
        assert_eq!(
            parse_config("interval_seconds = 28800").interval_seconds,
            MAX_INTERVAL_SECONDS,
        );
        assert_eq!(
            parse_config("interval_seconds = 999999").interval_seconds,
            MAX_INTERVAL_SECONDS,
        );
        // A float far past u64 saturates on cast, then clamps — never wraps.
        assert_eq!(
            parse_config("interval_seconds = 1e30").interval_seconds,
            MAX_INTERVAL_SECONDS,
        );
    }

    #[test]
    fn config_window_title_totals_false_only_on_literal_false() {
        assert!(!parse_config("window_title_totals = false").window_title_totals);
        assert!(!parse_config("window_title_totals = \"false\"").window_title_totals);
        // Anything other than the literal `false` is truthy.
        assert!(parse_config("window_title_totals = true").window_title_totals);
        assert!(parse_config("window_title_totals = 0").window_title_totals);
    }

    #[test]
    fn config_battery_false_only_on_literal_false() {
        assert!(!parse_config("battery = false").battery);
        assert!(!parse_config("battery = \"false\"").battery);
        // Anything other than the literal `false` is truthy — same rule as
        // `window_title_totals`, so the two booleans cannot drift apart.
        assert!(parse_config("battery = true").battery);
        assert!(parse_config("battery = 0").battery);
    }

    // ---- plugin config: the drives -------------------------------------------

    #[test]
    fn config_disks_is_a_comma_separated_list_in_the_order_given() {
        assert_eq!(
            parse_config("disks = \"/, /home, /mnt/data\"").disks,
            vec!["/", "/home", "/mnt/data"],
            "the cells are drawn in the order the user listed them",
        );
        // Unquoted, a single drive, and Windows letters all work the same way.
        assert_eq!(parse_config("disks = /home").disks, vec!["/home"]);
        assert_eq!(parse_config("disks = \"C:, D:\"").disks, vec!["C:", "D:"]);
    }

    #[test]
    fn config_disks_drops_blanks_and_repeats() {
        // A trailing comma is the commonest typo, and a doubled entry would
        // otherwise draw the same drive twice.
        assert_eq!(
            parse_config("disks = \"/, ,/home,\"").disks,
            vec!["/", "/home"]
        );
        assert_eq!(parse_config("disks = \"/, /\"").disks, vec!["/"]);
    }

    #[test]
    fn config_disks_blank_reads_as_unset_not_as_none() {
        // The same rule an empty label follows: `disks = ""` is what you write
        // while you are *changing* the drives, and honouring it literally would
        // silently drop the metric. `disk = false` is how you turn it off.
        let default = default_disks();
        assert_eq!(parse_config("disks = \"\"").disks, default);
        assert_eq!(parse_config("disks = \" , \"").disks, default);
    }

    #[test]
    fn config_disk_false_only_on_literal_false() {
        assert!(!parse_config("disk = false").disk);
        assert!(!parse_config("disk = \"false\"").disk);
        // Same truthiness rule as `battery` and `window_title_totals`, so the
        // three booleans cannot drift apart.
        assert!(parse_config("disk = true").disk);
        assert!(parse_config("disk = 0").disk);
    }

    #[test]
    fn config_disk_false_takes_no_reading_at_all() {
        // Hardware-independent: with the metric off there is nothing to stat,
        // and every renderer downstream sees the empty list an unreadable host
        // produces. `disks` is still parsed, so turning it back on keeps the
        // selection.
        let cfg = parse_config("disk = false\ndisks = \"/, /home\"");
        assert_eq!(cfg.disk_readings(), Vec::new());
        assert_eq!(cfg.disks, vec!["/", "/home"]);
    }

    #[test]
    fn config_disk_readings_answer_for_the_selected_drives() {
        // The live host, through the config: the root filesystem always exists,
        // and a path that does not is dropped rather than faked.
        let root = if cfg!(windows) { "C:" } else { "/" };
        let cfg = parse_config(&format!("disks = \"{root}, /no-such-mount-point\""));
        let readings = cfg.disk_readings();
        assert_eq!(readings.len(), 1, "{readings:?}");
        assert_eq!(readings[0].name, root);
        assert!(readings[0].total_mb > 0.0, "{readings:?}");
    }

    #[test]
    fn the_disk_label_follows_herdrs_config_and_the_plugin_overrides_it() {
        // A sidebar header that draws free space names it with `ui.disk_label`,
        // so honouring the key keeps that header and these cells saying the same
        // word — the bargain cpu and ram already make.
        let from_herdr = parse_herdr_labels("[ui]\ndisk_label = \"HERDR\"\n");
        assert_eq!(from_herdr.disk(), Some("HERDR"));
        assert_eq!(
            from_herdr.clone().with_overrides(&Config::default()).disk(),
            Some("HERDR"),
            "nothing set plugin-side leaves herdr's word standing",
        );

        // The plugin's own key wins where both are set, as it does for battery.
        let cfg = parse_config("disk_label = \"free\"");
        assert_eq!(cfg.disk_label.as_deref(), Some("free"));
        assert_eq!(from_herdr.with_overrides(&cfg).disk(), Some("free"));

        // With neither, the icon tier does the naming.
        assert_eq!(
            Labels::default().with_overrides(&Config::default()).disk(),
            None,
        );
    }

    #[test]
    fn config_battery_false_takes_no_reading_at_all() {
        // The whole point of the gate: opting out costs zero syscalls, and
        // every renderer downstream sees the same `None` a desktop produces.
        // Hardware-independent — true on a laptop and on a VM alike.
        let cfg = parse_config("battery = false");
        assert_eq!(cfg.battery_reading(), None);
    }

    #[test]
    fn config_icons_keeps_the_raw_tier_name() {
        // The parser stores what the user typed; `icons::resolve` owns the
        // vocabulary, including the case/separator folding it does.
        assert_eq!(parse_config("icons = nerdfont").icons, "nerdfont");
        assert_eq!(parse_config("icons = \"Nerd-Font\"").icons, "Nerd-Font");
        assert_eq!(parse_config("icons = bogus").icons, "bogus");
        assert_eq!(
            parse_config("icons = nerdfont").icon_set(),
            IconSet::NerdFont,
        );
        // A typo is cosmetic, never fatal: it falls back to auto-detection.
        // Which tier that yields depends on the host's installed fonts, so this
        // asserts only what is true on every machine — auto never selects a
        // font-dependent *emoji* tier, and never the gauge. The detector's own
        // behaviour is pinned in `icons`, where the font probe is injectable and
        // the outcome is therefore deterministic.
        assert!(matches!(
            parse_config("icons = bogus").icon_set(),
            IconSet::Text | IconSet::NerdFont,
        ));
    }

    #[test]
    fn config_skips_comments_and_malformed_lines() {
        let text = "\
            # mode = agents-panel\n\
            not a config line\n\
            mode2 = agents-panel\n\
            interval_seconds = 9\n";
        let cfg = parse_config(text);
        // The commented and digit-keyed lines are ignored; the valid one applies.
        assert_eq!(cfg.mode, Mode::Sidebar);
        assert_eq!(cfg.interval_seconds, 9);
    }

    // ---- herdr labels: [ui] gating + quotes ---------------------------------

    #[test]
    fn labels_are_unset_when_there_is_no_ui_section() {
        let labels = parse_herdr_labels("[server]\ncpu_label = \"NOPE\"\n");
        // Unset, NOT "the default string": the renderer treats a label the user
        // actually chose differently from one it invented, so the two must not
        // collapse into the same value here.
        assert_eq!(labels.cpu(), None);
        assert_eq!(labels.ram(), None);
        assert_eq!(labels.battery(), None);
        assert_eq!(labels.disk(), None);
        // Surfaces that always spell a word still get one.
        assert_eq!(labels.cpu_word(), "cpu");
        assert_eq!(labels.ram_word(), "ram");
    }

    #[test]
    fn the_plugin_config_owns_the_battery_label() {
        // herdr does not know `battery_label`, so putting it in herdr's [ui]
        // makes every `reload-config` log `unknown config key`. The plugin
        // config is its proper home, and it overrides herdr's if both are set.
        let cfg = parse_config("battery_label = \"\u{f241}\"");
        assert_eq!(cfg.battery_label.as_deref(), Some("\u{f241}"));

        let from_herdr = parse_herdr_labels("[ui]\nbattery_label = \"HERDR\"\n");
        assert_eq!(
            from_herdr.clone().with_overrides(&cfg).battery(),
            Some("\u{f241}")
        );

        // With nothing set plugin-side, herdr's value still applies.
        let bare = Config::default();
        assert_eq!(from_herdr.with_overrides(&bare).battery(), Some("HERDR"));
    }

    #[test]
    fn plugin_cpu_and_ram_label_overrides_win_per_key() {
        // Each key overrides independently: naming one metric plugin-side must
        // not detach the other from herdr's config.
        let herdr = parse_herdr_labels("[ui]\ncpu_label = \"H\"\nram_label = \"R\"\n");
        let cfg = parse_config("ram_label = 'M'\n");
        let labels = herdr.with_overrides(&cfg);
        assert_eq!(labels.cpu(), Some("H")); // untouched key keeps herdr's value
        assert_eq!(labels.ram(), Some("M"));

        let bare = parse_config("");
        assert_eq!(bare.cpu_label, None);
        assert_eq!(bare.ram_label, None);
    }

    #[test]
    fn an_empty_plugin_label_is_name_nothing_not_unset() {
        // The herdr-side blank-means-unset rule exists because herdr ships the
        // keys as commented templates with empty quotes. Nothing ships these
        // plugin keys at all, so an empty value here can only be deliberate —
        // it reaches the renderer as the bare-number "name nothing".
        let cfg = parse_config("cpu_label = \"\"\n");
        assert_eq!(cfg.cpu_label.as_deref(), Some(""));
        let labels = Labels::default().with_overrides(&cfg);
        assert_eq!(labels.cpu(), Some(""));
        assert_eq!(labels.ram(), None); // the other key stays unset

        // Surfaces that always spell a word still get one — a wide report
        // column headed by nothing would be a bare figure floating in a table.
        assert_eq!(labels.cpu_word(), "cpu");

        // Both words, not just cpu: the report has a RAM column with exactly the
        // same blank-heading failure.
        let ram = Labels::default().with_overrides(&parse_config("ram_label = \"\"\n"));
        assert_eq!(ram.ram(), Some(""));
        assert_eq!(ram.ram_word(), "ram");
    }

    #[test]
    fn every_plugin_label_reads_an_empty_value_the_same_way() {
        // One file, one rule. `battery_label` used to read a blank as unset
        // while the other two read it as "name nothing", which made the same
        // three characters mean opposite things three lines apart.
        let cfg = parse_config("cpu_label = \"\"\nram_label = \"\"\nbattery_label = \"\"\n");
        assert_eq!(cfg.cpu_label.as_deref(), Some(""));
        assert_eq!(cfg.ram_label.as_deref(), Some(""));
        assert_eq!(cfg.battery_label.as_deref(), Some(""));

        // An absent key is still unset — that is the difference that matters.
        let bare = parse_config("");
        assert_eq!(bare.battery_label, None);
    }

    #[test]
    fn ram_display_defaults_to_percent_and_ignores_unknown_values() {
        assert_eq!(parse_config("").ram_display, RamDisplay::Percent);
        assert_eq!(
            parse_config("ram_display = \"both\"").ram_display,
            RamDisplay::Percent
        );
    }

    #[test]
    fn ram_display_forgives_case_and_quoting_the_way_icons_does() {
        // `icons` in this same file folds case, so a file that took `Emoji` but
        // silently ignored `GB` would be a trap rather than a rule.
        for text in [
            "ram_display = \"GB\"",
            "ram_display = \"Gb\"",
            "ram_display = \"ABSOLUTE\"",
            "ram_display = gb", // unquoted is legal in this parser
        ] {
            assert_eq!(
                parse_config(text).ram_display,
                RamDisplay::Absolute,
                "{text}"
            );
        }
        assert_eq!(
            parse_config("ram_display = \"PERCENT\"").ram_display,
            RamDisplay::Percent
        );
    }

    #[test]
    fn ram_display_gb_and_absolute_both_select_the_absolute_form() {
        // `gb` is what users reach for, `absolute` is what it does — the cell
        // stays in MB below a gigabyte either way.
        assert_eq!(
            parse_config("ram_display = \"gb\"").ram_display,
            RamDisplay::Absolute
        );
        assert_eq!(
            parse_config("ram_display = \"absolute\"").ram_display,
            RamDisplay::Absolute
        );
        assert_eq!(
            parse_config("ram_display = \"percent\"").ram_display,
            RamDisplay::Percent
        );
    }

    #[test]
    fn an_empty_label_reads_as_unset_not_as_blank() {
        // herdr ships these keys as commented templates with empty quotes:
        //     # cpu_label = ""   #  nf-oct-cpu
        // Uncommenting one without pasting a glyph must not strip the naming
        // off every row and leave bare percentages.
        let labels = parse_herdr_labels("[ui]\ncpu_label = \"\"\nram_label = \"\"\n");
        assert_eq!(labels.cpu(), None);
        assert_eq!(labels.ram(), None);
        assert_eq!(labels.cpu_word(), "cpu");
        // The same line with a real glyph pasted in IS set.
        let filled = parse_herdr_labels("[ui]\ncpu_label = \"\u{f4bc}\"   # nf-oct-cpu\n");
        assert_eq!(filled.cpu(), Some("\u{f4bc}"));
    }

    #[test]
    fn a_label_set_to_the_default_word_is_still_explicitly_set() {
        // The distinction the old string-comparison could not make. Someone who
        // writes `cpu_label = "cpu"` has chosen a word, and a glyph tier must
        // honour that choice rather than treating it as "nothing configured".
        let labels = parse_herdr_labels("[ui]\ncpu_label = \"cpu\"\n");
        assert_eq!(labels.cpu(), Some("cpu"));
        assert_eq!(labels.ram(), None);
    }

    #[test]
    fn labels_read_only_inside_ui_section() {
        let text = "\
            [ui]\n\
            cpu_label = \"C\"\n\
            ram_label = 'M'\n\
            battery_label = \"PWR\"\n\
            [ui.toast]\n\
            cpu_label = \"WRONG\"\n\
            ram_label = \"WRONG\"\n\
            battery_label = \"WRONG\"\n";
        let labels = parse_herdr_labels(text);
        assert_eq!(labels.cpu(), Some("C")); // from [ui], not [ui.toast]
        assert_eq!(labels.ram(), Some("M"));
        assert_eq!(labels.battery(), Some("PWR"));
    }

    #[test]
    fn labels_ignored_before_ui_section() {
        let text = "\
            cpu_label = \"EARLY\"\n\
            [ui]\n\
            ram_label = \"R\"\n";
        let labels = parse_herdr_labels(text);
        assert_eq!(labels.cpu(), None); // key before any section is ignored
        assert_eq!(labels.ram(), Some("R"));
    }

    #[test]
    fn labels_section_header_is_trimmed_before_matching() {
        // `[ ui ]` still counts as the ui table — the name is trimmed.
        let labels = parse_herdr_labels("[ ui ]\ncpu_label = X\n");
        assert_eq!(labels.cpu(), Some("X"));
    }

    // ---- state paths: one updater per session --------------------------------

    #[test]
    fn a_pid_file_is_named_for_one_session_only() {
        // The state dir is shared by every session herdr runs, so the session
        // key in the name is the only thing keeping two sessions' updaters
        // apart. Same key, same file; different key, different file.
        assert_eq!(pid_file_name("42c3c964"), "updater-42c3c964.pid");
        assert_ne!(pid_file_name("42c3c964"), pid_file_name("e60b12c5"));
    }

    #[test]
    fn a_sweep_finds_every_sessions_claim_and_the_legacy_one() {
        // `--disable` is one decision for the whole machine, so it has to reach
        // updaters this session never started — including the daemon left
        // holding the un-keyed pid file an upgrade from before 1.11.1 stranded.
        let dir = crate::testutil::scratch("pid-files");
        for name in [
            pid_file_name("aaaaaaaa"),
            pid_file_name("bbbbbbbb"),
            LEGACY_PID_FILE.to_string(),
            // An empty key is still this scheme's shape — a socket path that
            // could not be resolved. A file we cannot explain is safer swept
            // than left holding a claim nothing ever releases.
            "updater-.pid".to_string(),
        ] {
            std::fs::write(dir.join(name), "1\n").expect("fixture pid file");
        }
        // The rest of the state dir stays out of it: these record the user's
        // decisions, not a process to stop.
        for other in ["enabled", "bootstrapped", "updater.pid.bak"] {
            std::fs::write(dir.join(other), "1\n").expect("fixture state file");
        }

        let found: Vec<String> = pid_files_in(&dir)
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Sorted, so a caller behaves the same run to run.
        assert_eq!(
            found,
            [
                "updater-.pid",
                "updater-aaaaaaaa.pid",
                "updater-bbbbbbbb.pid",
                LEGACY_PID_FILE,
            ],
        );
    }

    #[test]
    fn a_state_dir_that_is_not_there_yields_no_claims() {
        // A fresh install runs `--disable` before anything has written the dir.
        // Nothing to stop is not an error.
        let missing = crate::testutil::scratch("pid-files-missing").join("nope");
        assert!(pid_files_in(&missing).is_empty());
    }

    // ---- shared helpers ------------------------------------------------------

    #[test]
    fn strip_quotes_matches_js_semantics() {
        assert_eq!(strip_quotes("\"foo\""), "foo");
        assert_eq!(strip_quotes("'foo'"), "foo");
        assert_eq!(strip_quotes("\"foo"), "foo"); // leading only
        assert_eq!(strip_quotes("foo\""), "foo"); // trailing only
        assert_eq!(strip_quotes("\"foo'"), "foo"); // mismatched
        assert_eq!(strip_quotes("\""), ""); // lone quote collapses to empty
        assert_eq!(strip_quotes("bare"), "bare");
    }

    #[test]
    fn parse_kv_line_strips_inline_comments_from_herdrs_own_config_style() {
        // herdr's config.toml documents its keys with trailing comments. The
        // parser used to hand the whole tail back as the value, so following
        // herdr's own example put `"   #  nf-oct-cpu` in the sidebar.
        assert_eq!(
            parse_kv_line(r#"cpu_label = "X"   #  nf-oct-cpu"#),
            Some(("cpu_label", "X")),
        );
        assert_eq!(
            parse_kv_line("interval_seconds = 12  # seconds"),
            Some(("interval_seconds", "12")),
        );
        assert_eq!(
            parse_kv_line("mode = sidebar # why"),
            Some(("mode", "sidebar"))
        );
        // A `#` INSIDE quotes is part of the value, not a comment.
        assert_eq!(
            parse_kv_line(r##"cpu_label = "#1""##),
            Some(("cpu_label", "#1"))
        );
        // An explicit empty string is a real setting — "name nothing" — and must
        // survive rather than being rejected as a missing value.
        assert_eq!(parse_kv_line(r#"cpu_label = """#), Some(("cpu_label", "")));
    }

    #[test]
    fn parse_kv_line_rejects_bad_keys_and_empty_values() {
        assert_eq!(parse_kv_line("mode = sidebar"), Some(("mode", "sidebar")));
        assert_eq!(parse_kv_line("  spaced  =  v  "), Some(("spaced", "v")));
        assert_eq!(parse_kv_line("mode2 = x"), None); // digit in key
        assert_eq!(parse_kv_line("a b = x"), None); // space in key
        assert_eq!(parse_kv_line("noeq"), None); // no '='
        assert_eq!(parse_kv_line("mode =   "), None); // empty value
                                                      // The first '=' splits; later '=' stays in the value.
        assert_eq!(parse_kv_line("mode = a=b"), Some(("mode", "a=b")));
    }
}
