//! `/proc` sampling and `sysconf` probes.
//!
//! Everything here is host-local: reading `/proc/<pid>/stat` for CPU jiffies and
//! parent PIDs, `/proc/<pid>/statm` for RSS, and `/proc/meminfo` for the total.
//! `sysconf` values (clock ticks, page size, CPU count) come from `libc` rather
//! than shelling out to `getconf`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Per-process sample: parent PID and cumulative CPU jiffies (utime + stime).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcEntry {
    pub ppid: u32,
    pub jiffies: u64,
}

/// Read a positive `sysconf(name)` value, falling back when it is unavailable.
fn sysconf(name: libc::c_int, fallback: u64) -> u64 {
    // SAFETY: `sysconf` is a pure query of a static system value.
    let v = unsafe { libc::sysconf(name) };
    if v > 0 {
        v as u64
    } else {
        fallback
    }
}

/// Clock ticks per second (`_SC_CLK_TCK`); `/proc` stat times are in these.
pub fn clk_tck() -> u64 {
    sysconf(libc::_SC_CLK_TCK, 100)
}

/// Bytes per memory page (`_SC_PAGESIZE`); multiplies `statm` resident pages.
pub fn page_size() -> u64 {
    sysconf(libc::_SC_PAGESIZE, 4096)
}

/// Number of online logical CPUs (`_SC_NPROCESSORS_ONLN`); normalizes CPU%.
/// At least 1.
pub fn nproc() -> u64 {
    sysconf(libc::_SC_NPROCESSORS_ONLN, 1).max(1)
}

/// Scan `/proc` once, returning `pid -> {ppid, jiffies}` for every live process.
/// Processes that vanish mid-scan are skipped.
pub fn scan_proc() -> HashMap<u32, ProcEntry> {
    let mut procs = HashMap::new();
    let dir = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return procs,
    };
    for entry in dir.flatten() {
        // Numeric directory names are pids; everything else (`self`, `meminfo`,
        // ..) fails to parse and is skipped.
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        // A process can vanish between readdir and read — ignore it.
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(proc_entry) = parse_stat(&stat) {
                procs.insert(pid, proc_entry);
            }
        }
    }
    procs
}

/// Parse one `/proc/<pid>/stat` line into a [`ProcEntry`].
///
/// `comm` (field 2) may contain spaces and parentheses, so everything after the
/// **last** `)` is taken as the space-delimited tail starting at field 3
/// (state). In that tail: index 1 = ppid (field 4), index 11 = utime (field 14),
/// index 12 = stime (field 15).
pub fn parse_stat(stat: &str) -> Option<ProcEntry> {
    // `+ 2` skips the `)` and the single space that follows it, landing on the
    // state field. Both are ASCII, so this stays on a char boundary.
    let tail = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(ProcEntry {
        ppid,
        jiffies: utime + stime,
    })
}

/// Invert a proc map into `ppid -> [child pid, ..]`.
pub fn children_map(procs: &HashMap<u32, ProcEntry>) -> HashMap<u32, Vec<u32>> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, p) in procs {
        kids.entry(p.ppid).or_default().push(pid);
    }
    kids
}

/// Every PID in `root`'s process subtree (inclusive), via the children map.
/// Iterative DFS with a visited set, so shared parents and cycles terminate.
pub fn subtree(root: u32, kids: &HashMap<u32, Vec<u32>>) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !out.insert(pid) {
            continue; // already visited — dedup
        }
        if let Some(children) = kids.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    out
}

/// Parse `MemTotal` (kB) out of `/proc/meminfo` text and convert to MB.
fn parse_mem_total_mb(meminfo: &str) -> Option<f64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

/// Total system RAM in MB from `/proc/meminfo` `MemTotal` (0 if unreadable).
/// Read once and cached for the process lifetime.
pub fn mem_total_mb() -> f64 {
    static MEM_TOTAL_MB: OnceLock<f64> = OnceLock::new();
    *MEM_TOTAL_MB.get_or_init(|| {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| parse_mem_total_mb(&text))
            .unwrap_or(0.0)
    })
}

/// Render `mb` as a whole-percent-of-`total_mb` string, or `""` when unknown.
fn pct_string(mb: f64, total_mb: f64) -> String {
    if total_mb > 0.0 {
        // `round()` is half-away-from-zero, which for these non-negative
        // values is ordinary half-up rounding.
        format!("{}%", (100.0 * mb / total_mb).round() as i64)
    } else {
        String::new()
    }
}

