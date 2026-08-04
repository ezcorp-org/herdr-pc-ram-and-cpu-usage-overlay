//! Windows process sampling — the `proc` module twin of the Linux `/proc` reader.
//!
//! Same public surface as `proc.rs`, different probes: the process table comes
//! from a Toolhelp32 snapshot (pid + ppid), cumulative CPU time from
//! `GetProcessTimes` (kernel + user, in 100 ns FILETIME ticks — so [`clk_tck`]
//! is 10^7), RSS from `K32GetProcessMemoryInfo` (working set), and the machine
//! total from `GlobalMemoryStatusEx`. Processes we cannot open (other users,
//! protected system processes) still contribute their pid/ppid edge — the
//! subtree walk needs the topology even when the counters are unreadable — but
//! sample as zero CPU/RSS, which is correct for panes we own: a herdr pane's
//! subtree runs as the current user.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetProcessTimes, OpenProcess, TerminateProcess, ALL_PROCESSOR_GROUPS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// Per-process sample: parent PID and cumulative CPU time in [`clk_tck`] units
/// (here FILETIME 100 ns ticks; the Linux twin uses jiffies — the shared math in
/// `collect::measure` divides by [`clk_tck`] so both come out as seconds).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcEntry {
    pub ppid: u32,
    pub jiffies: u64,
}

/// CPU-time units per second: FILETIME ticks are 100 ns.
pub fn clk_tck() -> u64 {
    10_000_000
}

/// Number of logical CPUs; normalizes CPU% to a share of the whole machine.
/// At least 1.
///
/// `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)`, NOT
/// `available_parallelism()`. The metric is a share of the WHOLE MACHINE, to
/// match the Linux twin's `_SC_NPROCESSORS_ONLN`. On Windows
/// `available_parallelism` is `GetSystemInfo().dwNumberOfProcessors`, which is
/// documented as the count for the CALLER'S PROCESSOR GROUP — so on a box with
/// more than 64 logical CPUs it saturates at 64, halving the divisor and
/// doubling every reported CPU%. (It is not affinity-aware either; std's own
/// docs note it can overcount under a process affinity mask or job object
/// limit. Neither function honours affinity, which is what keeps this in step
/// with `_SC_NPROCESSORS_ONLN`.)
pub fn nproc() -> u64 {
    // SAFETY: a pure query of a static system value.
    let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    (n as u64).max(1)
}

/// An open process handle that closes itself on drop; `None` when the process
/// cannot be opened with `access` (gone, or not ours to query).
struct Process(HANDLE);

impl Process {
    fn open(pid: u32, access: u32) -> Option<Process> {
        // SAFETY: `OpenProcess` is a pure acquisition call; a null handle means
        // failure and is mapped to `None`.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Process(handle))
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle was returned live by `OpenProcess`.
        unsafe { CloseHandle(self.0) };
    }
}

/// `FILETIME` (two 32-bit halves) to one u64 of 100 ns ticks.
fn filetime_ticks(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Cumulative kernel+user CPU ticks for `pid`, or `None` when unreadable.
fn cpu_ticks(pid: u32) -> Option<u64> {
    let process = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut user = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: all four out-params are valid caller-owned FILETIMEs.
    let ok =
        unsafe { GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user) };
    (ok != 0).then(|| filetime_ticks(&kernel) + filetime_ticks(&user))
}

/// One pass over the Toolhelp32 process snapshot, yielding each entry.
fn for_each_process(mut f: impl FnMut(&PROCESSENTRY32W)) {
    // SAFETY: TH32CS_SNAPPROCESS snapshots are a documented read-only query.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `entry` is a properly sized PROCESSENTRY32W for both calls.
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                f(&entry);
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
}

/// Snapshot the process table once, returning `pid -> {ppid, cpu ticks}` for
/// every live process. Unopenable processes keep their pid/ppid edge (the
/// subtree walk needs it) with zero ticks.
pub fn scan_proc() -> HashMap<u32, ProcEntry> {
    let mut procs = HashMap::new();
    for_each_process(|entry| {
        let pid = entry.th32ProcessID;
        if pid == 0 {
            return; // the System Idle pseudo-process is not a real root
        }
        procs.insert(
            pid,
            ProcEntry {
                ppid: entry.th32ParentProcessID,
                jiffies: cpu_ticks(pid).unwrap_or(0),
            },
        );
    });
    procs
}

/// Executable file name (e.g. `space-usage.exe`) of `pid`, or `None` when the
/// process is gone. Used by the daemon's stale-pid-file guard.
pub fn process_image_name(pid: u32) -> Option<String> {
    let mut found = None;
    for_each_process(|entry| {
        if entry.th32ProcessID == pid {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            found = Some(String::from_utf16_lossy(&entry.szExeFile[..len]));
        }
    });
    found
}

