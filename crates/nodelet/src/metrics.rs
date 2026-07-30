//! Live node resource pressure.
//!
//! Replaces the previously-hardcoded-healthy `MemoryPressure`/`DiskPressure`/
//! `PIDPressure` node conditions (`node.rs::conditions()` used to report
//! `"False"` unconditionally — see docs/GAP_CLOSURE.md) with real reads:
//! `/proc/meminfo` for memory, `statvfs(2)` for disk, `/proc/sys/kernel/pid_max`
//! + a `/proc` directory scan for PIDs. Piggybacks on the existing infrequent
//! status-push interval (default 60s) — no new timer.

use std::ffi::CString;
use std::mem::MaybeUninit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

fn parse_kb_field(rest: &str) -> Option<u64> {
    rest.split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// Parse the two `/proc/meminfo` fields this needs. `MemAvailable` (not the
/// older, less accurate `MemFree`) is what kubelet's own eviction manager
/// keys off of — it accounts for reclaimable cache/buffers, `MemFree` alone
/// does not and would report pressure far too eagerly.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb_field(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb_field(rest);
        }
    }
    Some(MemInfo { total_bytes: total?, available_bytes: available? })
}

pub fn read_mem_info() -> Option<MemInfo> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// `statvfs(2)` on `path`'s filesystem. Returns `None` on any failure
/// (missing path, permission, non-Linux) — callers treat that as "unknown",
/// not "under pressure", so a misconfigured path fails open rather than
/// permanently tainting the node.
pub fn read_disk_info(path: &str) -> Option<DiskInfo> {
    let c_path = CString::new(path).ok()?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = stat.f_frsize as u64;
    Some(DiskInfo {
        total_bytes: stat.f_blocks as u64 * block_size,
        // f_bavail (available to unprivileged users), not f_bfree — matches
        // what a container process could actually still write.
        available_bytes: stat.f_bavail as u64 * block_size,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidInfo {
    pub max_pids: u64,
    pub current_pids: u64,
}

/// Count `/proc` entries that are pure-numeric directories — each one is a
/// live PID. Cheap enough for a 60s-interval check (a single non-recursive
/// `readdir`); this is exactly what `ps`/`top` scan too.
fn count_running_pids(proc_path: &str) -> Option<u64> {
    let entries = std::fs::read_dir(proc_path).ok()?;
    Some(
        entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
            .count() as u64,
    )
}

pub fn read_pid_info() -> Option<PidInfo> {
    let max_pids = std::fs::read_to_string("/proc/sys/kernel/pid_max").ok()?.trim().parse::<u64>().ok()?;
    let current_pids = count_running_pids("/proc")?;
    Some(PidInfo { max_pids, current_pids })
}

pub fn pid_pressure(pid: &PidInfo, threshold_percent: u8) -> bool {
    if pid.max_pids == 0 {
        return false;
    }
    let used = pid.current_pids.min(pid.max_pids);
    let available_percent = ((pid.max_pids - used) as f64 / pid.max_pids as f64) * 100.0;
    available_percent < threshold_percent as f64
}

pub fn memory_pressure(mem: &MemInfo, threshold_bytes: u64) -> bool {
    mem.available_bytes < threshold_bytes
}

pub fn disk_pressure(disk: &DiskInfo, threshold_percent: u8) -> bool {
    if disk.total_bytes == 0 {
        return false;
    }
    let available_percent = (disk.available_bytes as f64 / disk.total_bytes as f64) * 100.0;
    available_percent < threshold_percent as f64
}

/// The three live pressure conditions, computed with graceful fallback
/// (unreadable -> "no pressure", never "unknown -> assume pressure", so a
/// sandboxed/odd environment can't wedge a node NotSchedulable by itself).
pub struct Pressure {
    pub memory: bool,
    pub disk: bool,
    pub pid: bool,
}

pub fn read_pressure(
    disk_path: &str,
    mem_threshold_bytes: u64,
    disk_threshold_percent: u8,
    pid_threshold_percent: u8,
) -> Pressure {
    Pressure {
        memory: read_mem_info().map(|m| memory_pressure(&m, mem_threshold_bytes)).unwrap_or(false),
        disk: read_disk_info(disk_path).map(|d| disk_pressure(&d, disk_threshold_percent)).unwrap_or(false),
        pid: read_pid_info().map(|p| pid_pressure(&p, pid_threshold_percent)).unwrap_or(false),
    }
}

#[cfg(test)]
#[path = "metrics_tests/meminfo.rs"]
mod tests_meminfo;
#[cfg(test)]
#[path = "metrics_tests/pressure.rs"]
mod tests_pressure;
#[cfg(test)]
#[path = "metrics_tests/disk_info.rs"]
mod tests_disk_info;
#[cfg(test)]
#[path = "metrics_tests/pid_info.rs"]
mod tests_pid_info;
