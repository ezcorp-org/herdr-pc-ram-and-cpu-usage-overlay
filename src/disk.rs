//! Free space on the drives the user selected, one backend per platform.
//!
//! [`read`] answers one question per selected mount: *how much room is left on
//! it, if we can tell?* A mount we cannot stat — a path that does not exist, an
//! unplugged drive, a network share that has gone away — yields no reading at
//! all, and the renderer draws no cell for it. That is the same rule
//! [`crate::battery`] follows, and for the same reason: a fabricated `0%` is
//! worse than an absent one.
//!
//! Every backend is split in two: an impure half that touches the host (a
//! `statvfs` call, a Win32 call) and a pure half — [`from_bytes`] — that turns
//! the raw byte counts into a [`Disk`]. Only the impure halves carry `#[cfg]`,
//! so the conversion and the Windows mount normalisation compile, and are
//! unit-tested, on every target.

/// Bytes in a megabyte, the unit every figure here is stored in (matching
/// [`crate::proc`]'s RAM figures, so one formatter renders both).
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// Space on one filesystem.
#[derive(Debug, Clone, PartialEq)]
pub struct Disk {
    /// The mount as the user selected it (`/`, `/home`, `C:`), used verbatim
    /// when a cell has to name which drive it is reporting.
    pub name: String,
    /// Space available to this user, in MB. Never above [`Self::total_mb`].
    pub free_mb: f64,
    /// Space already occupied, in MB. Not simply `total - free`: a unix
    /// filesystem reserves blocks only root may fill, and those are neither
    /// used nor available to anyone else.
    pub used_mb: f64,
    /// Size of the filesystem, in MB. Always positive — [`from_bytes`] rejects
    /// anything else, so there is always something to be a percentage of.
    pub total_mb: f64,
}

impl Disk {
    /// Percent of the filesystem in use, 0..=100 — the figure `df` prints under
    /// `Use%`.
    ///
    /// Used rather than free because this cell sits beside cpu and ram, which
    /// are both usage: a row reading `cpu 27% · ram 61% · disk 21%` invites
    /// everyone to read the last number the same way as the first two, and a
    /// disk that is 79% full does not deserve to look like the idlest thing on
    /// the machine.
    ///
    /// The denominator is `used + available`, not the filesystem size, which is
    /// exactly what `df` does: the blocks reserved for root belong to neither
    /// side, so counting them would report a full disk as 95% rather than 100%
    /// and disagree with the tool people check against.
    pub fn used_percent(&self) -> f64 {
        let usable = self.used_mb + self.free_mb;
        if usable <= 0.0 {
            return 0.0;
        }
        (100.0 * self.used_mb / usable).clamp(0.0, 100.0)
    }
}

/// Readings for `mounts`, in the order given, skipping every mount this host
/// cannot answer for.
///
/// Call once per refresh and pass the result down. Each entry costs one
/// `statvfs` (no subprocess, no allocation of consequence), but that is still
/// once per *drive*, and the machine-wide surfaces all draw the same list.
pub fn read(mounts: &[String]) -> Vec<Disk> {
    mounts.iter().filter_map(|mount| host_read(mount)).collect()
}

/// unix (Linux, macOS, the BSDs): one `statvfs` per mount.
#[cfg(unix)]
fn host_read(mount: &str) -> Option<Disk> {
    statvfs::probe(mount)
}

/// Windows: `kernel32!GetDiskFreeSpaceExW`.
#[cfg(windows)]
fn host_read(mount: &str) -> Option<Disk> {
    disk_free_space::probe(mount)
}

/// Any other target: no filesystem API we speak, so every drive reads as
/// unanswerable and the metric simply does not draw.
#[cfg(not(any(unix, windows)))]
fn host_read(_mount: &str) -> Option<Disk> {
    None
}

