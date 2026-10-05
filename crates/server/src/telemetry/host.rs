//! The host's own readings, which every role reports while no per-host collector does (a
//! single-host deployment): disk and memory use and the CPU time a hypervisor stole.
//!
//! - `norbelys_host_disk_used_ratio{mount}`: the share of the root filesystem's blocks that an
//!   unprivileged writer can no longer use (`statvfs`), the space a role's spool, the database's
//!   WAL and logs share on a small host.
//! - `norbelys_host_memory_used_ratio`: `1 − MemAvailable / MemTotal` from `/proc/meminfo`, the
//!   kernel's own estimate of what can still be allocated without swapping
//!   (<https://www.kernel.org/doc/html/latest/filesystems/proc.html#meminfo>).
//! - `norbelys_host_cpu_steal_ratio`: the share of CPU time stolen by the hypervisor since the
//!   previous reading, from the first line of `/proc/stat`
//!   (<https://www.kernel.org/doc/html/latest/filesystems/proc.html#miscellaneous-kernel-statistics-in-proc-stat>),
//!   the sign of an oversold virtual machine.
//!
//! The readings are taken when the metrics are collected, never on a timer of their own. Where
//! `/proc` does not exist (a developer's macOS) the memory and steal readings report nothing.

use std::sync::Mutex;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

/// The filesystem whose use is reported.
const ROOT: &str = "/";

/// Registers the three gauges on `meter`; each reads the host when the metrics are collected.
pub(super) fn register(meter: &Meter) {
    let _ = meter
        .f64_observable_gauge("norbelys_host_disk_used_ratio")
        .with_description("The share of the filesystem's blocks no longer available, by mount.")
        .with_callback(|observer| {
            if let Some(used) = disk_used(ROOT) {
                observer.observe(used, &[KeyValue::new("mount", ROOT)]);
            }
        })
        .build();
    let _ = meter
        .f64_observable_gauge("norbelys_host_memory_used_ratio")
        .with_description("The share of the host's memory not available to new allocations.")
        .with_callback(|observer| {
            if let Some(used) = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .as_deref()
                .and_then(memory_used)
            {
                observer.observe(used, &[]);
            }
        })
        .build();
    let previous: Mutex<Option<Cpu>> = Mutex::new(None);
    let _ = meter
        .f64_observable_gauge("norbelys_host_cpu_steal_ratio")
        .with_description("The share of CPU time the hypervisor stole since the previous reading.")
        .with_callback(move |observer| {
            let Some(now) = std::fs::read_to_string("/proc/stat")
                .ok()
                .as_deref()
                .and_then(cpu)
            else {
                return;
            };
            let Ok(mut previous) = previous.lock() else {
                return;
            };
            if let Some(stolen) = (*previous).and_then(|before| steal_between(before, now)) {
                observer.observe(stolen, &[]);
            }
            *previous = Some(now);
        })
        .build();
}

/// `part / whole` as a ratio in `[0, 1]`, to a millionth; `None` when `whole` is 0.
fn ratio(part: u64, whole: u64) -> Option<f64> {
    if whole == 0 {
        return None;
    }
    let millionths = u128::from(part.min(whole)) * 1_000_000 / u128::from(whole);
    u32::try_from(millionths)
        .ok()
        .map(|millionths| f64::from(millionths) / 1_000_000.0)
}

/// The used share of the filesystem at `path`, or `None` when it cannot be read.
fn disk_used(path: &str) -> Option<f64> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let blocks = stat.f_blocks;
    ratio(blocks.saturating_sub(stat.f_bavail), blocks)
}

/// `1 − MemAvailable / MemTotal` from the text of `/proc/meminfo`.
fn memory_used(meminfo: &str) -> Option<f64> {
    let field = |name: &str| {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    ratio(total.saturating_sub(available), total)
}

/// The CPU time counters of `/proc/stat`'s first line, in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cpu {
    /// `steal`.
    steal: u64,
    /// `user + nice + system + idle + iowait + irq + softirq + steal`: guest time is already
    /// inside `user` and `nice`.
    total: u64,
}

/// The aggregate `cpu` line of `/proc/stat`.
fn cpu(stat: &str) -> Option<Cpu> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<_, _>>()
        .ok()?;
    let steal = *fields.get(7)?;
    Some(Cpu {
        steal,
        total: fields.iter().copied().fold(0_u64, u64::saturating_add),
    })
}

/// The share of the time between two readings that was stolen; `None` when the counters did not
/// move forward (the same instant, or a reset).
fn steal_between(before: Cpu, after: Cpu) -> Option<f64> {
    let total = after.total.checked_sub(before.total)?;
    let steal = after.steal.checked_sub(before.steal)?;
    ratio(steal, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `/proc/meminfo` (Linux 6.8, an 8 GiB virtual machine) gives the used share from
    /// `MemTotal` and `MemAvailable`, not from `MemFree`, which counts the page cache as used.
    #[test]
    fn memory_use_comes_from_memavailable() {
        let meminfo = "MemTotal:        8131260 kB\n\
                       MemFree:          412308 kB\n\
                       MemAvailable:    6098444 kB\n\
                       Buffers:          201716 kB\n\
                       Cached:          5207216 kB\n";
        let used = memory_used(meminfo).unwrap();
        assert!((used - 0.25).abs() < 0.001, "{used}");
        assert_eq!(memory_used("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
        assert_eq!(memory_used("MemFree: 12 kB\n"), None);
    }

    /// Two readings of a real `/proc/stat` give the stolen share of the ticks in between, and a
    /// counter that went back (a reset) reports nothing rather than a nonsense share.
    #[test]
    fn steal_is_the_share_of_ticks_between_two_readings() {
        let before =
            cpu("cpu  4705 356 584 3699176 23060 0 277 1000 0 0\ncpu0 1393 280 2 0 0 0 0 0 0 0\n")
                .unwrap();
        let after = cpu("cpu  4805 356 584 3699976 23060 0 277 1100 0 0\n").unwrap();
        assert_eq!(after.steal, 1_100);
        let stolen = steal_between(before, after).unwrap();
        assert!((stolen - 0.1).abs() < 0.000_001, "{stolen}");
        assert_eq!(steal_between(after, before), None);
        assert_eq!(cpu("intr 1 2 3\n"), None);
    }

    /// Ratios are bounded to `[0, 1]` and refuse an empty whole.
    #[test]
    fn ratios_are_bounded() {
        assert_eq!(ratio(1, 4), Some(0.25));
        assert_eq!(ratio(5, 4), Some(1.0));
        assert_eq!(ratio(0, 0), None);
        assert_eq!(ratio(u64::MAX, u64::MAX), Some(1.0));
    }
}
