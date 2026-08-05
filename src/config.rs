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
    /// Naming for the battery cell, overriding herdr's `[ui] battery_label`.
    ///
    /// Battery lives here rather than only in herdr's config because herdr has
    /// no battery of its own to label: `battery_label` is not a key it knows, so
    /// putting it in herdr's `[ui]` makes `herdr server reload-config` report
    /// `unknown config key ui.battery_label; ignoring key` on every reload.
    /// Harmless but noisy, and needless — nothing outside this plugin renders a
    /// battery, so there is no second surface to keep in step. `cpu_label` and
    /// `ram_label` stay in herdr's config precisely because the sidebar's
    /// system-usage header does share those.
    ///
    /// Names the battery wherever the plugin draws it: the window title, the
    /// report's total line, and the `--icons` preview.
    pub battery_label: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Sidebar,
            interval_seconds: 5,
            window_title_totals: true,
            battery: true,
            icons: "auto".to_string(),
            battery_label: None,
        }
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
}

/// Default naming for each metric when herdr's `[ui]` config sets none.
pub const DEFAULT_CPU_LABEL: &str = "cpu";
pub const DEFAULT_RAM_LABEL: &str = "ram";
pub const DEFAULT_BATTERY_LABEL: &str = "bat";

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
}

impl Labels {
    /// Build a set of labels directly, bypassing herdr's config file.
    ///
    /// `None` means "herdr named nothing for this metric", which is what lets an
    /// icon tier supply its own naming. Test-only: production always arrives
    /// here through [`parse_herdr_labels`], and an unused constructor on a
    /// public type is an invitation to construct one some other way.
    #[cfg(test)]
    pub fn new(cpu: Option<&str>, ram: Option<&str>, battery: Option<&str>) -> Self {
        Self {
            cpu: cpu.map(str::to_string),
            ram: ram.map(str::to_string),
            battery: battery.map(str::to_string),
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

    /// Apply the plugin config's own label overrides on top of herdr's.
    ///
    /// Only battery has one, for the reason given on [`Config::battery_label`].
    /// Returning `Self` keeps the load-then-override pair a single expression at
    /// each call site, so no caller can load the labels and forget the overrides.
    pub fn with_overrides(mut self, config: &Config) -> Self {
        if let Some(label) = &config.battery_label {
            self.battery = Some(label.clone());
        }
        self
    }

    /// The word to print on surfaces that always spell one out regardless of
    /// tier — the full-width terminal report's columns and its total line.
    pub fn cpu_word(&self) -> &str {
        self.cpu().unwrap_or(DEFAULT_CPU_LABEL)
    }

    pub fn ram_word(&self) -> &str {
        self.ram().unwrap_or(DEFAULT_RAM_LABEL)
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

/// Updater single-instance pid file (`<state_dir>/updater.pid`).
pub fn pid_file() -> PathBuf {
    state_dir().join("updater.pid")
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
/// (numeric `>= 1`), `window_title_totals` and `battery` (`false` only when
/// they equal the literal `false`, any other value is truthy), and `icons`
/// (a tier name kept verbatim for [`crate::icons::resolve`]). Unknown keys are
/// ignored.
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
            "battery_label" => cfg.battery_label = non_empty(value),
            "battery" => cfg.battery = value != "false",
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
            _ => {}
        }
    }
    labels
}

/// `Some(owned)` for a non-empty string, `None` for an empty one.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
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
