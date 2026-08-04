//! Plugin config + herdr `[ui]` label loading, and env/state path resolution.
//!
//! - [`load_config`] parses `$HERDR_PLUGIN_CONFIG_DIR/config.toml` (flat
//!   `key = value` lines).
//! - [`load_herdr_labels`] reads `cpu_label` / `ram_label` from herdr's OWN
//!   `[ui]` section so per-space rows match the patched sidebar header.
//! - The path helpers resolve the herdr-injected env (`HERDR_PLUGIN_*`) with the
//!   same `<tmpdir>/<id>` fallbacks the runtime uses.

use std::path::PathBuf;

/// Status-surfacing strategy (plugin `config.toml` `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Stock herdr: a "usage" pseudo-agent per space in the agents panel.
    AgentsPanel,
    /// Patched herdr: display-only metadata rendered inside the spaces card.
    Sidebar,
}

/// Plugin user config from `$HERDR_PLUGIN_CONFIG_DIR/config.toml`.
#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub interval_seconds: u64,
    pub window_title_totals: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::AgentsPanel,
            interval_seconds: 5,
            window_title_totals: true,
        }
    }
}

/// CPU / RAM label tokens sourced from herdr's `[ui]` config (default cpu/ram).
#[derive(Debug, Clone)]
pub struct Labels {
    pub cpu: String,
    pub ram: String,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            cpu: "cpu".to_string(),
            ram: "ram".to_string(),
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

/// Load `cpu_label` / `ram_label` from herdr's `[ui]` config section.
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

/// Marker recording that the updater is *wanted* (`<state_dir>/enabled`).
///
/// The pid file says whether a daemon is live *right now*; this says whether the
/// user ever asked for one. `--restore` (the manifest `[[startup]]` hook) reads
/// it so the updater comes back after a herdr or machine restart instead of
/// silently staying off until someone re-invokes `status-enable`.
pub fn enabled_flag() -> PathBuf {
    state_dir().join("enabled")
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

/// XDG config base: `$XDG_CONFIG_HOME` if set (and non-empty), else `~/.config`.
pub(crate) fn config_home() -> PathBuf {
    non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// Path to herdr's OWN `config.toml` (`<config_home>/herdr/config.toml`).
fn herdr_config_path() -> PathBuf {
    config_home().join("herdr").join("config.toml")
}

// ---- pure parsers (hand-rolled, no `toml` crate) ----------------------------

/// Parse the plugin's flat `config.toml` text into a [`Config`], starting from
/// the documented defaults.
///
/// Recognised keys: `mode` (`agents-panel` | `sidebar`), `interval_seconds`
/// (numeric `>= 1`), `window_title_totals` (`false` only when it equals the
/// literal `false`, any other value is truthy). Unknown keys are ignored.
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
            _ => {}
        }
    }
    cfg
}

/// Parse herdr's OWN `config.toml` text for `cpu_label` / `ram_label`, reading
/// them ONLY inside the `[ui]` section — not `[ui.toast]` or any other table.
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
        match parse_kv_line(line) {
            Some(("cpu_label", value)) => labels.cpu = value.to_string(),
            Some(("ram_label", value)) => labels.ram = value.to_string(),
            _ => {}
        }
    }
    labels
}

/// Section name inside a leading `[...]` table header (the `[^\]]+` up to the
/// first `]`), or `None` when the line is not a table header.
fn section_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let inner = &rest[..rest.find(']')?];
    (!inner.is_empty()).then_some(inner)
}

/// Split one flat `key = value` line into `(key, unquoted_value)`.
///
/// Deliberately naive, matching the subset of TOML these config files use: the
/// key is one or more ASCII letters/underscores,
/// the value is everything after the FIRST `=` with surrounding whitespace
/// trimmed (non-empty required) and at most one leading and one trailing quote
/// (`"` or `'`) removed. Inline `#` comments are NOT stripped — by design, to
/// keep the parser predictable.
fn parse_kv_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
        return None;
    }
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some((key, strip_quotes(value)))
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
        assert_eq!(cfg.mode, Mode::AgentsPanel);
        assert_eq!(cfg.interval_seconds, 5);
        assert!(cfg.window_title_totals);
    }

    #[test]
    fn config_mode_only_accepts_known_values() {
        assert_eq!(parse_config("mode = sidebar").mode, Mode::Sidebar);
        assert_eq!(parse_config("mode = agents-panel").mode, Mode::AgentsPanel);
        // Unknown value leaves the default untouched.
        assert_eq!(parse_config("mode = bogus").mode, Mode::AgentsPanel);
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
    fn config_skips_comments_and_malformed_lines() {
        let text = "\
            # mode = sidebar\n\
            not a config line\n\
            mode2 = sidebar\n\
            interval_seconds = 9\n";
        let cfg = parse_config(text);
        // The commented and digit-keyed lines are ignored; the valid one applies.
        assert_eq!(cfg.mode, Mode::AgentsPanel);
        assert_eq!(cfg.interval_seconds, 9);
    }

    // ---- herdr labels: [ui] gating + quotes ---------------------------------

    #[test]
    fn labels_default_when_no_ui_section() {
        let labels = parse_herdr_labels("[server]\ncpu_label = \"NOPE\"\n");
        assert_eq!(labels.cpu, "cpu");
        assert_eq!(labels.ram, "ram");
    }

    #[test]
    fn labels_read_only_inside_ui_section() {
        let text = "\
            [ui]\n\
            cpu_label = \"C\"\n\
            ram_label = 'M'\n\
            [ui.toast]\n\
            cpu_label = \"WRONG\"\n\
            ram_label = \"WRONG\"\n";
        let labels = parse_herdr_labels(text);
        assert_eq!(labels.cpu, "C"); // from [ui], not [ui.toast]
        assert_eq!(labels.ram, "M");
    }

    #[test]
    fn labels_ignored_before_ui_section() {
        let text = "\
            cpu_label = \"EARLY\"\n\
            [ui]\n\
            ram_label = \"R\"\n";
        let labels = parse_herdr_labels(text);
        assert_eq!(labels.cpu, "cpu"); // key before any section is ignored
        assert_eq!(labels.ram, "R");
    }

    #[test]
    fn labels_section_header_is_trimmed_before_matching() {
        // `[ ui ]` still counts as the ui table — the name is trimmed.
        let labels = parse_herdr_labels("[ ui ]\ncpu_label = X\n");
        assert_eq!(labels.cpu, "X");
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