/// The single gate every backend pushes its raw byte counts through.
///
/// Rejects what cannot be rendered — a zero or negative size (a pseudo-filesystem
/// like `/proc`, or a drive that answered with nothing), and any non-finite
/// figure — and pins both figures into the size, because a filesystem that
/// reports more than it has would otherwise render past 100%.
///
/// `used_bytes` is passed rather than derived: on unix it is
/// `(blocks - free) * size`, which is NOT `total - available`, since the blocks
/// reserved for root are in neither figure.
fn from_bytes(name: &str, free_bytes: f64, used_bytes: f64, total_bytes: f64) -> Option<Disk> {
    if !free_bytes.is_finite()
        || !used_bytes.is_finite()
        || !total_bytes.is_finite()
        || total_bytes <= 0.0
    {
        return None;
    }
    Some(Disk {
        name: name.to_string(),
        free_mb: free_bytes.clamp(0.0, total_bytes) / BYTES_PER_MB,
        used_mb: used_bytes.clamp(0.0, total_bytes) / BYTES_PER_MB,
        total_mb: total_bytes / BYTES_PER_MB,
    })
}

// ---- unix: statvfs ----------------------------------------------------------

/// unix backend: `statvfs(2)`, which every unix we build for provides and which
/// answers for whatever filesystem the path lands on — so a user can name a
/// mount point or any directory inside it and get the same reading.
#[cfg(unix)]
mod statvfs {
    use super::{from_bytes, Disk};

    /// Stat the filesystem holding `mount`, or `None` when the call fails (a
    /// path that does not exist, a stale network mount, a permission wall).
    pub fn probe(mount: &str) -> Option<Disk> {
        // An interior NUL cannot reach the kernel as a path, so a config value
        // carrying one is simply not a mount we can stat.
        let path = std::ffi::CString::new(mount).ok()?;
        // SAFETY: `statvfs` is six-plus integers, so an all-zero bit pattern is
        // a valid value of it.
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a live NUL-terminated C string and `buf` is a live,
        // correctly sized, caller-owned statvfs; the call only writes into it.
        if unsafe { libc::statvfs(path.as_ptr(), &mut buf) } != 0 {
            return None;
        }
        // `f_frsize` is the fragment size the block counts are expressed in.
        // Some filesystems leave it zero and mean `f_bsize`, so fall back rather
        // than multiplying every count by nothing.
        let block = if buf.f_frsize > 0 {
            buf.f_frsize as f64
        } else {
            buf.f_bsize as f64
        };
        // Three counts, and the difference between them is the whole reason
        // `df` prints what it prints:
        //   f_bavail — room THIS user can still fill (not f_bfree, which counts
        //              the blocks reserved for root as free);
        //   f_blocks - f_bfree — room already occupied;
        //   f_blocks — the filesystem's size.
        // The reserved blocks are in neither of the first two, which is why the
        // used percentage divides by their sum rather than by the size.
        from_bytes(
            mount,
            buf.f_bavail as f64 * block,
            buf.f_blocks.saturating_sub(buf.f_bfree) as f64 * block,
            buf.f_blocks as f64 * block,
        )
    }
}

// ---- Windows: GetDiskFreeSpaceExW -------------------------------------------

