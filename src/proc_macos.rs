//! macOS process sampling — the `proc` module twin of the Linux `/proc` reader.
//!
//! Same public surface as `proc.rs`, different probes: the process table comes
//! from `proc_listallpids`, parent PIDs and the executable name from
//! `proc_pidinfo(PROC_PIDTBSDINFO)`, cumulative CPU time and RSS from
//! `proc_pidinfo(PROC_PIDTASKINFO)`, and the machine total from
//! `sysctl hw.memsize`. Processes whose *times* we cannot read (other users,
//! platform-binary protections) still contribute their pid/ppid edge — the
//! subtree walk needs the topology even when the counters are unreadable — but
//! sample as zero CPU/RSS, which is correct for panes we own: a herdr pane's
//! subtree runs as the current user.
//!
//! **CPU units.** `proc_taskinfo` reports nanoseconds, not clock ticks, so
//! [`ProcEntry::jiffies`] holds nanoseconds and [`clk_tck`] returns 10^9. That
//! is the whole trick that lets the three backends share one formula: the maths
//! in `collect::measure` is `Δjiffies / clk_tck() / elapsed_s / nproc()`, so any
//! backend may pick its own unit as long as `clk_tck()` names it. Linux uses
//! `_SC_CLK_TCK` jiffies (100), Windows 100 ns FILETIME ticks (10^7).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Per-process sample: parent PID and cumulative CPU time in [`clk_tck`] units
/// (here nanoseconds — see the module docs; the Linux twin uses jiffies).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcEntry {
    pub ppid: u32,
    pub jiffies: u64,
}

/// CPU-time units per second: `proc_taskinfo` totals are in nanoseconds.
pub fn clk_tck() -> u64 {
    1_000_000_000
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

/// Number of online logical CPUs (`_SC_NPROCESSORS_ONLN`); normalizes CPU%.
/// At least 1.
pub fn nproc() -> u64 {
    sysconf(libc::_SC_NPROCESSORS_ONLN, 1).max(1)
}

/// Slack added to the pid buffer so a fork racing the sizing call still fits.
/// The kernel already pads its own answer by 20; this is belt and braces.
const PID_HEADROOM: usize = 32;

/// How many times to grow-and-retry when the pid buffer comes back full.
const PID_LIST_ATTEMPTS: usize = 4;

/// Every live pid, via `proc_listallpids`.
///
/// Called with a null buffer it answers "how many pids?" (the kernel pads that
/// count by 20 itself); called with a buffer it fills it and returns how many
/// it wrote. Both are counts, not byte lengths — `proc_listallpids` divides the
/// kernel's byte count by `sizeof(int)` for us. A buffer that comes back
/// completely full may have been truncated, so we grow and ask again.
fn list_pids() -> Vec<u32> {
    let mut capacity = 0usize;
    for _ in 0..PID_LIST_ATTEMPTS {
        // SAFETY: a null buffer with size 0 is the documented sizing query.
        let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        capacity = capacity.max(needed as usize + PID_HEADROOM);

        let mut pids = vec![0 as libc::c_int; capacity];
        let bytes = (capacity * std::mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: `pids` owns `capacity` c_ints and `bytes` is exactly that
        // span in bytes, so the kernel cannot write past the allocation.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if count <= 0 {
            return Vec::new();
        }
        let count = count as usize;
        if count >= capacity {
            capacity *= 2; // filled to the brim — assume truncation and retry
            continue;
        }
        pids.truncate(count);
        // pid 0 is `kernel_task`, a pseudo-process and not a real tree root
        // (launchd is pid 1, parented to 0), so drop it like the Windows twin
        // drops System Idle.
        return pids
            .into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| pid as u32)
            .collect();
    }
    Vec::new()
}

/// Fetch one `proc_pidinfo` flavor for `pid`, or `None` when it is unavailable.
///
/// `proc_pidinfo` returns the number of bytes it wrote, or <= 0 on failure —
/// EPERM for a process we may not inspect, ESRCH once it exits. A short write
/// leaves the tail of the struct zeroed, which would read back as a perfectly
/// plausible "0 ns of CPU, 0 bytes resident" sample, so anything less than a
/// full-size write is treated as failure rather than as data.
///
/// # Safety
///
/// `T` must be the plain-old-data FFI struct that `flavor` fills, and one for
/// which an all-zero bit pattern is a valid value.
unsafe fn pid_info<T>(pid: u32, flavor: libc::c_int) -> Option<T> {
    let mut info: T = std::mem::zeroed();
    let size = std::mem::size_of::<T>() as libc::c_int;
    let written = libc::proc_pidinfo(
        pid as libc::c_int,
        flavor,
        0,
        (&mut info as *mut T).cast(),
        size,
    );
    (written == size).then_some(info)
}

