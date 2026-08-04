//! Icon tiers — the glyph vocabulary a metric is rendered with.
//!
//! The sidebar row is only a few characters wide, so every glyph has to earn its
//! column. Four tiers ladder up by how much they assume about the user's font:
//!
//! | tier | cpu | ram | battery | needs |
//! |---|---|---|---|---|
//! | [`IconSet::Text`] | `cpu 26%` | `ram 8%` | `bat 74%` | nothing |
//! | [`IconSet::Unicode`] | `cpu ░26%` | `ram ░8%` | `bat ▓74%` | nothing |
//! | [`IconSet::NerdFont`] | ` 26%` | ` 8%` | ` 74%` | a Nerd Font |
//! | [`IconSet::Emoji`] | `💻26%` | `🧠8%` | `🔋74%` | a colour emoji font |
//!
//! Only the first two are safe unprompted, and that claim is measured rather
//! than assumed: every glyph they emit was checked with `fc-list :charset=<cp>`
//! against the default mono faces (DejaVu Sans Mono, Liberation Mono) and is
//! present in both. Glyphs that are present in only *one* of them — `▣ ▤ ▮ ▯ ▰
//! ▱ ⚡` — are deliberately unused, and emoji are missing from both *and*
//! double-width, which shears a narrow sidebar. The `safe_tiers_*` test enforces
//! that contract against real rendered output, so it cannot rot.
//!
//! [`resolve`] picks the tier. Its `auto` mode chooses between exactly two of
//! them — [`IconSet::NerdFont`] when a Nerd Font is detected, else
//! [`IconSet::Text`]. It deliberately never selects [`IconSet::Unicode`]: those
//! glyphs are *present* everywhere, but `░`/`▒` are dither patterns that render
//! as an indistinct blob at terminal sizes. Being in the font and being legible
//! are different properties, and only the first one is measurable from here.
//! See [`auto_detect`] and [`nerd_font_available`] for what the detection can
//! and cannot know.
//!
//! The word in front of the number always comes from herdr's own `[ui]` config
//! ([`crate::config::Labels`]) — a tier supplies the glyph, never the wording.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::battery::{Battery, State};
use crate::config::non_empty_env;

/// Which glyph vocabulary to render metrics with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSet {
    /// Plain words — always renders, never needs a font.
    Text,
    /// BMP block glyphs verified present in the default mono fonts of all three
    /// platforms. No font install required.
    Unicode,
    /// Nerd Font private-use glyphs. Requires a Nerd Font.
    NerdFont,
    /// Emoji pictograms. Requires a colour emoji font.
    Emoji,
}

// ---- glyph tables (the single source of truth) -------------------------------

/// Level gauge for the [`IconSet::Unicode`] tier, low step first.
///
/// `(exclusive upper bound, glyph)`, scanned in order. The closing
/// `f64::INFINITY` row makes the table total, so no reading — including one past
/// 100 — can fall off the end.
const GAUGE_RAMP: [(f64, char); 4] = [
    (34.0, '░'),          // U+2591 light shade
    (67.0, '▒'),          // U+2592 medium shade
    (90.0, '▓'),          // U+2593 dark shade
    (f64::INFINITY, '█'), // U+2588 full block
];

/// Nerd Font battery family (`nf-fa-battery_*`), low step first.
const NERD_BATTERY_RAMP: [(f64, char); 5] = [
    (15.0, '\u{f244}'),          // U+F244 empty
    (40.0, '\u{f243}'),          // U+F243 quarter
    (65.0, '\u{f242}'),          // U+F242 half
    (90.0, '\u{f241}'),          // U+F241 three quarters
    (f64::INFINITY, '\u{f240}'), // U+F240 full
];

/// U+F4BC `nf-oct-cpu`.
const NERD_CPU: char = '\u{f4bc}';
/// U+EFC5 `nf-md-memory`.
const NERD_RAM: char = '\u{efc5}';

/// U+1F4BB personal computer.
const EMOJI_CPU: char = '💻';
/// U+1F9E0 brain.
const EMOJI_RAM: char = '🧠';
/// U+1F50B battery.
const EMOJI_BATTERY: char = '🔋';

/// Charge is climbing.
const MARK_CHARGING: char = '+';
/// On power but not climbing — topped off, or held down by a charge limit.
const MARK_STEADY: char = '=';

// ---- charge state -------------------------------------------------------------

