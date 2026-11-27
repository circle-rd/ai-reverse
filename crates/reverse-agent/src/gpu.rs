//! Reads GPU load and the processes holding each GPU from `nvtop -s`, the JSON snapshot of the
//! NVTOP monitor.
//!
//! One tool covers NVIDIA (x86 and ARM), AMD and Intel with one format. The agent runs the
//! first `nvtop` on its `PATH`: the GPU image ships the one it needs, and a natively installed
//! agent uses the host's. A container cannot borrow the host's binary, which is linked against
//! the host's libraries.
//!
//! The figures are device-wide. The processes are visible only when the agent shares the host's
//! PID namespace (`pid: host`); otherwise the list is empty. A process is named from
//! `/proc/<pid>/comm`, the kernel's executable name, and the command line that nvtop also
//! prints is never deserialized: it can carry secrets such as `--api-key`. Two values are
//! deliberately not taken at face value:
//!
//! - `mem_util` is the memory controller activity on NVIDIA with nvtop before 3.3, not the
//!   memory in use. Memory use is computed from `mem_used` and `mem_total` only, so an old
//!   nvtop yields an unknown figure rather than a wrong one.
//! - On a unified-memory GB10 the snapshot's memory is derived from the GPU processes the
//!   caller can see, which is nothing inside a container. The system memory share stands in.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use reverse_core::status::{GpuProcess, GpuStatus, UNKNOWN, UNKNOWN_F64};
use serde::Deserialize;
use tokio::process::Command;

const NVTOP: &str = "nvtop";
const SEARCH_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const BUDGET: Duration = Duration::from_secs(4);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_NAME_CHARS: usize = 64;
const MIB: u64 = 1024 * 1024;

