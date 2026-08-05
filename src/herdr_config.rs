//! Making herdr's own sidebar actually render the token this plugin pushes.
//!
//! The daemon reports a named `usage` metadata token, but herdr only draws a
//! `$name` token that some row in ITS `config.toml` references — and herdr's
//! built-in rows reference none. So on a fresh install every layer works and the
//! sidebar still shows nothing, which is exactly what a new user reports as "the
//! plugin is broken".
//!
//! There is no API and no manifest section for this: the plugin manifest has
//! startup / action / event / pane / link-handler / build sections and nothing
//! that contributes UI, and the request schema (protocol 19) has no config
//! writer. Editing the user's own config is the only mechanism that exists, so
//! this module does it as carefully as a program can:
//!
//! - **guarded** — never touches a table that already references `$usage`;
//! - **marked** — everything written is wrapped in a comment block, so the file
//!   itself records what came from us and removal needs no side-state that could
//!   drift from reality;
//! - **reversible** — `status-disable` takes the block back out;
//! - **recoverable** — a `.bak` alongside the original before every write, and
//!   the write itself is a temp-file rename so a crash cannot truncate a config.

use std::path::{Path, PathBuf};

use crate::config::{self, Mode};

/// Opening line of a block this plugin wrote into herdr's config.
const BEGIN: &str = "# --- added by ez-corp.space-usage (removed by `status-disable`) ---";
/// Closing line of that block.
const END: &str = "# --- end ez-corp.space-usage ---";

/// The row the daemon needs herdr to draw, as it appears in a `rows` array.
const USAGE_ROW: &str = r#"["$usage"],"#;

/// The token itself, used to detect a row the user already wrote by hand.
const USAGE_TOKEN: &str = "$usage";

/// What [`ensure_usage_row`] or [`remove_usage_row`] did to herdr's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The config already said what it needed to; nothing was written.
    Unchanged,
    /// The config was rewritten.
    Written,
}

impl Change {
    /// Whether herdr needs to re-read its config for this to take effect.
    pub fn needs_reload(self) -> bool {
        self == Change::Written
    }
}

/// Make sure herdr's config has a row rendering our `$usage` token for `mode`.
///
/// A no-op when the mode's table already references the token — including when
/// the user wrote it themselves, which is the common case for anyone who set
/// this up before 1.8.0.
pub fn ensure_usage_row(mode: Mode) -> crate::Result<Change> {
    let path = config::herdr_config_path();
    // An absent config is not an error: herdr writes one on first run, but the
    // plugin can be installed before that ever happens. Starting from empty text
    // makes that case fall out of the same code path as every other.
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    match with_usage_row(&text, mode) {
        None => Ok(Change::Unchanged),
        Some(updated) => {
            write_config(&path, &text, &updated)?;
            Ok(Change::Written)
        }
    }
}

/// Take our marked block back out of herdr's config.
///
/// Only removes what carries our markers, so a row the user wrote by hand — or
/// moved out of our block — survives. That asymmetry is deliberate: we undo our
/// own edit, not the user's.
pub fn remove_usage_row() -> crate::Result<Change> {
    let path = config::herdr_config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Change::Unchanged);
    };
    match without_marked_blocks(&text) {
        None => Ok(Change::Unchanged),
        Some(updated) => {
            write_config(&path, &text, &updated)?;
            Ok(Change::Written)
        }
    }
}

/// Back up `original`, then replace `path` with `updated` atomically.
///
/// The backup is written before the config so an interrupted run leaves the user
/// with a copy of what they had; the rename means the config itself is only ever
/// the old file or the new one, never a half-written mixture.
fn write_config(path: &Path, original: &str, updated: &str) -> crate::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Only back up a config that existed. Writing a `.bak` of "" for a file we
    // are creating would leave a confusing empty artefact behind.
    if !original.is_empty() {
        std::fs::write(backup_path(path), original)?;
    }
    let temp = path.with_extension("toml.space-usage-tmp");
    std::fs::write(&temp, updated)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Where the pre-edit copy of herdr's config is kept.
///
/// Plugin-scoped in the name so it cannot be mistaken for — or collide with — a
/// backup herdr or the user made.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".space-usage.bak");
    path.with_file_name(name)
}