/// Charge-direction suffix for a battery reading, or `None` when the state says
/// nothing worth a column.
///
/// Deliberately ASCII and shared by all four tiers. Switching tier should change
/// the *pictures*, not the meaning of the line, and a mark that every font on
/// earth can draw can never break the font-safety contract of the safe tiers.
///
/// | state | mark | reads as |
/// |---|---|---|
/// | `Charging` | `+` | going up |
/// | `Full`, `NotCharging` | `=` | on power, holding |
/// | `Discharging`, `Unknown` | none | the number is the whole story |
///
/// The match is exhaustive on purpose: a new [`State`] variant must fail the
/// build here rather than silently inherit a blank mark.
fn charge_mark(state: State) -> Option<char> {
    match state {
        State::Charging => Some(MARK_CHARGING),
        // `Full` is topped off; `NotCharging` is a plugged-in battery a vendor
        // charge limit is holding below full. Both mean "on power, not moving".
        State::Full | State::NotCharging => Some(MARK_STEADY),
        // Discharging is the common case, so it gets the narrowest rendering.
        State::Discharging | State::Unknown => None,
    }
}

// ---- metrics ------------------------------------------------------------------

/// The metrics a tier can draw. Private — callers go through the [`IconSet`]
/// methods, which is what keeps the glyph tables from leaking into the wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cpu,
    Ram,
    Battery,
}

impl Metric {
    /// The word herdr's `[ui]` config yields for this metric when the user has
    /// set no override.
    ///
    /// Mirrors [`crate::config::Labels`]'s defaults. It exists so we can tell "the
    /// user typed this word" from "this is just the default we were handed",
    /// which is what lets an explicit label win even in the glyph tiers — see
    /// [`IconSet::render`].
    fn default_word(self) -> &'static str {
        match self {
            Metric::Cpu => "cpu",
            Metric::Ram => "ram",
            Metric::Battery => "bat",
        }
    }

    /// Nerd Font glyph. Battery is the only metric whose picture tracks the
    /// reading, so it ramps like the gauge does.
    fn nerd_glyph(self, percent: f64) -> char {
        match self {
            Metric::Cpu => NERD_CPU,
            Metric::Ram => NERD_RAM,
            Metric::Battery => ramp_pick(&NERD_BATTERY_RAMP, percent),
        }
    }

    fn emoji(self) -> char {
        match self {
            Metric::Cpu => EMOJI_CPU,
            Metric::Ram => EMOJI_RAM,
            Metric::Battery => EMOJI_BATTERY,
        }
    }
}

// ---- rendering ----------------------------------------------------------------

impl IconSet {
    /// Render CPU load — `cpu ░26%` in the [`IconSet::Unicode`] tier.
    pub fn cpu(self, label: &str, percent: f64) -> String {
        self.render(Metric::Cpu, label, percent, None)
    }

    /// Render RAM use — `ram ░8%` in the [`IconSet::Unicode`] tier.
    pub fn ram(self, label: &str, percent: f64) -> String {
        self.render(Metric::Ram, label, percent, None)
    }

    /// Render a battery reading — `bat ▓74%+` for 74% and charging in the
    /// [`IconSet::Unicode`] tier.
    pub fn battery(self, label: &str, reading: Battery) -> String {
        self.render(
            Metric::Battery,
            label,
            reading.percent,
            charge_mark(reading.state),
        )
    }

    /// `[word ][glyph]<n>%[mark]` — the one assembly every tier and metric goes
    /// through, so the tiers cannot drift apart in spacing or ordering.
    fn render(self, metric: Metric, label: &str, percent: f64, mark: Option<char>) -> String {
        // The glyph tiers put a picture where the word would go, so they print a
        // word only when the user actually chose one (herdr `[ui]` beats the
        // tier — the config escape hatch has to keep working at every setting).
        // Text and Unicode always carry it: there, the word is the only thing
        // naming the number.
        let names_metric = self.spells_out_the_word() || label != metric.default_word();
        let word = if names_metric && !label.is_empty() {
            format!("{label} ")
        } else {
            String::new() // an empty label must not leave a stray leading space
        };

        let glyph = match self {
            IconSet::Text => String::new(),
            // The gauge runs straight into the number it is measuring.
            IconSet::Unicode => ramp_pick(&GAUGE_RAMP, percent).to_string(),
            // Nerd Font glyphs are drawn edge-to-edge in their cell; without the
            // space the digits touch them.
            IconSet::NerdFont => format!("{} ", metric.nerd_glyph(percent)),
            // Emoji already occupy two columns — a space on top reads as a gap.
            IconSet::Emoji => metric.emoji().to_string(),
        };

        let mark = mark.map(String::from).unwrap_or_default();
        format!("{word}{glyph}{}%{mark}", round_percent(percent))
    }

    /// Whether this tier spells the metric out in words rather than drawing it.
    fn spells_out_the_word(self) -> bool {
        match self {
            IconSet::Text | IconSet::Unicode => true,
            IconSet::NerdFont | IconSet::Emoji => false,
        }
    }
}