/// Snapshots every GPU the process can see.
///
/// Returns an empty list when `nvtop` is absent, too old for `-s` or fails: the figures are
/// optional, and the caller asks again so a driver that comes up late is picked up.
pub async fn read(unified_memory: bool, host_memory_percent: i32) -> Vec<GpuStatus> {
    match snapshot().await {
        Ok(json) => match decode(&json) {
            Some(devices) => {
                let pids: Vec<u32> = devices
                    .iter()
                    .flat_map(|device| &device.processes)
                    .filter_map(|process| process.pid.trim().parse().ok())
                    .collect();
                let names = process_names(pids).await;
                to_status(devices, unified_memory, host_memory_percent, &names)
            }
            None => {
                tracing::debug!("gpu.nvtop_unparsable");
                Vec::new()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!("gpu.nvtop_absent");
            Vec::new()
        }
        Err(error) => {
            tracing::debug!(%error, "gpu.nvtop_failed");
            Vec::new()
        }
    }
}

async fn snapshot() -> io::Result<String> {
    let mut command = Command::new(NVTOP);
    // The agent's environment holds its credentials; the tool needs none of it. The search path
    // is set explicitly: without it the lookup falls back to /bin:/usr/bin, which misses the
    // /usr/local/bin install of the GPU image.
    command
        .arg("-s")
        .env_clear()
        .env("PATH", SEARCH_PATH)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(BUDGET, command.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "nvtop timed out"))??;
    if !output.status.success() {
        return Err(io::Error::other("nvtop exited with an error"));
    }
    if output.stdout.len() > MAX_OUTPUT_BYTES {
        return Err(io::Error::other("nvtop output is too large"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The fields of one device that are used; every other key is skipped, `cmdline` included.
#[derive(Deserialize)]
struct Device {
    device_name: String,
    temp: Option<String>,
    power_draw: Option<String>,
    gpu_util: Option<String>,
    mem_used: Option<String>,
    mem_total: Option<String>,
    #[serde(default)]
    processes: Vec<Process>,
}

#[derive(Deserialize)]
struct Process {
    pid: String,
    gpu_mem_bytes_alloc: Option<String>,
}

fn decode(json: &str) -> Option<Vec<Device>> {
    serde_json::from_str(json).ok()
}

/// Names the processes from the kernel's `comm`, read in one blocking pass off the runtime.
async fn process_names(pids: Vec<u32>) -> HashMap<u32, String> {
    tokio::task::spawn_blocking(move || {
        pids.into_iter()
            .filter_map(|pid| {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
                Some((pid, clean_name(&comm)?))
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn to_status(
    devices: Vec<Device>,
    unified_memory: bool,
    host_memory_percent: i32,
    names: &HashMap<u32, String>,
) -> Vec<GpuStatus> {
    devices
        .into_iter()
        .map(|device| {
            let memory = memory_percent(device.mem_used.as_deref(), device.mem_total.as_deref());
            GpuStatus {
                name: device
                    .device_name
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect(),
                processes: device
                    .processes
                    .iter()
                    .filter_map(|process| process_of(process, names))
                    .collect(),
                utilization_percent: percent(device.gpu_util.as_deref()),
                memory_used_percent: if unified_memory {
                    host_memory_percent
                } else {
                    memory.unwrap_or(UNKNOWN)
                },
                memory_shared: unified_memory,
                temperature_c: reading(device.temp.as_deref(), "C", 0.0, 150.0)
                    .map_or(UNKNOWN, round),
                power_watts: reading(device.power_draw.as_deref(), "W", 0.0, 5000.0)
                    .unwrap_or(UNKNOWN_F64),
            }
        })
        .collect()
}

fn process_of(raw: &Process, names: &HashMap<u32, String>) -> Option<GpuProcess> {
    let pid: u32 = raw.pid.trim().parse().ok()?;
    Some(GpuProcess {
        pid,
        name: names.get(&pid).cloned(),
        memory_mib: memory_mib(raw.gpu_mem_bytes_alloc.as_deref()),
    })
}

/// Keeps the printable part of a kernel name, bounded for display.
fn clean_name(comm: &str) -> Option<String> {
    let name: String = comm
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Parses a byte count such as `"3036069888"`; nvtop writes other units for some cards, which
/// are unknown rather than guessed.
fn memory_mib(raw: Option<&str>) -> i32 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(UNKNOWN, |bytes| {
            i32::try_from(bytes / MIB).unwrap_or(i32::MAX)
        })
}

/// Parses `"36C"`, `"6W"`, `"2%"`; `null`, `"N/A"` and out-of-range values are unknown.
fn reading(raw: Option<&str>, unit: &str, min: f64, max: f64) -> Option<f64> {
    raw?.trim()
        .strip_suffix(unit)?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (min..=max).contains(value))
}

fn percent(raw: Option<&str>) -> i32 {
    reading(raw, "%", 0.0, 100.0).map_or(UNKNOWN, round)
}

#[allow(clippy::cast_possible_truncation)]
fn round(value: f64) -> i32 {
    value.round() as i32
}

/// Memory in use as a share of the total, from byte counts; absent from an old nvtop.
fn memory_percent(used: Option<&str>, total: Option<&str>) -> Option<i32> {
    let used: u128 = used?.trim().parse().ok()?;
    let total: u128 = total?.trim().parse().ok().filter(|total| *total > 0)?;
    i32::try_from((used * 100 / total).min(100)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim `nvtop -s` output, trimmed of nothing but the process list of the first sample.
    const GB10_3_4: &str = r#"[
  {
   "device_name": "NVIDIA GB10",
   "device_architecture": "Blackwell",
   "memory_type": null,
   "gpu_clock": "1982MHz",
   "temp": "36C",
   "fan_speed": null,
   "power_draw": "6W",
   "gpu_util": "2%",
   "mem_util": "82%",
   "mem_total": "130593935360",
   "mem_used": "107683545088",
   "mem_free": "9366863872",
   "processes" : [
     {
       "pid": "1336019",
       "cmdline": "/app/llama-server --api-key-file /out/api-key --host 127.0.0.1",
       "kind": "compute",
       "gpu_mem_bytes_alloc": "3036069888"
     }
   ]
  }
]"#;

    const GB10_3_2: &str = r#"[{"device_name": "NVIDIA GB10", "gpu_clock": "1982MHz", "mem_clock": null,
   "temp": "36C", "fan_speed": null, "power_draw": "6W", "gpu_util": "0%", "mem_util": null}]"#;

    const RTX4090_3_4: &str = r#"[
  {"device_name": "NVIDIA GeForce RTX 4090", "memory_type": null, "temp": "24C", "power_draw": "26W",
   "gpu_util": "0%", "mem_util": "0%", "mem_total": "25757220864", "mem_used": "983040",
   "mem_free": "25251741696"},
  {"device_name": "NVIDIA GeForce RTX 4090", "temp": "24C", "power_draw": "6W", "gpu_util": "0%",
   "mem_util": "0%", "mem_total": "25757220864", "mem_used": "9371648", "mem_free": "25240207360"}
]"#;

    // 9 MiB used of 24 GiB, yet `mem_util` reads 2 %: it is the memory controller activity.
    const RTX4090_3_2: &str = r#"[
  {"device_name": "NVIDIA GeForce RTX 4090", "gpu_clock": "210MHz", "temp": "24C", "fan_speed": "0%",
   "power_draw": "26W", "gpu_util": "0%", "mem_util": "1%"},
  {"device_name": "NVIDIA GeForce RTX 4090", "temp": "24C", "power_draw": "6W", "gpu_util": "0%",
   "mem_util": "2%"}
]"#;

    const AMD_3_2: &str = r#"[{"device_name": "AMD Radeon Graphics", "gpu_clock": "607MHz",
   "temp": "29C", "fan_speed": "CPU Fan", "power_draw": "4W", "gpu_util": "0%", "mem_util": "3%"}]"#;

    fn parse(json: &str, unified: bool, host: i32) -> Vec<GpuStatus> {
        to_status(
            decode(json).expect("fixture decodes"),
            unified,
            host,
            &HashMap::new(),
        )
    }

    #[test]
    fn a_unified_memory_gpu_reports_the_system_memory_share_not_the_process_sum() {
        let gpus = parse(GB10_3_4, true, 41);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "NVIDIA GB10");
        assert_eq!(gpus[0].memory_used_percent, 41);
        assert!(gpus[0].memory_shared);
        assert_eq!(gpus[0].utilization_percent, 2);
        assert_eq!(gpus[0].temperature_c, 36);
        assert!((gpus[0].power_watts - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_old_nvtop_on_a_gb10_still_reports_the_system_share() {
        assert_eq!(parse(GB10_3_2, true, 55)[0].memory_used_percent, 55);
    }

    #[test]
    fn discrete_memory_comes_from_byte_counts() {
        let gpus = parse(RTX4090_3_4, false, 50);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[1].memory_used_percent, 0);
        assert!(!gpus[1].memory_shared);
        assert_eq!(
            memory_percent(Some("12884901888"), Some("25757220864")),
            Some(50)
        );
    }

    #[test]
    fn an_old_nvtop_never_turns_memory_activity_into_memory_use() {
        for gpu in parse(RTX4090_3_2, false, 50) {
            assert_eq!(gpu.memory_used_percent, UNKNOWN);
        }
        assert_eq!(parse(AMD_3_2, false, 50)[0].memory_used_percent, UNKNOWN);
    }

    #[test]
    fn amd_reports_its_load_temperature_and_power() {
        let gpu = &parse(AMD_3_2, false, 0)[0];
        assert_eq!(gpu.name, "AMD Radeon Graphics");
        assert_eq!((gpu.temperature_c, gpu.utilization_percent), (29, 0));
        assert!((gpu.power_watts - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unreadable_or_absurd_values_are_unknown() {
        let gpus = parse(
            r#"[{"device_name":"X","temp":"N/A","power_draw":null,"gpu_util":"140%"}]"#,
            false,
            0,
        );
        assert_eq!(gpus[0].temperature_c, UNKNOWN);
        assert_eq!(gpus[0].utilization_percent, UNKNOWN);
        assert!(gpus[0].power_watts < 0.0);
    }

    #[test]
    fn output_that_is_not_a_device_list_yields_no_gpu() {
        assert!(decode("No GPU to monitor.").is_none());
        assert!(parse("[]", false, 0).is_empty());
    }

    #[test]
    fn a_process_is_named_from_its_kernel_name_and_its_memory_is_in_mib() {
        let names = HashMap::from([(1_336_019, "llama-server".to_owned())]);
        let gpus = to_status(decode(GB10_3_4).unwrap(), true, 41, &names);
        assert_eq!(
            gpus[0].processes,
            vec![GpuProcess {
                pid: 1_336_019,
                name: Some("llama-server".to_owned()),
                memory_mib: 2895,
            }]
        );
    }

    #[test]
    fn the_command_line_never_reaches_the_status() {
        let names = HashMap::from([(1_336_019, "llama-server".to_owned())]);
        let gpus = to_status(decode(GB10_3_4).unwrap(), true, 41, &names);
        let json = serde_json::to_string(&gpus).unwrap();
        assert!(!json.contains("api-key"));
        assert!(!json.contains("--host"));
    }

    #[test]
    fn a_process_without_a_readable_name_or_memory_is_kept_with_unknowns() {
        let gpus = to_status(
            decode(
                r#"[{"device_name":"X","processes":[{"pid":"42","cmdline":null,"gpu_mem_bytes_alloc":"0.01 MiB"}]}]"#,
            )
            .unwrap(),
            false,
            0,
            &HashMap::new(),
        );
        assert_eq!(
            gpus[0].processes,
            vec![GpuProcess {
                pid: 42,
                name: None,
                memory_mib: UNKNOWN,
            }]
        );
    }

    #[test]
    fn a_device_without_processes_has_an_empty_list() {
        assert!(parse(RTX4090_3_2, false, 0)[0].processes.is_empty());
    }

    #[test]
    fn kernel_names_drop_control_characters_and_are_bounded() {
        assert_eq!(
            clean_name("llama-server\n").as_deref(),
            Some("llama-server")
        );
        assert_eq!(clean_name("\n").as_deref(), None);
        assert_eq!(clean_name(&"x".repeat(200)).unwrap().chars().count(), 64);
    }
}
