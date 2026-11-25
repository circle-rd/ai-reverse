//! Reads what machine the agent runs on from `/proc` and `/sys`; nothing here needs a privilege.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use reverse_core::wire::{AgentInfo, HardwareInfo, HardwareKind};

/// The facts about this process and its host, read once at startup.
#[must_use]
pub fn collect() -> AgentInfo {
    AgentInfo {
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        hardware: detect(Path::new("/")),
    }
}

/// Reads the host description below `root`, which is `/` outside tests.
#[must_use]
pub fn detect(root: &Path) -> HardwareInfo {
    let read = |relative: &str| fs::read_to_string(root.join(relative)).ok();
    let product = read("sys/class/dmi/id/product_name").and_then(clean);
    let vendor = read("sys/class/dmi/id/sys_vendor").and_then(clean);
    let gpus = gpu_models(&root.join("proc/driver/nvidia/gpus"));
    let kind = classify(product.as_deref(), &gpus);
    HardwareInfo {
        kind,
        product,
        vendor,
        gpus,
        memory_mb: read("proc/meminfo")
            .and_then(|text| meminfo_kib(&text, "MemTotal"))
            .map_or(0, |kib| kib / 1024),
        cpu_arch: std::env::consts::ARCH.to_owned(),
        cpu_cores: read("proc/cpuinfo").map_or(0, |text| count_processors(&text)),
    }
}

fn clean(raw: String) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn gpu_models(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut models: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path().join("information")).ok())
        .filter_map(|text| gpu_model(&text))
        .collect();
    models.sort();
    models
}

/// Extracts the `Model:` line of `/proc/driver/nvidia/gpus/*/information`. The driver reports
/// `Unknown` for a GPU it cannot name (unified-memory GB10), which says nothing and is dropped.
fn gpu_model(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("Model:"))
        .and_then(|model| clean(model.to_owned()))
        .filter(|model| !model.eq_ignore_ascii_case("unknown"))
}

/// Reads one `Key:   N kB` line of `/proc/meminfo`.
pub(crate) fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn count_processors(cpuinfo: &str) -> u32 {
    let count = cpuinfo
        .lines()
        .filter(|line| {
            line.split(':')
                .next()
                .is_some_and(|k| k.trim() == "processor")
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// GB10 is the chip of every DGX Spark and of its OEM siblings; the driver may not name it, so
/// the product name (for example `Dell Pro Max with GB10`) is checked as well as the GPU.
fn classify(product: Option<&str>, gpus: &[String]) -> HardwareKind {
    let names_gb10 = |text: &str| text.to_ascii_uppercase().contains("GB10");
    let spark = product
        .is_some_and(|p| p.to_ascii_lowercase().contains("dgx spark") || names_gb10(p))
        || gpus.iter().any(|g| names_gb10(g));
    if spark {
        HardwareKind::DgxSpark
    } else {
        HardwareKind::Other
    }
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
    fn a_gb10_host_is_read_from_proc_and_sys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sys/class/dmi/id/product_name", "NVIDIA DGX Spark\n");
        write(root, "sys/class/dmi/id/sys_vendor", "NVIDIA\n");
        write(
            root,
            "proc/meminfo",
            "MemTotal:      125829120 kB\nMemFree: 1 kB\n",
        );
        write(
            root,
            "proc/cpuinfo",
            "processor\t: 0\nmodel\t: x\nprocessor\t: 1\n",
        );
        write(
            root,
            "proc/driver/nvidia/gpus/000f:01:00.0/information",
            "Model: \t\t NVIDIA GB10\nIRQ:   \t\t 12\n",
        );
        let info = detect(root);
        assert_eq!(info.kind, HardwareKind::DgxSpark);
        assert_eq!(info.product.as_deref(), Some("NVIDIA DGX Spark"));
        assert_eq!(info.vendor.as_deref(), Some("NVIDIA"));
        assert_eq!(info.gpus, ["NVIDIA GB10"]);
        assert_eq!(info.memory_mb, 122_880);
        assert_eq!(info.cpu_cores, 2);
    }

    #[test]
    fn an_oem_gb10_is_recognised_by_its_gpu() {
        let gpus = vec!["NVIDIA GB10".to_owned()];
        assert_eq!(
            classify(Some("ASUS Ascent GX10"), &gpus),
            HardwareKind::DgxSpark
        );
    }

    #[test]
    fn an_oem_gb10_the_driver_cannot_name_is_recognised_by_its_product() {
        assert_eq!(
            classify(Some("Dell Pro Max with GB10 FCM1253"), &[]),
            HardwareKind::DgxSpark
        );
        assert_eq!(classify(Some("PowerEdge R760"), &[]), HardwareKind::Other);
    }

    #[test]
    fn a_gpu_the_driver_reports_as_unknown_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "proc/driver/nvidia/gpus/000f:01:00.0/information",
            "Model: \t\t Unknown\nIRQ:   \t\t 12\n",
        );
        assert!(detect(dir.path()).gpus.is_empty());
    }

    #[test]
    fn an_unknown_or_empty_host_is_other_with_zeroes() {
        let dir = tempfile::tempdir().unwrap();
        let info = detect(dir.path());
        assert_eq!(info.kind, HardwareKind::Other);
        assert!(info.gpus.is_empty());
        assert_eq!(info.memory_mb, 0);
        assert_eq!(info.cpu_cores, 0);
        assert!(info.product.is_none());
    }

    #[test]
    fn meminfo_without_a_total_is_ignored() {
        assert_eq!(meminfo_kib("MemFree: 1 kB\n", "MemTotal"), None);
        assert_eq!(meminfo_kib("MemTotal: abc kB\n", "MemTotal"), None);
        assert_eq!(meminfo_kib("MemTotal: 4 kB\n", "MemTotal"), Some(4));
    }
}