/// The first glyph in `ramp` whose exclusive upper bound is above `percent`.
///
/// Shared by the Unicode gauge and the Nerd Font battery family so there is one
/// ramp rule instead of two. Both tables close with `f64::INFINITY`, so every
/// finite reading matches — negative and past-100 included.
///
/// The two non-finite readings are handled deliberately rather than by accident,
/// because both compare false against every bound and would otherwise land on
/// whichever step the fallback happened to pick: `NaN` becomes the lowest step
/// (a reading we could not make sense of is not a full one) and `+INFINITY` the
/// highest.
fn ramp_pick(ramp: &[(f64, char)], percent: f64) -> char {
    let percent = if percent.is_nan() {
        f64::NEG_INFINITY
    } else {
        percent
    };
    ramp.iter()
        .find(|&&(upper, _)| percent < upper)
        .or_else(|| ramp.last())
        .map_or(' ', |&(_, glyph)| glyph) // unreachable: both tables are non-empty
}

/// Percentage rounded to whole for display, matching the rounding the daemon
/// already applies to the sidebar row.
///
/// A float-to-int cast saturates rather than wrapping and turns `NaN` into `0`,
/// so no reading can panic here.
fn round_percent(percent: f64) -> i64 {
    percent.round() as i64
}

// ---- tier resolution ------------------------------------------------------------

/// Locale variables in POSIX precedence order, highest first.
const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

/// Pick the tier named by the plugin config, auto-detecting when it says nothing
/// useful.
///
/// Accepts `text`, `unicode`, `nerdfont`, `emoji` and `auto`, ignoring case and
/// any `-`/`_` separators (so `Nerd-Font` works). Anything else — including a
/// missing key — auto-detects instead of erroring: a typo in a cosmetic setting
/// must not blank the sidebar.
pub fn resolve(configured: Option<&str>) -> IconSet {
    resolve_with(configured, non_empty_env, nerd_font_available)
}

/// [`resolve`] with the environment and the font probe injected.
///
/// Both seams exist for the tests. Mutating the real process environment to
/// exercise locale precedence would race every other test thread in the binary,
/// and the font probe reads the *host's* installed fonts — so without a seam
/// every auto-detection test would pass or fail depending on whether the machine
/// running it happens to have a Nerd Font, which is no test at all.
fn resolve_with(
    configured: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    has_nerd_font: impl Fn() -> bool,
) -> IconSet {
    match configured.map(normalise_tier_name).as_deref() {
        Some("text") => IconSet::Text,
        Some("unicode") => IconSet::Unicode,
        Some("nerdfont") => IconSet::NerdFont,
        Some("emoji") => IconSet::Emoji,
        _ => auto_detect(env, has_nerd_font),
    }
}

/// Fold a configured tier name to its canonical spelling.
fn normalise_tier_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(['-', '_'], "")
}

/// The best tier this machine looks able to draw: [`IconSet::NerdFont`] when a
/// Nerd Font is installed, else [`IconSet::Text`].
///
/// Two things this deliberately does NOT do, both learned the hard way:
///
/// - **It never auto-selects [`IconSet::Unicode`].** Those glyphs are *present*
///   in the stock faces — that was measured — but `░`/`▒` are dither patterns,
///   and at terminal sizes they render as an indistinct blob rather than a light
///   shade. Present and legible are different properties, and only the first is
///   measurable from here. The gauge stays available by explicit opt-in.
/// - **It falls back to [`IconSet::Text`], never to something merely likely.**
///   A wrong guess costs the user an unreadable sidebar, which is strictly worse
///   than plain words.
///
/// The locale check gates the whole thing: a terminal that cannot carry UTF-8
/// cannot carry a Nerd Font glyph either, and that is cheap to rule out first.
fn auto_detect(env: impl Fn(&str) -> Option<String>, has_nerd_font: impl Fn() -> bool) -> IconSet {
    // `non_empty_env` treats an empty value as unset, which is also what POSIX
    // says about an empty `LC_ALL` — so an empty higher-precedence variable
    // correctly falls through to the next one.
    let locale = LOCALE_VARS.iter().find_map(|name| env(name));
    // Order matters: the locale check is a couple of `getenv`s, the font probe
    // forks a process. Short-circuit means a `C`-locale host never pays for it.
    if is_utf8_locale(locale.as_deref()) && has_nerd_font() {
        IconSet::NerdFont
    } else {
        IconSet::Text
    }
}

// ---- Nerd Font probe ------------------------------------------------------------