/// Cumulative user+system CPU nanoseconds and RSS bytes for `pid`.
fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
    // SAFETY: `proc_taskinfo` is the POD struct PROC_PIDTASKINFO fills.
    unsafe { pid_info::<libc::proc_taskinfo>(pid, libc::PROC_PIDTASKINFO) }
}

/// Parent pid and executable name for `pid`.
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: `proc_bsdinfo` is the POD struct PROC_PIDTBSDINFO fills.
    unsafe { pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) }
}

/// Snapshot the process table once, returning `pid -> {ppid, cpu ns}` for every
/// live process. Processes whose times are unreadable keep their pid/ppid edge
/// (the subtree walk needs it) with zero CPU; processes that vanish mid-scan,
/// or whose parent link we cannot read at all, are skipped.
pub fn scan_proc() -> HashMap<u32, ProcEntry> {
    let mut procs = HashMap::new();
    for pid in list_pids() {
        // No ppid means no edge worth recording — the pid exited between the
        // listing and now, or is one we may not inspect.
        let Some(bsd) = bsd_info(pid) else {
            continue;
        };
        // Times, unlike the parent link, are optional: dropping an intermediate
        // node whose counters we cannot read would orphan every descendant
        // below it and under-report the workspace.
        let jiffies = task_info(pid)
            .map(|ti| ti.pti_total_user + ti.pti_total_system)
            .unwrap_or(0);
        procs.insert(
            pid,
            ProcEntry {
                ppid: bsd.pbi_ppid,
                jiffies,
            },
        );
    }
    procs
}

