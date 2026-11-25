//! Load of the host, read from `/proc` and `/sys`, and of its GPUs, read from `nvidia-smi`.
//!
//! Nothing here needs a privilege. `nvidia-smi` is optional: on a host or in an image that
//! lacks it the GPU figures are simply absent, never guessed.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use reverse_core::status::{GpuStatus, HostStatus, UNKNOWN, UNKNOWN_F64};
use tokio::process::Command;

use crate::hardware::meminfo_kib;

const NVIDIA_SMI: &str = "nvidia-smi";
const NVIDIA_SMI_QUERY: &str =
    "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw";
const NVIDIA_SMI_BUDGET: Duration = Duration::from_secs(4);

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

/// Asks `nvidia-smi` about every GPU.
///
/// Returns an empty list when the tool is absent or fails: the figures are optional, and the
/// caller keeps asking so a driver that comes up late is picked up.
pub async fn read_gpus(unified_memory: bool, host_memory_percent: i32) -> Vec<GpuStatus> {
    match query_nvidia_smi().await {
        Ok(csv) => parse_nvidia_smi(&csv, unified_memory, host_memory_percent),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("gpu.nvidia_smi_absent");
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(%error, "gpu.nvidia_smi_failed");
            Vec::new()
        }
    }
}

async fn query_nvidia_smi() -> io::Result<String> {
    let mut command = Command::new(NVIDIA_SMI);
    command
        .args([NVIDIA_SMI_QUERY, "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(NVIDIA_SMI_BUDGET, command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "nvidia-smi timed out"))??;
    if !output.status.success() {
        return Err(io::Error::other("nvidia-smi exited with an error"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses `name, util %, mem used MiB, mem total MiB, temp C, power W`; `[N/A]` is unknown.
///
/// A unified-memory GPU reports no memory of its own: its memory is the system's, so the host
/// share stands in and the entry says so.
fn parse_nvidia_smi(csv: &str, unified_memory: bool, host_memory_percent: i32) -> Vec<GpuStatus> {
    csv.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let [name, util, used, total, temp, power] = fields[..] else {
                return None;
            };
            let number = |raw: &str| raw.parse::<f64>().ok().filter(|v| v.is_finite());
            let memory = match (number(used), number(total)) {
                (Some(used), Some(total)) if total > 0.0 => Some(percent(used / total * 100.0)),
                _ => None,
            };
            Some(GpuStatus {
                name: name.to_owned(),
                utilization_percent: number(util).map_or(UNKNOWN, percent),
                memory_used_percent: match (memory, unified_memory) {
                    (Some(memory), _) => memory,
                    (None, true) => host_memory_percent,
                    (None, false) => UNKNOWN,
                },
                memory_shared: memory.is_none() && unified_memory,
                temperature_c: number(temp).map_or(UNKNOWN, percent),
                power_watts: number(power).map_or(UNKNOWN_F64, |watts| watts.max(0.0)),
            })
        })
        .collect()
}

/// Rounds a reading to a whole number; used for percentages and degrees alike.
#[allow(clippy::cast_possible_truncation)]
fn percent(value: f64) -> i32 {
    value.round().clamp(0.0, f64::from(i32::MAX)) as i32
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

    // Verbatim from a Dell Pro Max with GB10 and from a dual RTX 4090 host.
    #[test]
    fn a_unified_memory_gpu_borrows_the_host_memory_share() {
        let gpus = parse_nvidia_smi("NVIDIA GB10, 0, [N/A], [N/A], 36, 6.86\n", true, 41);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].memory_used_percent, 41);
        assert!(gpus[0].memory_shared);
        assert_eq!(gpus[0].temperature_c, 36);
        assert!((gpus[0].power_watts - 6.86).abs() < f64::EPSILON);
    }

    #[test]
    fn discrete_gpus_report_their_own_memory() {
        let gpus = parse_nvidia_smi(
            "NVIDIA GeForce RTX 4090, 12, 1088, 24564, 25, 26.75\nNVIDIA GeForce RTX 4090, 0, 12, 24564, 26, 6.53\n",
            false,
            50,
        );
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].utilization_percent, 12);
        assert_eq!(gpus[0].memory_used_percent, 4);
        assert!(!gpus[0].memory_shared);
    }

    #[test]
    fn a_discrete_gpu_with_no_memory_reading_stays_unknown() {
        let gpus = parse_nvidia_smi("RTX, [N/A], [N/A], [N/A], [N/A], [N/A]\n", false, 50);
        assert_eq!(gpus[0].memory_used_percent, UNKNOWN);
        assert_eq!(gpus[0].utilization_percent, UNKNOWN);
        assert!(gpus[0].power_watts < 0.0);
    }

    #[test]
    fn a_malformed_line_is_skipped() {
        assert!(parse_nvidia_smi("garbage\n", false, 0).is_empty());
    }
}