/// `"<n>%"` of total system RAM for `mb`, or `""` if the total is unknown.
pub fn ram_pct(mb: f64) -> String {
    pct_string(mb, mem_total_mb())
}

/// Sum of RSS (MB) across `pids`, reading `/proc/<pid>/statm` field 2 (resident
/// pages) × page size. Vanished pids contribute nothing.
pub fn rss_mb(pids: &HashSet<u32>) -> f64 {
    let page = page_size();
    let mut bytes: u64 = 0;
    for &pid in pids {
        if let Ok(statm) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
            if let Some(resident) = statm
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
            {
                bytes += resident * page;
            }
        }
    }
    bytes as f64 / (1024.0 * 1024.0)
}

/// Executable name of `pid` from `/proc/<pid>/comm`, or `None` when the process
/// is gone or unreadable. Used by the daemon's stale-pid-file guard; also
/// doubles as the liveness probe (a vanished pid has no `comm` to read).
pub fn process_image_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|text| text.trim().to_string())
}

/// Best-effort graceful stop of `pid` via SIGTERM (failure is ignored — the
/// daemon's statuses self-clear via their TTL either way).
pub fn stop_process(pid: u32) {
    // SAFETY: `kill` merely posts SIGTERM to the pid.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_handles_comm_with_spaces_and_parens() {
        // comm = "weird (name) proc" — must slice after the LAST ')'.
        // Tail fields (0-based): [0]=state S, [1]=ppid 100, .. [11]=utime 42,
        // [12]=stime 8.
        let stat = "1234 (weird (name) proc) S 100 1234 1234 0 -1 \
                    4194560 12345 0 0 0 42 8 0 0 20 0 1 0 987654 1234567 890";
        let entry = parse_stat(stat).expect("parseable stat line");
        assert_eq!(entry.ppid, 100);
        assert_eq!(entry.jiffies, 50); // 42 + 8
    }

    #[test]
    fn parse_stat_plain_comm() {
        // No parens/spaces in comm — the common case.
        let stat = "9 (bash) S 7 9 9 0 -1 4194304 100 0 0 0 11 4 0 0 20 0 1 0 5";
        let entry = parse_stat(stat).expect("parseable stat line");
        assert_eq!(entry.ppid, 7);
        assert_eq!(entry.jiffies, 15); // 11 + 4
    }

    #[test]
    fn subtree_walks_descendants_with_dedup_and_cycle_safety() {
        // Diamond (4 reachable via both 2 and 3) exercises dedup; the 4 -> 1
        // back-edge is a cycle that must still terminate.
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        kids.insert(1, vec![2, 3]);
        kids.insert(2, vec![4]);
        kids.insert(3, vec![4]);
        kids.insert(4, vec![1]);
        kids.insert(999, vec![6]); // unrelated tree — must not be pulled in

        let got = subtree(1, &kids);
        assert_eq!(got, HashSet::from([1, 2, 3, 4]));
    }

    #[test]
    fn children_map_then_subtree_over_synthetic_procs() {
        let procs: HashMap<u32, ProcEntry> = [
            (1, 0),   // root
            (2, 1),   // child of 1
            (3, 1),   // child of 1
            (4, 2),   // grandchild
            (5, 4),   // great-grandchild
            (6, 999), // unrelated subtree
        ]
        .into_iter()
        .map(|(pid, ppid)| (pid, ProcEntry { ppid, jiffies: 0 }))
        .collect();

        let kids = children_map(&procs);
        assert_eq!(subtree(1, &kids), HashSet::from([1, 2, 3, 4, 5]));
    }

    #[test]
    fn ram_pct_math_rounds_and_guards_zero_total() {
        // 100 * 1024 / 16384 = 6.25 -> 6
        assert_eq!(pct_string(1024.0, 16384.0), "6%");
        // 100 * 250 / 10000 = 2.5 -> 3 (half away from zero)
        assert_eq!(pct_string(250.0, 10000.0), "3%");
        // full machine
        assert_eq!(pct_string(16384.0, 16384.0), "100%");
        // unknown total -> empty string
        assert_eq!(pct_string(512.0, 0.0), "");
    }

    #[test]
    fn parse_mem_total_extracts_kb_and_divides() {
        let meminfo = "MemTotal:       16384 kB\nMemFree:         1024 kB\n";
        assert_eq!(parse_mem_total_mb(meminfo), Some(16.0)); // 16384 / 1024
        assert_eq!(parse_mem_total_mb("MemFree: 10 kB\n"), None);
    }
}
