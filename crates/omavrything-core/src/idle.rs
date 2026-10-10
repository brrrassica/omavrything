//! Idle detection and the rescan scheduler.
//!
//! Idleness is measured from `/proc/loadavg` (1-minute load), `/proc/stat`
//! (CPU idle fraction), and `/proc/diskstats` (in-flight I/O). The systemd
//! login1 idle hint is a secondary signal wired in a later milestone (M7).

use std::time::{Duration, Instant};

/// Thresholds below/above which the system is considered idle.
#[derive(Debug, Clone, Copy)]
pub struct IdleThresholds {
    /// Maximum 1-minute load average.
    pub max_load1: f64,
    /// Minimum CPU idle fraction (0.0–1.0).
    pub min_cpu_idle: f64,
    /// Maximum in-flight I/O operations.
    pub max_io_in_flight: u64,
}

impl Default for IdleThresholds {
    fn default() -> Self {
        Self {
            max_load1: 1.0,
            min_cpu_idle: 0.5,
            max_io_in_flight: 4,
        }
    }
}

/// A point-in-time view of system activity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleSnapshot {
    pub load1: f64,
    pub cpu_idle: f64,
    pub io_in_flight: u64,
}

impl IdleSnapshot {
    /// Whether this snapshot is below all thresholds.
    pub fn is_idle(&self, t: &IdleThresholds) -> bool {
        self.load1 <= t.max_load1
            && self.cpu_idle >= t.min_cpu_idle
            && self.io_in_flight <= t.max_io_in_flight
    }
}

/// Aggregate CPU time counters from `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// Parse the 1-minute load average from `/proc/loadavg`.
pub fn parse_loadavg(content: &str) -> Option<f64> {
    content.split_whitespace().next()?.parse().ok()
}

/// Parse the aggregate `cpu` line from `/proc/stat`.
pub fn parse_cpu(content: &str) -> Option<CpuTimes> {
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    // idle (index 3) + iowait (index 4).
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuTimes { total, idle })
}

/// Sum the in-flight I/O count from `/proc/diskstats`.
pub fn parse_diskstats(content: &str) -> u64 {
    let mut total = 0u64;
    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Field 3 (index 2) is the device name; skip partitions so their
        // in-flight I/O is not double-counted with the parent disk.
        let Some(name) = fields.get(2) else { continue };
        if is_partition(name) {
            continue;
        }
        // Field 12 (index 11) is "I/Os currently in progress".
        if let Some(v) = fields.get(11).and_then(|s| s.parse::<u64>().ok()) {
            total += v;
        }
    }
    total
}