/// Best-effort stop of `pid` (the Windows stand-in for SIGTERM). Statuses the
/// daemon pushed self-clear via their TTL, and `--disable` sweeps them anyway,
/// so an abrupt termination loses nothing.
pub fn stop_process(pid: u32) {
    if let Some(process) = Process::open(pid, PROCESS_TERMINATE) {
        // SAFETY: terminating a handle we own with PROCESS_TERMINATE access.
        unsafe { TerminateProcess(process.0, 0) };
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
/// (Windows recycles PIDs aggressively, so parent links can genuinely cycle.)
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

/// Total physical RAM in MB via `GlobalMemoryStatusEx` (0 if the call fails).
/// Read once and cached for the process lifetime.
pub fn mem_total_mb() -> f64 {
    static MEM_TOTAL_MB: OnceLock<f64> = OnceLock::new();
    *MEM_TOTAL_MB.get_or_init(|| {
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status` is a properly sized MEMORYSTATUSEX.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            status.ullTotalPhys as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
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

/// Sum of resident memory (MB) across `pids` — working set size per
/// `K32GetProcessMemoryInfo`, the closest Windows analogue of Linux RSS.
/// Vanished or unopenable pids contribute nothing.
pub fn rss_mb(pids: &HashSet<u32>) -> f64 {
    let mut bytes: u64 = 0;
    for &pid in pids {
        let Some(process) = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
            continue;
        };
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: `counters` is a properly sized PROCESS_MEMORY_COUNTERS.
        if unsafe { K32GetProcessMemoryInfo(process.0, &mut counters, counters.cb) } != 0 {
            bytes += counters.WorkingSetSize as u64;
        }
    }
    bytes as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pure helpers mirror proc.rs and keep its tests; the probe tests below
    // run against the live machine, asserting only what any Windows box grants.

    #[test]
    fn subtree_walks_descendants_with_dedup_and_cycle_safety() {
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
        assert_eq!(pct_string(1024.0, 16384.0), "6%");
        assert_eq!(pct_string(250.0, 10000.0), "3%");
        assert_eq!(pct_string(16384.0, 16384.0), "100%");
        assert_eq!(pct_string(512.0, 0.0), "");
    }

    #[test]
    fn scan_includes_our_own_process_with_cpu_ticks() {
        let procs = scan_proc();
        assert!(
            procs.contains_key(&std::process::id()),
            "our own pid is in the snapshot"
        );

        // Our own process is always openable, so the counter is readable. It is
        // NOT asserted non-zero straight away: `GetProcessTimes` is only charged
        // on ~15.6 ms scheduler ticks, so a young test binary can legitimately
        // still read zero. Burn CPU until the kernel charges us, with a deadline
        // so a genuinely broken probe fails instead of hanging.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut ticks = 0;
        while ticks == 0 && std::time::Instant::now() < deadline {
            ticks = cpu_ticks(std::process::id()).expect("own cpu ticks are readable");
            std::hint::black_box((0..200_000u64).sum::<u64>());
        }
        assert!(ticks > 0, "own cumulative CPU ticks never left zero");
    }

    #[test]
    fn stop_process_terminates_a_child() {
        // A child we own that does nothing but wait, so the only thing that can
        // end it is our TerminateProcess. `ping` ships with every Windows SKU
        // and paces one echo per second.
        let mut child = std::process::Command::new("ping")
            .args(["-n", "300", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        stop_process(child.id());
        // `wait` reaps it; without the terminate landing this blocks for ~300 s
        // and the test times out, which is the failure we want to see.
        let status = child.wait().expect("wait for ping");
        assert!(
            !status.success(),
            "a terminated child must not exit cleanly"
        );
    }

    #[test]
    fn own_image_name_is_reported_and_vanished_pid_is_none() {
        let name = process_image_name(std::process::id()).expect("own image name");
        assert!(!name.is_empty());
        assert_eq!(process_image_name(u32::MAX), None);
    }

    #[test]
    fn machine_ram_total_is_positive() {
        assert!(mem_total_mb() > 0.0);
    }

    #[test]
    fn nproc_counts_the_whole_machine() {
        let n = nproc();
        assert!(n >= 1, "nproc must never report zero");
        // Every processor group can only be >= the caller's single group, which
        // is the answer `available_parallelism` gives and this function
        // deliberately does NOT use. Equal on any box with <= 64 logical CPUs;
        // strictly greater is exactly the case that used to double CPU%.
        let one_group = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        assert!(
            n >= one_group,
            "all-groups count {n} is below the single-group {one_group}"
        );
    }

    #[test]
    fn own_rss_is_positive() {
        let pids = HashSet::from([std::process::id()]);
        assert!(rss_mb(&pids) > 0.0);
    }
}