/// Whether some installed font carries [`NERD_CPU`], cached for the process.
///
/// **This detects "a Nerd Font is installed", not "your terminal uses one".**
/// The distinction is not pedantry — it is the whole accuracy budget of this
/// function. herdr draws the sidebar into whatever terminal emulator the user
/// launched it from, and that emulator's font lives in its own config, which no
/// plugin can read. Worse, the updater daemon runs detached with null stdio, so
/// it has no terminal to interrogate even in principle: the usual trick of
/// printing a glyph and asking the terminal where the cursor landed needs a TTY
/// this process does not have.
///
/// So this is a heuristic with one known false positive — a Nerd Font present
/// on disk but not selected in the terminal — which lands the user back on the
/// boxes we are trying to avoid. It is chosen anyway because the alternative
/// (never using icons) is worse for the many people who install a Nerd Font
/// precisely to use it, and because the miss is one config line to correct after
/// `--icons` shows it. `auto` is a convenience, not a contract.
///
/// Cached in a `OnceLock`: the daemon resolves the tier once per refresh, and
/// forking `fc-list` every five seconds forever to be told the same thing would
/// be indefensible.
fn nerd_font_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe_nerd_font(NERD_CPU))
}

/// Ask fontconfig whether any installed font covers `glyph`.
///
/// `fc-list :charset=<hex>` prints one line per matching font and nothing at
/// all when there is no match, so non-empty stdout IS the answer. Anything that
/// goes wrong — no `fc-list` on `PATH`, a non-zero exit, a spawn failure — reads
/// as "no", which falls back to [`IconSet::Text`]: the safe direction.
///
/// fontconfig is a Linux convention. macOS and Windows have their own font
/// databases (CoreText, DirectWrite) and generally no `fc-list`, so `auto`
/// yields `Text` there and those users pick a tier explicitly after running
/// `--icons`. Querying CoreText/DirectWrite would mean real FFI against two more
/// system APIs to sharpen a heuristic that still could not see the terminal's
/// actual font — not worth it.
fn probe_nerd_font(glyph: char) -> bool {
    Command::new("fc-list")
        .arg(format!(":charset={:x}", glyph as u32))
        .arg("family")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success() && !out.stdout.is_empty())
}

