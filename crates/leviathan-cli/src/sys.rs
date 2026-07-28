//! Peak memory measurement and machine identification.
//!
//! M1's headline exit criterion is a memory number ("500 MB NDJSON indexed with
//! peak process memory < 400 MB"), so the harness has to be able to measure peak
//! memory or it cannot gate the milestone. That is the only reason this module
//! exists, and the only reason the CLI contains any `unsafe` at all.
//!
//! **Peak, not current.** Sampling current RSS on a timer would miss the spike
//! that matters. Both platforms below report a high-water mark the kernel
//! maintains, which cannot be missed no matter when it is read.
//!
//! Linux and Windows are covered — CI runs the former, development happens on
//! the latter. Elsewhere this reports `None`, and the harness prints `—` rather
//! than inventing a number.

use std::fmt;

/// Peak resident set size of this process, in bytes, since it started.
///
/// Returns `None` on platforms where it is not implemented.
#[must_use]
pub fn peak_rss() -> Option<u64> {
    imp::peak_rss()
}

#[cfg(target_os = "linux")]
mod imp {
    /// `VmHWM` is the kernel's high-water mark for resident memory, in kB.
    pub fn peak_rss() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
}

#[cfg(windows)]
mod imp {
    /// `PROCESS_MEMORY_COUNTERS` from psapi.h.
    ///
    /// Laid out by hand rather than pulled from `windows-sys`, to keep the
    /// crate's dependency count at zero. Only `peak_working_set_size` is read;
    /// the rest of the fields exist so the struct is the size the OS expects.
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    pub fn peak_rss() -> Option<u64> {
        let mut counters = ProcessMemoryCounters {
            cb: u32::try_from(size_of::<ProcessMemoryCounters>()).ok()?,
            ..Default::default()
        };

        // SAFETY: `counters` is a live, correctly-sized, correctly-aligned
        // `PROCESS_MEMORY_COUNTERS`, and `cb` tells the OS its size so it
        // cannot write past the end. `GetCurrentProcess` returns a pseudo-handle
        // that is always valid and needs no closing. `K32GetProcessMemoryInfo`
        // has been exported from kernel32 since Windows 7, which is below this
        // project's floor.
        let ok =
            unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, counters.cb) };

        (ok != 0).then_some(counters.peak_working_set_size as u64)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn peak_rss() -> Option<u64> {
        None
    }
}

/// What machine produced a number.
///
/// Printed above every benchmark table and embedded in the JSON output. A
/// throughput figure without the machine that produced it is not a measurement,
/// it is a boast — and SPEC §M7 promises the benchmarks are reproducible.
pub struct Machine {
    /// Logical CPUs available to this process.
    pub cpus: usize,
    /// Target architecture, e.g. `x86_64`.
    pub arch: &'static str,
    /// Target OS, e.g. `windows`.
    pub os: &'static str,
    /// The cargo profile the binary was built with, as far as it can tell.
    pub profile: &'static str,
}

impl Machine {
    /// Describe the machine this process is running on.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            cpus: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
            arch: std::env::consts::ARCH,
            os: std::env::consts::OS,
            profile: profile(),
        }
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} × {}, {}, profile {}",
            self.cpus, self.arch, self.os, self.profile
        )
    }
}

/// Which profile this binary was built with.
///
/// There is no first-class way to ask, so this infers it from what the compiler
/// was told. The distinction that matters is "was this optimized for speed" —
/// publishing a throughput number from a debug build would be nonsense, and the
/// harness warns when it detects one.
const fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug (UNOPTIMIZED — do not publish these numbers)"
    } else {
        "optimized"
    }
}

/// True if this build is fast enough for its numbers to mean anything.
#[must_use]
pub const fn is_optimized() -> bool {
    !cfg!(debug_assertions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_rss_is_plausible_where_implemented() {
        if let Some(bytes) = peak_rss() {
            // A process that has linked std and run a test harness is over 1 MB
            // and under 100 GB. Anything outside that is a unit error.
            assert!(bytes > 1_000_000, "implausibly small: {bytes}");
            assert!(bytes < 100_000_000_000, "implausibly large: {bytes}");
        }
    }

    #[test]
    fn peak_rss_never_decreases() {
        let Some(before) = peak_rss() else { return };
        // Touch enough memory to move the high-water mark on any platform.
        let ballast: Vec<u8> = (0..64_000_000).map(|i| (i % 251) as u8).collect();
        assert_eq!(ballast.len(), 64_000_000);
        let after = peak_rss().expect("still implemented");
        assert!(after >= before, "peak went backwards: {before} -> {after}");
        drop(ballast);
        assert!(
            peak_rss().expect("still implemented") >= after,
            "peak is not a high-water mark"
        );
    }

    #[test]
    fn machine_describes_itself() {
        let machine = Machine::detect();
        assert!(!machine.arch.is_empty());
        assert!(!machine.os.is_empty());
        assert_eq!(machine.profile == "optimized", is_optimized());
    }
}