// ---- pure text edits --------------------------------------------------------

/// herdr's config text with a `$usage` row added for `mode`, or `None` when the
/// mode's table already references the token.
///
/// Three shapes, in the order a real config runs into them:
///   1. no such table — append the whole table, herdr's own default rows plus
///      ours, so adding usage does not silently drop `branch` / `git_status`;
///   2. table with a `rows` array — insert our row just inside its closing `]`;
///   3. table without one — write a fresh `rows` array under the header.
fn with_usage_row(text: &str, mode: Mode) -> Option<String> {
    let (table, default_rows) = mode.sidebar_table();
    let lines: Vec<&str> = text.split('\n').collect();

    let Some(header) = table_header_line(&lines, table) else {
        return Some(append_table(text, table, default_rows));
    };
    let body = header + 1..next_table_line(&lines, header);
    if lines[body.clone()]
        .iter()
        .any(|line| strip_comment(line).contains(USAGE_TOKEN))
    {
        return None; // already referenced — the user's row, or ours from before
    }

    let mut out: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    match rows_close(&lines, body.clone()) {
        Rows::Closes(line, col) => splice_into_array(&mut out, line, col),
        Rows::Missing => {
            let block = marked_block("", &[rows_array(default_rows)]);
            out.splice(body.start..body.start, block);
        }
        // A `rows` key whose array never closes is a config that does not parse.
        // Adding a second `rows` under it would turn a syntax error the user can
        // find into a duplicate-key error they cannot.
        Rows::Unterminated => return None,
    }
    Some(out.join("\n"))
}

/// herdr's config text with every block this plugin wrote removed, or `None`
/// when it wrote none.
///
/// Reversible in content, not always byte for byte. Splitting a one-line
/// `rows = [[..], [..]]` leaves the closing `]` on its own line and a trailing
/// comma on the row above once our block is gone — both valid TOML, and the same
/// rows in the same order. Reflowing the user's formatting back is not worth the
/// risk of a rewrite that guesses wrong; every other shape does round trip
/// exactly.
///
/// Tolerates an unterminated block (a half-hand-edited config) by removing to
/// the end of the file rather than leaving a stray marker behind — a dangling
/// `# --- added by ...` header with no end is not something a user can act on.
fn without_marked_blocks(text: &str) -> Option<String> {
    if !text.contains(BEGIN) {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed == BEGIN {
            inside = true;
        } else if inside {
            inside = trimmed != END;
        } else {
            out.push(line);
        }
    }
    // Removing a whole appended table leaves the blank line that preceded it, so
    // collapse a run of trailing blanks back to a single terminating newline.
    while out.len() > 1 && out[out.len() - 1].trim().is_empty() {
        out.pop();
    }
    out.push("");
    Some(out.join("\n"))
}

/// Index of the line declaring table `name`.
fn table_header_line(lines: &[&str], name: &str) -> Option<usize> {
    lines.iter().position(|line| table_name(line) == Some(name))
}

/// Index one past the end of the table body starting at `from` — the next table
/// header, or the end of the file.
fn next_table_line(lines: &[&str], from: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(from + 1)
        .find(|(_, line)| table_name(line).is_some())
        .map(|(i, _)| i)
        .unwrap_or(lines.len())
}

/// The table `line` declares, or `None` if it declares none.
///
/// Deliberately stricter than [`config::section_name`], which only asks whether a
/// line opens with `[`. Row lines open with `[` too — `["state_icon", "workspace"]`
/// read as a table called `"state_icon", "workspace"`, which ended the table body
/// at its first row. The guard against double-adding then never saw the `$usage`
/// row it had written a moment earlier, so every run added another one.
///
/// A header is the whole line: `[`, a bare dotted key, `]`, and nothing but an
/// optional comment after it. Quotes, commas and `$` are what tell a row apart.
fn table_name(line: &str) -> Option<&str> {
    let line = strip_comment(line).trim();
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    let bare = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    (!inner.is_empty() && inner.chars().all(bare)).then_some(inner)
}