/// Whether `locale` names a UTF-8 charset (`en_US.UTF-8`, `en_US.utf8`, …).
fn is_utf8_locale(locale: Option<&str>) -> bool {
    let Some(locale) = locale else {
        return false; // no locale at all: assume the narrowest terminal
    };
    let lower = locale.to_ascii_lowercase();
    lower.contains("utf-8") || lower.contains("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tier, so a new one cannot quietly skip the sweeps below.
    const ALL_TIERS: [IconSet; 4] = [
        IconSet::Text,
        IconSet::Unicode,
        IconSet::NerdFont,
        IconSet::Emoji,
    ];

    /// Every charge state.
    const ALL_STATES: [State; 5] = [
        State::Charging,
        State::Discharging,
        State::Full,
        State::NotCharging,
        State::Unknown,
    ];

    fn bat(percent: f64, state: State) -> Battery {
        Battery { percent, state }
    }

    /// Environment lookup backed by a fixed table, mirroring `non_empty_env`'s
    /// rule that an empty value counts as unset.
    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
                .filter(|value| !value.is_empty())
        }
    }

    /// Resolve with no environment at all — isolates config parsing from locale.
    /// A host with no locale and no Nerd Font — the most conservative machine.
    fn resolve_bare(configured: Option<&str>) -> IconSet {
        resolve_with(configured, env_from(&[]), || false)
    }

    /// Probe stub: pretend a Nerd Font is installed.
    fn has_nerd() -> bool {
        true
    }

    /// Probe stub: pretend none is.
    fn no_nerd() -> bool {
        false
    }

    // ---- tier parsing --------------------------------------------------------

    #[test]
    fn every_tier_name_parses_to_its_tier() {
        assert_eq!(resolve_bare(Some("text")), IconSet::Text);
        assert_eq!(resolve_bare(Some("unicode")), IconSet::Unicode);
        assert_eq!(resolve_bare(Some("nerdfont")), IconSet::NerdFont);
        assert_eq!(resolve_bare(Some("emoji")), IconSet::Emoji);
    }

    #[test]
    fn tier_names_ignore_case_whitespace_and_separators() {
        assert_eq!(resolve_bare(Some("  Unicode ")), IconSet::Unicode);
        assert_eq!(resolve_bare(Some("EMOJI")), IconSet::Emoji);
        assert_eq!(resolve_bare(Some("Nerd-Font")), IconSet::NerdFont);
        assert_eq!(resolve_bare(Some("nerd_font")), IconSet::NerdFont);
    }

    #[test]
    fn auto_unknown_and_missing_all_fall_back_to_detection() {
        // A bare host detects `Text`; a UTF-8 host with a Nerd Font detects
        // `NerdFont`. Both must follow the *detector*, not a hardcoded default.
        let utf8 = || env_from(&[("LANG", "en_US.UTF-8")]);
        for configured in [None, Some("auto"), Some("bogus"), Some("")] {
            assert_eq!(resolve_bare(configured), IconSet::Text, "{configured:?}");
            assert_eq!(
                resolve_with(configured, utf8(), has_nerd),
                IconSet::NerdFont,
                "{configured:?}",
            );
        }
    }

    #[test]
    fn auto_never_selects_the_unicode_gauge() {
        // The gauge glyphs are *present* in the stock faces — measured — but
        // `░`/`▒` are dither patterns that render as an indistinct blob at
        // terminal sizes. Reported from a real sidebar, which is why `auto` no
        // longer reaches for them. No combination of locale and font may bring
        // it back; it stays available only by explicit opt-in.
        for utf8 in [true, false] {
            let env = if utf8 {
                env_from(&[("LANG", "en_US.UTF-8")])
            } else {
                env_from(&[("LANG", "C")])
            };
            for probe in [has_nerd, no_nerd] {
                assert_ne!(
                    auto_detect(&env, probe),
                    IconSet::Unicode,
                    "auto picked the gauge (utf8={utf8})",
                );
            }
        }
        // ..but asking for it by name still works.
        assert_eq!(resolve_bare(Some("unicode")), IconSet::Unicode);
    }

    // ---- locale auto-detection -----------------------------------------------

    #[test]
    fn auto_detect_prefers_lc_all_then_lc_ctype_then_lang() {
        // Highest-precedence variable wins even when it disagrees with the rest.
        let all_wins = env_from(&[
            ("LC_ALL", "C"),
            ("LC_CTYPE", "en_US.UTF-8"),
            ("LANG", "en_US.UTF-8"),
        ]);
        assert_eq!(auto_detect(all_wins, has_nerd), IconSet::Text);

        let ctype_wins = env_from(&[("LC_CTYPE", "C"), ("LANG", "en_US.UTF-8")]);
        assert_eq!(auto_detect(ctype_wins, has_nerd), IconSet::Text);

        // An empty higher-precedence variable is unset, so the next one decides.
        let empty_falls_through = env_from(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")]);
        assert_eq!(
            auto_detect(empty_falls_through, has_nerd),
            IconSet::NerdFont
        );

        let lang_only = env_from(&[("LANG", "en_US.UTF-8")]);
        assert_eq!(auto_detect(lang_only, has_nerd), IconSet::NerdFont);
    }

    // ---- the Nerd Font probe ---------------------------------------------------

    #[test]
    fn a_utf8_locale_without_a_nerd_font_still_falls_back_to_text() {
        // The safe direction. A machine that cannot draw the icons must get
        // words, never a row of boxes.
        let utf8 = env_from(&[("LANG", "en_US.UTF-8")]);
        assert_eq!(auto_detect(utf8, no_nerd), IconSet::Text);
    }

    #[test]
    fn the_font_probe_is_skipped_entirely_on_a_non_utf8_locale() {
        // Short-circuit: the probe forks `fc-list`, so a `C`-locale host must
        // never pay for it. Panicking from the stub proves it is not called.
        let c_locale = env_from(&[("LANG", "C")]);
        let must_not_run = || panic!("font probe ran despite a non-UTF-8 locale");
        assert_eq!(auto_detect(c_locale, must_not_run), IconSet::Text);
    }

    #[test]
    fn probing_for_an_absent_glyph_is_false_and_never_panics() {
        // U+10FFFE is a permanent noncharacter, so no font can claim it. This
        // also exercises the real `fc-list` path: on a host without fontconfig
        // the spawn simply fails, which must read as "no" rather than blowing
        // up. Either way the answer is false, so the test is deterministic
        // across Linux, macOS, and Windows CI runners.
        assert!(!probe_nerd_font('\u{10FFFE}'));
    }

    #[test]
    fn utf8_is_recognised_however_it_is_spelled() {
        assert!(is_utf8_locale(Some("en_US.UTF-8")));
        assert!(is_utf8_locale(Some("en_US.utf8")));
        assert!(is_utf8_locale(Some("C.UTF-8")));
        assert!(is_utf8_locale(Some("de_DE.Utf-8")));
        assert!(is_utf8_locale(Some("ja_JP.UTF8")));
    }

    #[test]
    fn non_utf8_locales_fall_back_to_text() {
        for locale in ["C", "POSIX", "en_US", "en_US.ISO-8859-1", ""] {
            assert_eq!(
                auto_detect(env_from(&[("LANG", locale)]), has_nerd),
                IconSet::Text,
                "{locale:?}",
            );
        }
        // Nothing set at all is also Text.
        assert_eq!(auto_detect(env_from(&[]), has_nerd), IconSet::Text);
        assert!(!is_utf8_locale(None));
    }

    // ---- the level gauge ------------------------------------------------------

    #[test]
    fn gauge_ramp_steps_on_its_documented_boundaries() {
        let gauge = |p: f64| ramp_pick(&GAUGE_RAMP, p);
        assert_eq!(gauge(0.0), '░');
        assert_eq!(gauge(33.0), '░');
        assert_eq!(gauge(33.9), '░');
        assert_eq!(gauge(34.0), '▒');
        assert_eq!(gauge(66.0), '▒');
        assert_eq!(gauge(67.0), '▓');
        assert_eq!(gauge(89.0), '▓');
        assert_eq!(gauge(90.0), '█');
        assert_eq!(gauge(100.0), '█');
    }

    #[test]
    fn gauge_ramp_is_total_for_impossible_readings() {
        // Out of range in both directions and NaN: a glyph, never a panic and
        // never an out-of-bounds step.
        assert_eq!(ramp_pick(&GAUGE_RAMP, -1.0), '░');
        assert_eq!(ramp_pick(&GAUGE_RAMP, -1e30), '░');
        assert_eq!(ramp_pick(&GAUGE_RAMP, 101.0), '█');
        assert_eq!(ramp_pick(&GAUGE_RAMP, f64::INFINITY), '█');
        assert_eq!(ramp_pick(&GAUGE_RAMP, f64::NEG_INFINITY), '░');
        // An unreadable percentage shows as the lowest step, not a full one.
        assert_eq!(ramp_pick(&GAUGE_RAMP, f64::NAN), '░');
    }

    #[test]
    fn ramp_tables_are_ordered_and_total() {
        // `ramp_pick` scans in order, so a table whose bounds are not ascending
        // would silently shadow steps; the closing INFINITY is what makes every
        // finite reading match.
        for ramp in [&GAUGE_RAMP[..], &NERD_BATTERY_RAMP[..]] {
            for pair in ramp.windows(2) {
                assert!(pair[0].0 < pair[1].0, "bounds must ascend: {pair:?}");
            }
            assert_eq!(ramp.last().expect("non-empty").0, f64::INFINITY);
        }
    }

    #[test]
    fn nerd_battery_glyph_tracks_the_charge_level() {
        let glyph = |p: f64| Metric::Battery.nerd_glyph(p);
        assert_eq!(glyph(0.0), '\u{f244}'); // empty
        assert_eq!(glyph(14.9), '\u{f244}');
        assert_eq!(glyph(15.0), '\u{f243}'); // quarter
        assert_eq!(glyph(40.0), '\u{f242}'); // half
        assert_eq!(glyph(65.0), '\u{f241}'); // three quarters
        assert_eq!(glyph(74.0), '\u{f241}');
        assert_eq!(glyph(90.0), '\u{f240}'); // full
        assert_eq!(glyph(100.0), '\u{f240}');
    }

    // ---- rendering, tier by tier ----------------------------------------------

    #[test]
    fn text_tier_is_words_and_numbers_only() {
        assert_eq!(IconSet::Text.cpu("cpu", 26.0), "cpu 26%");
        assert_eq!(IconSet::Text.ram("ram", 8.0), "ram 8%");
        assert_eq!(
            IconSet::Text.battery("bat", bat(74.0, State::Discharging)),
            "bat 74%",
        );
    }

    #[test]
    fn unicode_tier_keeps_the_word_and_adds_a_gauge() {
        assert_eq!(IconSet::Unicode.cpu("cpu", 26.0), "cpu ░26%");
        assert_eq!(IconSet::Unicode.ram("ram", 8.0), "ram ░8%");
        assert_eq!(
            IconSet::Unicode.battery("bat", bat(74.0, State::Discharging)),
            "bat ▓74%",
        );
    }

    #[test]
    fn nerdfont_tier_replaces_the_word_with_a_glyph() {
        assert_eq!(IconSet::NerdFont.cpu("cpu", 26.0), "\u{f4bc} 26%");
        assert_eq!(IconSet::NerdFont.ram("ram", 8.0), "\u{efc5} 8%");
        assert_eq!(
            IconSet::NerdFont.battery("bat", bat(74.0, State::Discharging)),
            "\u{f241} 74%",
        );
    }

    #[test]
    fn emoji_tier_sits_flush_against_the_number() {
        assert_eq!(IconSet::Emoji.cpu("cpu", 26.0), "💻26%");
        assert_eq!(IconSet::Emoji.ram("ram", 8.0), "🧠8%");
        assert_eq!(
            IconSet::Emoji.battery("bat", bat(74.0, State::Discharging)),
            "🔋74%",
        );
    }

    #[test]
    fn percentages_round_to_whole_and_survive_impossible_readings() {
        assert_eq!(IconSet::Text.cpu("cpu", 25.4), "cpu 25%");
        assert_eq!(IconSet::Text.cpu("cpu", 25.5), "cpu 26%");
        // Out of range is reported honestly rather than clamped — a CPU sum can
        // legitimately land past 100 — and must never panic.
        assert_eq!(IconSet::Text.cpu("cpu", -3.0), "cpu -3%");
        assert_eq!(IconSet::Unicode.cpu("cpu", 150.0), "cpu █150%");
        assert_eq!(IconSet::Text.cpu("cpu", f64::NAN), "cpu 0%");
        assert_eq!(IconSet::Text.cpu("cpu", 1e30), "cpu 9223372036854775807%");
    }

    // ---- charge state ----------------------------------------------------------

    #[test]
    fn every_tier_renders_every_charge_state() {
        // Marks are tier-independent by design, so the expectation is one table.
        let expected = [
            (State::Charging, "+"),
            (State::Full, "="),
            (State::NotCharging, "="),
            (State::Discharging, ""),
            (State::Unknown, ""),
        ];
        for set in ALL_TIERS {
            for (state, mark) in expected {
                let line = set.battery("bat", bat(74.0, state));
                assert!(
                    line.ends_with(&format!("74%{mark}")),
                    "{set:?} / {state:?}: {line}",
                );
            }
        }
        // Spelled out once, so the shape of the whole line is pinned too.
        assert_eq!(
            IconSet::Unicode.battery("bat", bat(74.0, State::Charging)),
            "bat ▓74%+",
        );
        assert_eq!(
            IconSet::Unicode.battery("bat", bat(100.0, State::Full)),
            "bat █100%=",
        );
    }

    // ---- herdr label overrides --------------------------------------------------

    #[test]
    fn a_custom_herdr_label_wins_in_every_tier() {
        // The `[ui]` label is the documented escape hatch, so it outranks even a
        // glyph tier's picture — the glyph stays, the user's word leads.
        for set in ALL_TIERS {
            let cpu = set.cpu("CPU", 26.0);
            let ram = set.ram("MEM", 8.0);
            let battery = set.battery("PWR", bat(74.0, State::Discharging));
            assert!(cpu.starts_with("CPU "), "{set:?}: {cpu}");
            assert!(ram.starts_with("MEM "), "{set:?}: {ram}");
            assert!(battery.starts_with("PWR "), "{set:?}: {battery}");
            // The word is added, not swapped in: the tier still contributes.
            assert!(cpu.ends_with("26%"), "{set:?}: {cpu}");
        }
        assert_eq!(IconSet::Text.cpu("CPU", 26.0), "CPU 26%");
        assert_eq!(IconSet::Unicode.cpu("CPU", 26.0), "CPU ░26%");
        assert_eq!(IconSet::NerdFont.cpu("CPU", 26.0), "CPU \u{f4bc} 26%");
        assert_eq!(IconSet::Emoji.cpu("CPU", 26.0), "CPU 💻26%");
    }

    #[test]
    fn the_default_word_is_not_repeated_by_the_glyph_tiers() {
        // Handed herdr's own default, a glyph tier shows only the picture — the
        // word would be redundant with it.
        assert_eq!(IconSet::Emoji.cpu("cpu", 26.0), "💻26%");
        assert_eq!(IconSet::NerdFont.ram("ram", 8.0), "\u{efc5} 8%");
        assert_eq!(
            IconSet::Emoji.battery("bat", bat(74.0, State::Discharging)),
            "🔋74%",
        );
        // An empty label never leaves a stray leading space in any tier.
        for set in ALL_TIERS {
            let line = set.cpu("", 26.0);
            assert!(!line.starts_with(' '), "{set:?}: {line:?}");
        }
    }

    // ---- font safety: the "renders without installing a font" contract -----------

    /// Every string a tier can produce: all three metrics, every charge state,
    /// every step of both ramps, and the awkward readings around them.
    ///
    /// The font-safety test walks this *output* rather than a hand-kept list of
    /// glyphs, so a glyph added anywhere in a tier is covered the moment it is
    /// added — the guard cannot drift from the implementation.
    fn every_rendering(set: IconSet) -> Vec<String> {
        let impossible = [-1.0, -1e30, 100.5, 150.0, f64::NAN, f64::INFINITY];
        let sweep = (0..=100).map(f64::from).chain(impossible);
        let mut out = Vec::new();
        for percent in sweep {
            // Labels are herdr's defaults: a word the *user* supplies is their
            // own business, and is the one thing a tier does not control.
            out.push(set.cpu(Metric::Cpu.default_word(), percent));
            out.push(set.ram(Metric::Ram.default_word(), percent));
            for state in ALL_STATES {
                out.push(set.battery(Metric::Battery.default_word(), bat(percent, state)));
            }
        }
        out
    }

    /// Private Use Area — glyphs here mean nothing without the matching font.
    fn is_private_use(c: char) -> bool {
        (0xE000..=0xF8FF).contains(&(c as u32))
    }

    /// The non-ASCII glyphs measured to be present in *both* default mono faces
    /// (DejaVu Sans Mono and Liberation Mono), via `fc-list :charset=<cp>`.
    ///
    /// BMP-and-not-private-use is necessary but not sufficient: `⚡` U+26A1 and
    /// `▮ ▯ ▰ ▱ ▣ ▤` all clear that bar yet ship in only one of the two faces, so
    /// they render as a box for half of Linux users. Only what is on this list
    /// has actually been measured, so only what is on this list may appear in a
    /// safe tier. Extending it means re-running the `fc-list` check first.
    const VERIFIED_MONO_GLYPHS: [char; 11] =
        ['■', '□', '●', '○', '█', '░', '▒', '▓', '·', '│', '┼'];

    #[test]
    fn safe_tiers_emit_only_bmp_non_private_use_glyphs() {
        // THIS IS THE CONTRACT: `Text` and `Unicode` must render on a stock
        // system with no font installed. Anything outside the BMP is emoji or
        // astral territory (missing from the default mono faces, and
        // double-width, which shears the sidebar); anything in the Private Use
        // Area is a Nerd Font glyph that draws as a blank box.
        //
        // If this test fails, the glyph you just added is not safe — put it in
        // the `NerdFont` or `Emoji` tier instead of loosening the assert.
        for set in [IconSet::Text, IconSet::Unicode] {
            for line in every_rendering(set) {
                for c in line.chars() {
                    assert!(
                        (c as u32) <= 0xFFFF,
                        "{set:?} emits non-BMP U+{:04X} in {line:?}",
                        c as u32,
                    );
                    assert!(
                        !is_private_use(c),
                        "{set:?} emits private-use U+{:04X} in {line:?}",
                        c as u32,
                    );
                    assert!(
                        c.is_ascii() || VERIFIED_MONO_GLYPHS.contains(&c),
                        "{set:?} emits unmeasured glyph {c} (U+{:04X}) in {line:?} \
                         — only glyphs checked against both default mono faces \
                         belong in a safe tier",
                        c as u32,
                    );
                }
            }
        }
    }

    #[test]
    fn safe_tier_glyph_tables_are_safe_at_the_source() {
        // The sweep above proves the rendered output is safe; this pins the
        // tables it draws from, so a glyph that is added but not yet reachable
        // is still caught.
        let safe_glyphs = GAUGE_RAMP
            .iter()
            .map(|&(_, glyph)| glyph)
            .chain([MARK_CHARGING, MARK_STEADY]);
        for c in safe_glyphs {
            assert!((c as u32) <= 0xFFFF, "U+{:04X} is not BMP", c as u32);
            assert!(!is_private_use(c), "U+{:04X} is private use", c as u32);
            assert!(
                c.is_ascii() || VERIFIED_MONO_GLYPHS.contains(&c),
                "U+{:04X} has not been measured against the default mono faces",
                c as u32,
            );
        }
    }

    #[test]
    fn opt_in_tiers_are_out_of_the_safe_range_by_construction() {
        // Documents *why* these two tiers are opt-in rather than auto-detected:
        // by definition their glyphs cannot be there without a font install.
        for glyph in [NERD_CPU, NERD_RAM]
            .into_iter()
            .chain(NERD_BATTERY_RAMP.iter().map(|&(_, glyph)| glyph))
        {
            assert!(
                is_private_use(glyph),
                "Nerd Font glyph U+{:04X} is outside the PUA — is it really a \
                 Nerd Font codepoint?",
                glyph as u32,
            );
        }
        for glyph in [EMOJI_CPU, EMOJI_RAM, EMOJI_BATTERY] {
            assert!(
                (glyph as u32) > 0xFFFF,
                "emoji U+{:04X} is inside the BMP",
                glyph as u32,
            );
        }
    }
}