/// A fixed-width C string field as a `String`, stopping at the first NUL — and
/// at the end of the field if the kernel filled every byte without one.
fn c_str_field(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Pick the executable name out of a `proc_bsdinfo`'s two name fields.
///
/// `pbi_name` is the 31-char `p_name`, `pbi_comm` the 15-char `p_comm`.
/// `p_name` is empty for a process that never exec'd, so fall back to `p_comm`
/// — the same order `libproc`'s own `proc_name()` uses. `p_comm` is the exact
/// analogue of Linux `/proc/<pid>/comm`, which is what that backend returns.
/// Both empty reads as `None`: an empty name would compare equal to another
/// empty name, and the daemon's stale-pid guard compares names.
fn image_name(name: &[libc::c_char], comm: &[libc::c_char]) -> Option<String> {
    let name = c_str_field(name);
    if !name.is_empty() {
        return Some(name);
    }
    let comm = c_str_field(comm);
    if comm.is_empty() {
        None
    } else {
        Some(comm)
    }
}

/// Executable name of `pid`, or `None` when the process is gone or unreadable.
/// Used by the daemon's stale-pid-file guard; also doubles as the liveness
/// probe (a vanished pid has no bsdinfo to read).
pub fn process_image_name(pid: u32) -> Option<String> {
    let info = bsd_info(pid)?;
    image_name(&info.pbi_name, &info.pbi_comm)
}

/// Best-effort graceful stop of `pid` via SIGTERM (failure is ignored — the
/// daemon's statuses self-clear via their TTL either way).
pub fn stop_process(pid: u32) {
    // SAFETY: `kill` merely posts SIGTERM to the pid.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
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

/// Bytes to MB, matching the Linux twin's kB/1024 (i.e. MiB, as every RAM
/// readout on these platforms means).
fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Physical RAM in bytes from `sysctl hw.memsize`, or `None` if the call fails.
fn hw_memsize() -> Option<u64> {
    let mut mib: [libc::c_int; 2] = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `mib` is a 2-entry MIB matching the `namelen` we pass; `bytes`
    // and `len` are a correctly sized caller-owned out-param pair; the null
    // `newp` / zero `newlen` make this a read.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut bytes as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>()).then_some(bytes)
}

/// Total physical RAM in MB via `sysctl hw.memsize` (0 if the call fails).
/// Read once and cached for the process lifetime.
pub fn mem_total_mb() -> f64 {
    static MEM_TOTAL_MB: OnceLock<f64> = OnceLock::new();
    *MEM_TOTAL_MB.get_or_init(|| hw_memsize().map(bytes_to_mb).unwrap_or(0.0))
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

/// Sum of RSS (MB) across `pids` — `pti_resident_size`, the Mach task's
/// resident footprint and the closest analogue of Linux RSS. Vanished or
/// unreadable pids contribute nothing.
pub fn rss_mb(pids: &HashSet<u32>) -> f64 {
    let mut bytes: u64 = 0;
    for &pid in pids {
        if let Some(ti) = task_info(pid) {
            bytes += ti.pti_resident_size;
        }
    }
    bytes_to_mb(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed-width C string field holding `text`, NUL-padded — or flush to
    /// the edge with no terminator when `text` is exactly `width` long.
    fn field(text: &str, width: usize) -> Vec<libc::c_char> {
        let mut out: Vec<libc::c_char> = text.bytes().map(|b| b as libc::c_char).collect();
        out.resize(width, 0);
        out
    }

    // ---- pure helpers (mirroring proc.rs, held to the same contract) ----

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
    fn cpu_unit_is_nanoseconds() {
        // The contract that lets this backend share `collect::measure` with the
        // other two: whatever unit `jiffies` is in, `clk_tck()` names it.
        // `proc_taskinfo` is nanoseconds, so one second must be 10^9 units.
        assert_eq!(clk_tck(), 1_000_000_000);
    }

    #[test]
    fn bytes_convert_to_mebibytes() {
        assert_eq!(bytes_to_mb(0), 0.0);
        assert_eq!(bytes_to_mb(1024 * 1024), 1.0);
        assert_eq!(bytes_to_mb(16 * 1024 * 1024 * 1024), 16384.0); // 16 GiB
        assert_eq!(bytes_to_mb(512 * 1024), 0.5);
    }

    #[test]
    fn c_str_field_stops_at_nul_and_tolerates_no_terminator() {
        assert_eq!(c_str_field(&field("bash", 16)), "bash");
        // Exactly MAXCOMLEN bytes with no room for a NUL — take the lot.
        assert_eq!(
            c_str_field(&field("exactly-16-chars", 16)),
            "exactly-16-chars"
        );
        // Garbage past the terminator is not ours to read.
        let mut padded = field("zsh", 16);
        padded[8] = b'X' as libc::c_char;
        assert_eq!(c_str_field(&padded), "zsh");
        assert_eq!(c_str_field(&field("", 16)), "");
    }

    #[test]
    fn image_name_prefers_p_name_and_falls_back_to_comm() {
        // p_name is the longer field, so it wins when populated.
        assert_eq!(
            image_name(&field("space-usage", 32), &field("space-usage", 16)),
            Some("space-usage".to_string())
        );
        // Never exec'd: p_name is empty, p_comm still names the process.
        assert_eq!(
            image_name(&field("", 32), &field("kernel_task", 16)),
            Some("kernel_task".to_string())
        );
        // Nothing at all — `None`, so the daemon's guard cannot match two
        // nameless processes to each other.
        assert_eq!(image_name(&field("", 32), &field("", 16)), None);
    }

    // ---- live probes (macOS CI runner; assert only what any Mac grants) ----

    #[cfg(target_os = "macos")]
    #[test]
    fn scan_includes_our_own_process_with_parent_and_cpu_time() {
        let procs = scan_proc();
        let me = procs
            .get(&std::process::id())
            .expect("our own pid is in the snapshot");
        // SAFETY: `getppid` is a pure query of our own process state.
        let parent = unsafe { libc::getppid() } as u32;
        assert_eq!(me.ppid, parent, "own parent pid mismatch");
        // Our own task is always readable, so its CPU counter is real.
        assert!(
            me.jiffies > 0,
            "own cumulative CPU nanoseconds read as zero"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pid_listing_finds_us_and_launchd() {
        let pids = list_pids();
        assert!(pids.contains(&std::process::id()), "own pid missing");
        assert!(pids.contains(&1), "launchd (pid 1) missing");
        assert!(!pids.contains(&0), "kernel_task pseudo-pid not filtered");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn own_image_name_is_reported_and_vanished_pid_is_none() {
        let name = process_image_name(std::process::id()).expect("own image name");
        assert!(!name.is_empty());
        assert_eq!(process_image_name(u32::MAX), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn machine_ram_total_is_positive() {
        assert!(mem_total_mb() > 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn own_rss_is_positive() {
        let pids = HashSet::from([std::process::id()]);
        assert!(rss_mb(&pids) > 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nproc_is_at_least_one() {
        assert!(nproc() >= 1);
    }
}