/// Where the `rows` array inside a table body ends.
enum Rows {
    /// The table declares no `rows` key.
    Missing,
    /// It declares one whose array never closes — a config that does not parse.
    Unterminated,
    /// `(line, column)` of the `]` that closes it.
    Closes(usize, usize),
}

/// Locate the end of the `rows` array inside `body`.
///
/// Scanning starts at the `rows` line rather than the table header so neither
/// the header's own brackets nor an earlier array-valued key (`rows_by_agent`,
/// say) can be mistaken for it.
fn rows_close(lines: &[&str], body: std::ops::Range<usize>) -> Rows {
    let Some(offset) = lines[body.clone()]
        .iter()
        .position(|line| is_rows_key(line))
    else {
        return Rows::Missing;
    };
    let start = body.start + offset;
    let mut depth = 0i32;
    for (offset, line) in lines[start..body.end].iter().enumerate() {
        if let Some(col) = scan_brackets(line, &mut depth) {
            return Rows::Closes(start + offset, col);
        }
    }
    Rows::Unterminated
}

/// Whether `line` declares the `rows` key (and not `rows_by_agent` or similar).
fn is_rows_key(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim() == "rows")
}

/// Track bracket depth across one line, returning the column of the `]` that
/// brings it back to zero.
///
/// String- and comment-aware, because a `#` or a `]` inside a quoted token style
/// (`{ token = "$git_dirty", fg = "#f9e2af" }`) is data, not syntax — the colour
/// hexes in a real herdr config are full of both.
fn scan_brackets(line: &str, depth: &mut i32) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (col, ch) in line.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(ch),
            (None, '#') => break, // comment: nothing after it is syntax
            (None, '[') => *depth += 1,
            (None, ']') => {
                *depth -= 1;
                if *depth == 0 {
                    return Some(col);
                }
            }
            _ => {}
        }
    }
    None
}

/// Insert our marked row just inside the `]` at `(line, col)`.
///
/// Splits the closing line when the `]` shares it with array content — herdr's
/// own documented example is a single-line `rows = [[...], [...]]`, and a
/// comment cannot be injected inline there without commenting out the rest of
/// the array. Splitting is also what lets one marker pair serve every shape.
fn splice_into_array(lines: &mut Vec<String>, line: usize, col: usize) {
    let closing = lines[line].clone();
    let indent = leading_space(&closing).to_string();
    let (head, tail) = closing.split_at(col);
    let mut block = marked_block(&format!("{indent}  "), &[USAGE_ROW.to_string()]);

    if head.trim().is_empty() {
        // The `]` already sits on its own line: leave it alone and put the block
        // above it.
        lines.splice(line..line, block);
        return;
    }
    // `["a"], ["b"]]` — the row before ours needs the separator an array written
    // on one line does not carry at its end.
    let head = head.trim_end();
    let separated = match head.ends_with(',') || head.ends_with('[') {
        true => head.to_string(),
        false => format!("{head},"),
    };
    block.insert(0, separated);
    block.push(format!("{indent}{tail}"));
    lines.splice(line..=line, block);
}

/// A whole `[table]` with herdr's default rows plus ours, appended to `text`.
fn append_table(text: &str, table: &str, default_rows: &[&str]) -> String {
    let mut body = vec![format!("[{table}]"), rows_array(default_rows)];
    body = marked_block("", &body);

    let mut out = text.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&body.join("\n"));
    out.push('\n');
    out
}

/// A `rows = [...]` array holding `default_rows` and, last, our usage row.
fn rows_array(default_rows: &[&str]) -> String {
    let mut out = String::from("rows = [\n");
    for row in default_rows {
        out.push_str(&format!("  {row},\n"));
    }
    out.push_str(&format!("  {USAGE_ROW}\n]"));
    out
}

/// Wrap `body` in this plugin's begin/end markers at `indent`.
///
/// Every write goes through here, so there is exactly one description of what
/// our edits look like — which is what makes [`without_marked_blocks`] a
/// complete undo rather than a best guess.
fn marked_block(indent: &str, body: &[String]) -> Vec<String> {
    let mut out = vec![format!("{indent}{BEGIN}")];
    out.extend(body.iter().flat_map(|chunk| {
        chunk
            .split('\n')
            .map(|line| format!("{indent}{line}"))
            .collect::<Vec<_>>()
    }));
    out.push(format!("{indent}{END}"));
    out
}