/// Windows backend: `kernel32!GetDiskFreeSpaceExW`.
///
/// Hand-declared rather than pulled from `windows-sys`, for the reason
/// `battery::power_status` gives: one function is less surface than another
/// feature flag, and keeping the binding local lets the half with the actual
/// logic — [`normalize_mount`](disk_free_space::normalize_mount) — be unit-tested
/// on Linux and macOS too.
#[cfg_attr(not(windows), allow(dead_code))]
mod disk_free_space {
    #[cfg(windows)]
    use super::{from_bytes, Disk};

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        /// Fills the three counters for the volume containing `directory`;
        /// returns 0 on failure. A null out-pointer is allowed, but we want two
        /// of the three, so all are passed.
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    /// Ask Windows for the volume behind `mount`, or `None` when the call fails
    /// (a drive letter with nothing in it, a disconnected share).
    #[cfg(windows)]
    pub fn probe(mount: &str) -> Option<Disk> {
        let wide: Vec<u16> = normalize_mount(mount)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut free_to_caller: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: `wide` is a live NUL-terminated UTF-16 string and the three
        // out-pointers are live, caller-owned u64s the call only writes into.
        if unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_to_caller,
                &mut total,
                &mut total_free,
            )
        } == 0
        {
            return None;
        }
        // `free_to_caller` honours any per-user quota, which is the figure a
        // user can actually write into — the same choice `f_bavail` makes on
        // unix. `total - total_free` is what is occupied, so a quota'd account
        // gets the same "used against the room I have" split unix gets.
        from_bytes(
            mount,
            free_to_caller as f64,
            total.saturating_sub(total_free) as f64,
            total as f64,
        )
    }

    /// Turn a drive as a person writes it into a path the API accepts.
    ///
    /// `GetDiskFreeSpaceExW` wants a directory: `C:\` is one, but the bare `C:`
    /// people actually type means "the current directory on C:", which is a
    /// different place and fails outright when there is none. `C` alone is the
    /// other obvious shorthand. Everything else — a full path, a UNC share — is
    /// passed through untouched, and only the *display* name stays as typed.
    pub fn normalize_mount(mount: &str) -> String {
        let mount = mount.trim();
        let mut chars = mount.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some(letter), None, None) if letter.is_ascii_alphabetic() => {
                format!("{letter}:\\")
            }
            (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => {
                format!("{letter}:\\")
            }
            _ => mount.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One gigabyte, in the bytes the backends hand [`from_bytes`].
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    // ---- the shared conversion gate -----------------------------------------

    #[test]
    fn from_bytes_converts_to_mb_and_keeps_the_name() {
        let disk = from_bytes("/", 231.0 * GB, 281.0 * GB, 512.0 * GB).expect("a readable fs");
        assert_eq!(disk.name, "/");
        assert_eq!(disk.free_mb, 231.0 * 1024.0);
        assert_eq!(disk.used_mb, 281.0 * 1024.0);
        assert_eq!(disk.total_mb, 512.0 * 1024.0);
        // 281 used of 512 usable = 54.9%
        assert!((disk.used_percent() - 54.883).abs() < 0.01, "{disk:?}");
    }

    #[test]
    fn used_percent_divides_by_what_the_user_can_touch_like_df_does() {
        // A real ext4: 1.8 TB of blocks, 5% reserved for root. 1.4 TB used and
        // 382 GB available means `df` prints 78%, NOT the 79% you get by
        // dividing the used bytes by the filesystem size — the reserve belongs
        // to neither side. Matching `df` is the point: it is what people check.
        let disk = from_bytes("/", 382.0 * GB, 1372.0 * GB, 1832.0 * GB).expect("a readable fs");
        assert_eq!(disk.used_percent().round(), 78.0);
        assert_ne!((100.0 * disk.used_mb / disk.total_mb).round(), 78.0);
    }

    #[test]
    fn used_percent_covers_both_ends_and_an_unusable_filesystem() {
        let disk = |free: f64, used: f64| {
            from_bytes("/", free * GB, used * GB, 512.0 * GB)
                .expect("a readable fs")
                .used_percent()
        };
        assert_eq!(disk(512.0, 0.0), 0.0, "an empty disk is 0% used");
        assert_eq!(disk(0.0, 512.0), 100.0, "a full disk is 100% used");
        // Nothing usable at all — every block reserved — has no ratio to give,
        // and must not divide by zero.
        assert_eq!(disk(0.0, 0.0), 0.0);
    }

    #[test]
    fn from_bytes_rejects_a_filesystem_with_no_size() {
        // A pseudo-filesystem (`/proc`, `/sys`) reports zero blocks: there is
        // nothing to be a percentage of, so there is no cell to draw.
        assert_eq!(from_bytes("/proc", 0.0, 0.0, 0.0), None);
        assert_eq!(from_bytes("/weird", 10.0 * GB, 0.0, -1.0), None);
    }

    #[test]
    fn from_bytes_rejects_non_numbers() {
        // A backend that multiplied a count by a nonsense block size would
        // otherwise render `NaN%`.
        assert_eq!(from_bytes("/", f64::NAN, 0.0, GB), None);
        assert_eq!(from_bytes("/", GB, f64::NAN, GB), None);
        assert_eq!(from_bytes("/", GB, 0.0, f64::NAN), None);
        assert_eq!(from_bytes("/", f64::INFINITY, 0.0, GB), None);
        assert_eq!(from_bytes("/", GB, 0.0, f64::INFINITY), None);
    }

    #[test]
    fn from_bytes_pins_both_figures_into_the_size() {
        // More available (or more used) than the filesystem has would render
        // past 100%.
        let over = from_bytes("/", 600.0 * GB, 0.0, 512.0 * GB).expect("clamped, not dropped");
        assert_eq!(over.free_mb, over.total_mb);
        assert_eq!(over.used_percent(), 0.0);

        let under = from_bytes("/", -5.0 * GB, 600.0 * GB, 512.0 * GB).expect("clamped");
        assert_eq!(under.free_mb, 0.0);
        assert_eq!(under.used_mb, under.total_mb);
        assert_eq!(under.used_percent(), 100.0);
    }

    // ---- the host itself -----------------------------------------------------

    #[test]
    fn read_answers_for_the_root_filesystem_and_skips_what_it_cannot_stat() {
        // The root filesystem exists on every machine this builds for, so this
        // exercises the real backend — the syscall, the block arithmetic, and
        // the conversion — rather than a fixture.
        let root = if cfg!(windows) { "C:" } else { "/" };
        let missing = "/no-such-mount-point-for-space-usage-tests";

        let readings = read(&[root.to_string(), missing.to_string()]);
        assert_eq!(
            readings.len(),
            1,
            "an unstattable mount contributes no reading: {readings:?}",
        );

        let disk = &readings[0];
        assert_eq!(disk.name, root);
        assert!(disk.total_mb > 0.0, "{disk:?}");
        assert!((0.0..=100.0).contains(&disk.used_percent()), "{disk:?}");
        assert!(disk.free_mb <= disk.total_mb, "{disk:?}");
        assert!(disk.used_mb <= disk.total_mb, "{disk:?}");
        // The reserve is why these do not add up to the size.
        assert!(
            disk.used_mb + disk.free_mb <= disk.total_mb + 1.0,
            "{disk:?}"
        );
    }

    #[test]
    fn read_of_nothing_is_no_readings() {
        // `disk = false` and an empty selection both arrive here, and neither
        // may touch the host.
        assert_eq!(read(&[]), Vec::new());
    }

    #[test]
    fn read_keeps_the_selection_order() {
        // The cells are drawn in the order the user listed their drives, so the
        // title does not reshuffle itself between refreshes.
        let root = if cfg!(windows) { "C:" } else { "/" };
        let readings = read(&[root.to_string(), root.to_string()]);
        assert_eq!(readings.len(), 2);
        // The NAME is what the order is about, and the only field of a live
        // reading that holds still. Comparing whole readings made this test
        // flaky: they are two `statvfs` calls a moment apart, and anything
        // writing to the disk in between — which on a build machine is
        // everything — moves `free_mb` a few kilobytes and fails an exact float
        // comparison. Seen once in roughly a hundred local runs; CI writes far
        // more than this box does.
        assert_eq!(readings[0].name, readings[1].name);
        assert_eq!(readings[0].name, root);
        // Same filesystem, so its size cannot have changed between the two
        // calls — unlike how much of it is free.
        assert_eq!(readings[0].total_mb, readings[1].total_mb);
    }

    #[test]
    fn a_path_with_an_interior_nul_is_not_a_mount() {
        // Cannot reach the kernel as a path, so it is unanswerable rather than
        // an error the user has to see.
        assert_eq!(read(&["/tmp\0/etc".to_string()]), Vec::new());
    }

    // ---- Windows: mount normalisation ---------------------------------------

    #[test]
    fn a_bare_drive_letter_becomes_a_root_directory() {
        // `C:` means "the current directory on C:" to Win32, which is a
        // different place from `C:\` and fails when there is none.
        let normalize = disk_free_space::normalize_mount;
        assert_eq!(normalize("C:"), "C:\\");
        assert_eq!(normalize("c"), "c:\\");
        assert_eq!(normalize(" D: "), "D:\\");
    }

    #[test]
    fn a_real_path_is_passed_through_untouched() {
        let normalize = disk_free_space::normalize_mount;
        assert_eq!(normalize("C:\\"), "C:\\");
        assert_eq!(normalize("D:\\data"), "D:\\data");
        assert_eq!(normalize("\\\\server\\share\\"), "\\\\server\\share\\");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("/home"), "/home");
        assert_eq!(normalize(""), "");
    }
}