/// Whether a `/proc/diskstats` device name is a partition rather than a whole
/// disk (`sda1`, `vdb2`, `nvme0n1p1`, `mmcblk0p1`).
fn is_partition(name: &str) -> bool {
    // nvme0n1p1, mmcblk0p1: a 'p' preceded by a digit and followed by digits.
    if let Some(pos) = name.rfind('p') {
        let before = &name[..pos];
        let after = &name[pos + 1..];
        if !after.is_empty()
            && after.bytes().all(|b| b.is_ascii_digit())
            && before.bytes().last().is_some_and(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    // sda1, vdb2, hdc3, xvda1: a known disk prefix, letters, then digits.
    for prefix in ["xvd", "sd", "hd", "vd"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let digits = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            if !digits.is_empty()
                && digits.len() < rest.len()
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

/// Reads system activity and decides whether the system is idle.
#[derive(Debug)]
pub struct IdleMonitor {
    thresholds: IdleThresholds,
    prev_cpu: Option<CpuTimes>,
}

impl IdleMonitor {
    pub fn new(thresholds: IdleThresholds) -> Self {
        Self {
            thresholds,
            prev_cpu: None,
        }
    }

    pub fn thresholds(&self) -> &IdleThresholds {
        &self.thresholds
    }

    /// Read the current activity and decide whether the system is idle. If the
    /// signals cannot be read, the system is treated as busy.
    pub fn is_idle(&mut self) -> bool {
        match self.snapshot() {
            Some(s) => s.is_idle(&self.thresholds),
            None => false,
        }
    }

    /// Read a fresh snapshot, computing the CPU idle fraction from the delta
    /// since the previous call.
    pub fn snapshot(&mut self) -> Option<IdleSnapshot> {
        let load1 = parse_loadavg(&std::fs::read_to_string("/proc/loadavg").ok()?)?;
        let cpu = parse_cpu(&std::fs::read_to_string("/proc/stat").ok()?)?;
        let io_in_flight = parse_diskstats(&std::fs::read_to_string("/proc/diskstats").ok()?);

        // The first call has no previous sample, so a delta cannot be computed;
        // report "no snapshot" rather than a misleading since-boot average.
        let Some(prev) = self.prev_cpu else {
            self.prev_cpu = Some(cpu);
            return None;
        };
        self.prev_cpu = Some(cpu);

        let cpu_idle = if cpu.total > prev.total {
            let dt = cpu.total - prev.total;
            let di = cpu.idle.saturating_sub(prev.idle);
            di as f64 / dt as f64
        } else {
            // Counters did not advance (or wrapped): treat as fully idle.
            1.0
        };

        Some(IdleSnapshot {
            load1,
            cpu_idle,
            io_in_flight,
        })
    }
}

/// Decides when a periodic rescan may run: the interval must have elapsed and
/// the system must be idle.
#[derive(Debug)]
pub struct RescanScheduler {
    interval: Duration,
    last: Instant,
}

impl RescanScheduler {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Instant::now(),
        }
    }

    /// Change the rescan interval (e.g. after a runtime config update).
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Whether a rescan should run at `now`. The interval timer resets whenever
    /// it elapses, so a busy system simply skips this cycle.
    pub fn should_rescan(&mut self, now: Instant, idle: bool) -> bool {
        if now.duration_since(self.last) < self.interval {
            return false;
        }
        self.last = now;
        idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_loadavg("0.42 0.30 0.25 1/234 5678"), Some(0.42));
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn parses_cpu_times() {
        // user nice system idle iowait irq softirq steal
        let content = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
        let cpu = parse_cpu(content).unwrap();
        assert_eq!(cpu.total, 1000);
        assert_eq!(cpu.idle, 850); // idle 800 + iowait 50
    }

    #[test]
    fn parses_diskstats_in_flight_skipping_partitions() {
        // major minor name reads ... field 12 is in-flight I/O.
        let content = "\
   8       0 sda 100 0 2000 50 200 0 4000 80 3 120 0
   8       1 sda1 50 0 1000 20 100 0 2000 40 2 60 0
 259       0 nvme0n1 10 0 100 5 20 0 200 6 1 30 0
 259       1 nvme0n1p1 5 0 50 2 10 0 100 3 4 15 0
";
        // sda (3) + nvme0n1 (1); partitions sda1 and nvme0n1p1 are skipped.
        assert_eq!(parse_diskstats(content), 4);
    }

    #[test]
    fn identifies_partitions() {
        assert!(is_partition("sda1"));
        assert!(is_partition("vdb2"));
        assert!(is_partition("nvme0n1p1"));
        assert!(is_partition("mmcblk0p1"));
        assert!(!is_partition("sda"));
        assert!(!is_partition("nvme0n1"));
        assert!(!is_partition("mmcblk0"));
    }

    #[test]
    fn snapshot_idle_gate() {
        let t = IdleThresholds {
            max_load1: 1.0,
            min_cpu_idle: 0.5,
            max_io_in_flight: 4,
        };
        let idle = IdleSnapshot {
            load1: 0.2,
            cpu_idle: 0.9,
            io_in_flight: 0,
        };
        assert!(idle.is_idle(&t));

        let busy_load = IdleSnapshot { load1: 4.0, ..idle };
        assert!(!busy_load.is_idle(&t));

        let busy_cpu = IdleSnapshot {
            cpu_idle: 0.1,
            ..idle
        };
        assert!(!busy_cpu.is_idle(&t));

        let busy_io = IdleSnapshot {
            io_in_flight: 10,
            ..idle
        };
        assert!(!busy_io.is_idle(&t));
    }

    #[test]
    fn scheduler_requires_interval_and_idle() {
        let mut s = RescanScheduler::new(Duration::from_secs(60));
        let t0 = Instant::now();

        // Too soon.
        assert!(!s.should_rescan(t0 + Duration::from_secs(10), true));

        // Interval elapsed but busy: no rescan, and the timer resets.
        assert!(!s.should_rescan(t0 + Duration::from_secs(61), false));

        // Next interval, idle: rescan.
        assert!(s.should_rescan(t0 + Duration::from_secs(122), true));
    }
}
