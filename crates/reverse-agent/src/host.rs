//! Load of the host, read from `/proc` and `/sys`. Nothing here needs a privilege.

use std::fs;
use std::path::Path;

use reverse_core::status::{HostStatus, UNKNOWN};

use crate::hardware::meminfo_kib;

/// Cumulative CPU time of the previous sample, the base of the next load figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    busy: u64,
    total: u64,
}

/// Reads the host below `root`, which is `/` outside tests.
///
/// `previous` carries the CPU counters between samples; the first sample has no base and
/// reports the CPU load as unknown.
#[must_use]
pub fn read_host(root: &Path, previous: &mut Option<CpuTimes>) -> HostStatus {
    let read = |relative: &str| fs::read_to_string(root.join(relative)).ok();
    let now = read("proc/stat").and_then(|text| parse_cpu_times(&text));
    let cpu_load_percent = match (previous.as_ref(), now) {
        (Some(before), Some(after)) => cpu_load(*before, after),
        _ => UNKNOWN,
    };
    if now.is_some() {
        *previous = now;
    }
    HostStatus {
        uptime_secs: read("proc/uptime")
            .and_then(|text| parse_uptime(&text))
            .unwrap_or(0),
        cpu_load_percent,
        memory_used_percent: read("proc/meminfo")
            .and_then(|text| memory_used_percent(&text))
            .unwrap_or(UNKNOWN),
        cpu_temperature_c: hottest_zone_c(&root.join("sys/class/thermal")),
    }
}

fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().next()?.strip_prefix("cpu ")?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // user nice system idle iowait irq softirq steal: idle and iowait are not work.
    let (idle, iowait) = (*fields.get(3)?, *fields.get(4)?);
    let total: u64 = fields.iter().take(8).sum();
    Some(CpuTimes {
        busy: total.saturating_sub(idle).saturating_sub(iowait),
        total,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn cpu_load(before: CpuTimes, after: CpuTimes) -> i32 {
    let total = after.total.saturating_sub(before.total);
    if total == 0 {
        return UNKNOWN;
    }
    let busy = after.busy.saturating_sub(before.busy);
    (busy.saturating_mul(100) / total).min(100) as i32
}

fn parse_uptime(text: &str) -> Option<u64> {
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds as u64)
}

#[allow(clippy::cast_possible_truncation)]
fn memory_used_percent(meminfo: &str) -> Option<i32> {
    let total = meminfo_kib(meminfo, "MemTotal").filter(|total| *total > 0)?;
    let available = meminfo_kib(meminfo, "MemAvailable")?;
    let used = total.saturating_sub(available);
    Some((used.saturating_mul(100) / total).min(100) as i32)
}

/// The hottest zone, which is the one that throttles first. Zones are milli-degrees Celsius.
fn hottest_zone_c(dir: &Path) -> i32 {
    let Ok(entries) = fs::read_dir(dir) else {
        return UNKNOWN;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("thermal_zone")
        })
        .filter_map(|entry| fs::read_to_string(entry.path().join("temp")).ok())
        .filter_map(|text| text.trim().parse::<i32>().ok())
        .filter(|milli| *milli > 0)
        .max()
        .map_or(UNKNOWN, |milli| milli / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn cpu_load_needs_two_samples() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "proc/stat", "cpu  100 0 100 700 100 0 0 0 0 0\n");
        let mut previous = None;
        assert_eq!(read_host(root, &mut previous).cpu_load_percent, UNKNOWN);
        write(root, "proc/stat", "cpu  200 0 200 800 100 0 0 0 0 0\n");
        // 200 busy ticks out of 300 elapsed.
        assert_eq!(read_host(root, &mut previous).cpu_load_percent, 66);
    }

    #[test]
    fn memory_and_uptime_are_read_from_proc() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "proc/meminfo",
            "MemTotal: 1000 kB\nMemFree: 10 kB\nMemAvailable: 250 kB\n",
        );
        write(root, "proc/uptime", "1155267.44 22489995.48\n");
        let host = read_host(root, &mut None);
        assert_eq!(host.memory_used_percent, 75);
        assert_eq!(host.uptime_secs, 1_155_267);
    }

    #[test]
    fn the_hottest_thermal_zone_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/thermal/thermal_zone0/temp", "37300\n");
        write(root, "sys/class/thermal/thermal_zone1/temp", "61250\n");
        write(root, "sys/class/thermal/cooling_device0/temp", "99000\n");
        assert_eq!(read_host(root, &mut None).cpu_temperature_c, 61);
    }

    #[test]
    fn a_host_without_proc_reports_everything_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let host = read_host(dir.path(), &mut None);
        assert_eq!(host.cpu_load_percent, UNKNOWN);
        assert_eq!(host.memory_used_percent, UNKNOWN);
        assert_eq!(host.cpu_temperature_c, UNKNOWN);
    }
}