/// `line` up to its first `#` outside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (col, ch) in line.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(ch),
            (None, '#') => return &line[..col],
            _ => {}
        }
    }
    line
}

/// The whitespace `line` starts with.
fn leading_space(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// herdr's built-in spaces rows, as `herdr --default-config` documents them
    /// — one line, which is the shape a user gets by uncommenting that example.
    const HERDR_DEFAULT: &str = "\
[ui]
sidebar_width = 32

[ui.sidebar.spaces]
rows = [[\"state_icon\", \"workspace\"], [\"branch\", \"git_status\"]]

[ui.toast]
delivery = \"off\"
";

    /// Every `$usage` reference in `text`, ignoring commented-out ones.
    fn usage_rows(text: &str) -> usize {
        text.split('\n')
            .filter(|line| strip_comment(line).contains(USAGE_TOKEN))
            .count()
    }

    // ---- adding the row ------------------------------------------------------

    #[test]
    fn a_default_herdr_config_gains_exactly_one_usage_row() {
        // The bug this whole module exists for: herdr's built-in rows reference
        // no `$usage`, so the daemon pushes a token nothing draws.
        assert_eq!(usage_rows(HERDR_DEFAULT), 0);
        let out = with_usage_row(HERDR_DEFAULT, Mode::Sidebar).expect("must add a row");
        assert_eq!(usage_rows(&out), 1);
        // The rows that were already there survive — adding usage must not cost
        // the user their branch and git status.
        assert!(out.contains("state_icon"), "{out}");
        assert!(out.contains("git_status"), "{out}");
        // And the tables around it are untouched.
        assert!(out.contains("[ui.toast]"), "{out}");
    }

    #[test]
    fn a_single_line_array_is_split_and_keeps_its_separators() {
        // herdr documents `rows = [[..], [..]]` on one line. A comment cannot go
        // inline there, and the last row carries no trailing comma, so both the
        // split and the comma have to be got right or the config stops parsing.
        let out = with_usage_row(HERDR_DEFAULT, Mode::Sidebar).unwrap();
        let rows: Vec<&str> = out
            .split('\n')
            .skip_while(|line| !line.contains("rows = ["))
            .take_while(|line| !line.trim().starts_with(']'))
            .collect();
        assert!(
            rows[0].trim_end().ends_with(','),
            "the row before ours needs a separator: {:?}",
            rows[0],
        );
        assert!(out.contains(&format!("  {USAGE_ROW}")), "{out}");
    }

    #[test]
    fn a_multi_line_array_keeps_its_closing_bracket_on_its_own_line() {
        let text = "\
[ui.sidebar.spaces]
rows = [
  [\"state_icon\", \"workspace\"],
  [\"branch\", \"git_status\"],
]
";
        let out = with_usage_row(text, Mode::Sidebar).unwrap();
        assert_eq!(usage_rows(&out), 1);
        // The bracket is still alone on its line, indented as it was.
        assert!(out.contains("\n]\n"), "{out}");
        assert_eq!(out.matches('[').count(), text.matches('[').count() + 1);
    }

    #[test]
    fn an_absent_table_is_appended_whole_with_herdrs_own_defaults() {
        // Appending only `rows = [["$usage"]]` would silently replace herdr's
        // built-in rows and cost the user their workspace name and branch.
        let out = with_usage_row("[ui]\nsidebar_width = 32\n", Mode::Sidebar).unwrap();
        assert!(out.contains("[ui.sidebar.spaces]"), "{out}");
        assert!(out.contains("state_icon"), "{out}");
        assert!(out.contains("git_status"), "{out}");
        assert_eq!(usage_rows(&out), 1);
        // The table we appended must not duplicate one already declared.
        assert_eq!(out.matches("[ui.sidebar.spaces]").count(), 1, "{out}");
    }

    #[test]
    fn an_empty_config_is_created_rather_than_skipped() {
        // The plugin can be installed before herdr has ever written its config.
        let out = with_usage_row("", Mode::Sidebar).unwrap();
        assert!(out.starts_with(BEGIN), "{out}");
        assert_eq!(usage_rows(&out), 1);
    }

    #[test]
    fn a_table_without_a_rows_key_gets_one() {
        let out = with_usage_row("[ui.sidebar.spaces]\nrow_gap = 1\n", Mode::Sidebar).unwrap();
        assert_eq!(usage_rows(&out), 1);
        assert!(out.contains("row_gap = 1"), "{out}");
        assert!(out.contains("rows = ["), "{out}");
    }

    #[test]
    fn agents_panel_mode_edits_the_agents_table_instead() {
        // The two modes render from different tables, so writing the row into
        // the wrong one looks exactly like the bug we are fixing.
        let out = with_usage_row(HERDR_DEFAULT, Mode::AgentsPanel).unwrap();
        assert!(out.contains("[ui.sidebar.agents]"), "{out}");
        // The spaces table it did NOT ask for stays as it was.
        assert!(
            out.contains("rows = [[\"state_icon\", \"workspace\"], [\"branch\", \"git_status\"]]"),
            "{out}",
        );
    }

    // ---- the guard -----------------------------------------------------------

    #[test]
    fn a_config_that_already_draws_usage_is_left_alone() {
        // Anyone who set this up before 1.8.0 wrote the row themselves. Adding a
        // second one would draw the reading twice on every space card.
        let text = "[ui.sidebar.spaces]\nrows = [[\"workspace\"], [\"$usage\"]]\n";
        assert_eq!(with_usage_row(text, Mode::Sidebar), None);
    }

    #[test]
    fn running_it_twice_changes_nothing_the_second_time() {
        let once = with_usage_row(HERDR_DEFAULT, Mode::Sidebar).unwrap();
        assert_eq!(with_usage_row(&once, Mode::Sidebar), None);
    }

    #[test]
    fn a_commented_out_usage_row_does_not_count_as_present() {
        // A user who commented ours out and forgot is back to a blank sidebar;
        // treating that as "already there" would leave them stuck.
        let text = "[ui.sidebar.spaces]\nrows = [\n  [\"workspace\"],\n  # [\"$usage\"],\n]\n";
        let out = with_usage_row(text, Mode::Sidebar).expect("must add a live row");
        assert_eq!(usage_rows(&out), 1);
    }

    #[test]
    fn a_usage_row_in_the_other_table_does_not_count() {
        // agents-panel users have `$usage` under `[ui.sidebar.agents]`. Switching
        // to sidebar mode must still add it to the spaces table.
        let text = "\
[ui.sidebar.agents]
rows = [[\"agent\", \"$usage\"]]

[ui.sidebar.spaces]
rows = [[\"workspace\"]]
";
        let out = with_usage_row(text, Mode::Sidebar).expect("must add a spaces row");
        assert_eq!(usage_rows(&out), 2);
    }

    #[test]
    fn a_commented_out_table_header_is_not_the_table() {
        // herdr ships `# [ui.sidebar.spaces]` as a template. Matching it would
        // append rows under a comment, where nothing renders them.
        let out = with_usage_row("# [ui.sidebar.spaces]\n# rows = []\n", Mode::Sidebar).unwrap();
        assert!(
            out.split('\n')
                .any(|line| line.trim() == "[ui.sidebar.spaces]"),
            "a real table header must be written: {out}",
        );
    }

    #[test]
    fn an_unterminated_rows_array_is_left_alone() {
        // A config that does not parse is the user's to fix; guessing where the
        // array was meant to end could corrupt it further.
        let text = "[ui.sidebar.spaces]\nrows = [\n  [\"workspace\"],\n";
        assert_eq!(with_usage_row(text, Mode::Sidebar), None);
    }

    // ---- removing it ---------------------------------------------------------

    #[test]
    fn removal_restores_the_config_byte_for_byte() {
        // What we add, we take back off — no marker, no blank line, no
        // reformatting left behind. Every shape except the one-line array, which
        // has to be split to take a comment and cannot be reflowed back.
        for original in [
            "[ui]\nsidebar_width = 32\n",
            "[ui.sidebar.spaces]\nrows = [\n  [\"branch\"],\n]\n",
            "[ui.sidebar.spaces]\nrow_gap = 1\n",
        ] {
            let added = with_usage_row(original, Mode::Sidebar).expect("adds");
            let removed = without_marked_blocks(&added).expect("removes");
            assert_eq!(removed, original, "round trip changed the file");
        }
    }

    #[test]
    fn removal_after_a_split_leaves_the_same_rows_and_no_trace_of_us() {
        // The one shape that cannot round trip byte for byte. What must still
        // hold: our row is gone, our markers are gone, the rows the user had are
        // all still there in order, and the result is somewhere we can add to
        // again — i.e. enable/disable/enable is not a one-way door.
        let added = with_usage_row(HERDR_DEFAULT, Mode::Sidebar).expect("adds");
        let removed = without_marked_blocks(&added).expect("removes");

        assert_eq!(usage_rows(&removed), 0, "{removed}");
        assert!(
            !removed.contains(BEGIN) && !removed.contains(END),
            "{removed}"
        );
        assert!(
            removed.contains(r#"["state_icon", "workspace"]"#),
            "{removed}"
        );
        assert!(removed.contains(r#"["branch", "git_status"]"#), "{removed}");
        assert_eq!(removed.matches("rows = [").count(), 1, "{removed}");
        // Re-enabling puts exactly one row back.
        let again = with_usage_row(&removed, Mode::Sidebar).expect("adds again");
        assert_eq!(usage_rows(&again), 1, "{again}");
    }

    #[test]
    fn removal_leaves_a_hand_written_usage_row_in_place() {
        // We undo our own edit, not the user's. A row they wrote carries no
        // marker and must survive `status-disable`.
        let text = "[ui.sidebar.spaces]\nrows = [[\"workspace\"], [\"$usage\"]]\n";
        assert_eq!(without_marked_blocks(text), None);
    }

    #[test]
    fn removal_of_an_unterminated_block_takes_the_marker_with_it() {
        // Half-hand-edited: a dangling begin marker with no end. Leaving it would
        // strand a comment the user cannot act on.
        let text = format!("[ui]\nx = 1\n{BEGIN}\nrows = [\n");
        let out = without_marked_blocks(&text).expect("removes");
        assert!(!out.contains(BEGIN), "{out}");
        assert_eq!(out, "[ui]\nx = 1\n");
    }

    #[test]
    fn removal_is_a_no_op_on_a_config_we_never_touched() {
        assert_eq!(without_marked_blocks(HERDR_DEFAULT), None);
    }

    // ---- the pieces ----------------------------------------------------------

    #[test]
    fn bracket_scanning_ignores_brackets_inside_strings_and_comments() {
        // A real herdr config styles tokens with colour hexes — `#f9e2af` inside
        // quotes is data, and a `#` outside them starts a comment. Both used to
        // be able to end the scan on the wrong character.
        let line = r##"  { token = "$git_dirty", fg = "#f9e2af" }, # ] not this one"##;
        let mut depth = 1;
        assert_eq!(scan_brackets(line, &mut depth), None);
        assert_eq!(depth, 1);

        let mut depth = 0;
        assert_eq!(scan_brackets("rows = [[\"a\"]]", &mut depth), Some(13));
    }

    #[test]
    fn the_rows_key_is_matched_exactly() {
        assert!(is_rows_key("rows = ["));
        assert!(is_rows_key("  rows=[ "));
        // `rows_by_agent` is a different key holding a different array; scanning
        // from it would find the wrong closing bracket.
        assert!(!is_rows_key("rows_by_agent = {}"));
        assert!(!is_rows_key("row_gap = 0"));
    }

    #[test]
    fn strip_comment_keeps_a_hash_inside_quotes() {
        assert_eq!(
            strip_comment(r##"fg = "#f9e2af" # note"##),
            r##"fg = "#f9e2af" "##,
        );
        assert_eq!(strip_comment("plain"), "plain");
        assert_eq!(strip_comment("# whole line").trim(), "");
    }

    #[test]
    fn the_backup_is_plugin_scoped() {
        // A bare `.bak` would collide with one herdr or the user made.
        let backup = backup_path(Path::new("/home/x/.config/herdr/config.toml"));
        assert_eq!(
            backup,
            Path::new("/home/x/.config/herdr/config.toml.space-usage.bak"),
        );
    }
}
