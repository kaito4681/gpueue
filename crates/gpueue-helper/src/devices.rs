//! GPU isolation: which device nodes a job unit may open.
//!
//! With `gpu_device_isolation = true` (the default) every job unit — spawned
//! or restored, with or without GPUs — gets `DevicePolicy=closed` and a
//! `DeviceAllow=` list (see [`job_device_allow`]). systemd turns that into a
//! cgroup device filter for every process of the unit, root included, so a
//! job can only open the NVIDIA device nodes of the GPUs it was assigned;
//! opening another GPU's `/dev/nvidia<minor>` fails with `EPERM`.
//! `closed` itself already allows the standard pseudo devices (`/dev/null`,
//! `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, `/dev/tty`,
//! `/dev/ptmx` and the `/dev/pts/*` pseudo terminals).
//!
//! gpueue addresses GPUs by index, in PCI bus order (`CUDA_DEVICE_ORDER=
//! PCI_BUS_ID`, the order of NVML and `nvidia-smi`), but the device node of
//! a GPU is named after its minor number, which need not match. The minors
//! come from the driver's `/proc/driver/nvidia/gpus/<bus>/information`
//! files (see [`read_gpu_minors`]); GPUs the driver excludes are skipped,
//! as they are by NVML and CUDA.

use std::fs;
use std::io;
use std::path::Path;

use gpueue_protocol::helper::GpuIndex;

/// Where the NVIDIA driver describes each GPU.
pub const NVIDIA_GPUS_DIR: &str = "/proc/driver/nvidia/gpus";

/// Read/write, no `mknod`.
pub const ACCESS: &str = "rw";

/// Device nodes every CUDA process needs besides its GPUs' own: the control
/// device, unified memory and the mode-setting device.
pub const NVIDIA_SHARED_NODES: &[&str] = &[
    "/dev/nvidiactl",
    "/dev/nvidia-uvm",
    "/dev/nvidia-uvm-tools",
    "/dev/nvidia-modeset",
];

/// `nvidia-uvm` by major number: covers `/dev/nvidia-uvm` and
/// `/dev/nvidia-uvm-tools` even when the driver creates them only after the
/// unit started (systemd skips a `DeviceAllow=` path that does not exist
/// yet).
pub const NVIDIA_UVM_CLASS: &str = "char-nvidia-uvm";

/// One `DeviceAllow=` entry: a device node path or `char-<group>`, and the
/// access (`r`, `w`, `m`).
pub type DeviceRule = (String, String);

/// The `DeviceAllow=` list of a job unit that was assigned `gpus`
/// (`DevicePolicy=closed` comes with it). `minors[i]` is the minor number
/// of GPU index `i` (see [`read_gpu_minors`]). A job without GPUs gets an
/// empty list: no NVIDIA device at all. An index without a known minor
/// (no such GPU on this host, e.g. a statically configured one that is not
/// there) adds no node of its own.
pub fn job_device_allow(gpus: &[GpuIndex], minors: &[u32]) -> Vec<DeviceRule> {
    let mut nodes: Vec<u32> = gpus
        .iter()
        .filter_map(|g| minors.get(g.0 as usize).copied())
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    if gpus.is_empty() {
        return Vec::new();
    }
    let mut rules: Vec<DeviceRule> = nodes
        .into_iter()
        .map(|m| (format!("/dev/nvidia{m}"), ACCESS.to_owned()))
        .collect();
    rules.extend(
        NVIDIA_SHARED_NODES
            .iter()
            .map(|n| ((*n).to_owned(), ACCESS.to_owned())),
    );
    rules.push((NVIDIA_UVM_CLASS.to_owned(), ACCESS.to_owned()));
    rules
}

/// What one `information` file says about a GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuNode {
    /// `Bus Location`, e.g. `0000:03:00.0`.
    pub bus: String,
    /// `Device Minor`: the GPU's node is `/dev/nvidia<minor>`.
    pub minor: u32,
    /// `GPU Excluded: Yes`: unusable, not enumerated by NVML or CUDA.
    pub excluded: bool,
}

/// Parses `/proc/driver/nvidia/gpus/<bus>/information`; `bus` (the
/// directory name) stands in for a missing `Bus Location`.
pub fn parse_gpu_information(bus: &str, text: &str) -> Option<GpuNode> {
    let mut node = GpuNode {
        bus: bus.to_ascii_lowercase(),
        minor: u32::MAX,
        excluded: false,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Bus Location" if !value.is_empty() => node.bus = value.to_ascii_lowercase(),
            "Device Minor" => node.minor = value.parse().ok()?,
            "GPU Excluded" => node.excluded = value.eq_ignore_ascii_case("yes"),
            _ => {}
        }
    }
    (node.minor != u32::MAX).then_some(node)
}

/// Minor numbers by GPU index: the usable GPUs in PCI bus order.
pub fn minors_by_index(mut nodes: Vec<GpuNode>) -> Vec<u32> {
    nodes.retain(|n| !n.excluded);
    // Bus locations have fixed-width hex fields, so text order is bus order.
    nodes.sort_by(|a, b| a.bus.cmp(&b.bus));
    nodes.into_iter().map(|n| n.minor).collect()
}

/// [`minors_by_index`] of the GPUs under `dir` ([`NVIDIA_GPUS_DIR`]). No
/// such directory (no NVIDIA driver loaded) means no GPUs.
pub fn read_gpu_minors(dir: &Path) -> io::Result<Vec<u32>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let bus = entry.file_name().to_string_lossy().into_owned();
        let text = fs::read_to_string(entry.path().join("information"))?;
        let node = parse_gpu_information(&bus, &text).ok_or_else(|| {
            io::Error::other(format!(
                "{}: no Device Minor",
                entry.path().join("information").display()
            ))
        })?;
        nodes.push(node);
    }
    Ok(minors_by_index(nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "Model: \t\t NVIDIA GeForce RTX 5070 Ti\nIRQ:   \t\t 16\n\
        GPU UUID: \t GPU-34b92e76-cfbc-b424-be8f-8b626a4dbbce\nBus Type: \t PCIe\n\
        Bus Location: \t 0000:03:00.0\nDevice Minor: \t 0\nGPU Excluded:\t No\n";

    fn rules(list: &[(&str, &str)]) -> Vec<DeviceRule> {
        list.iter()
            .map(|(p, a)| ((*p).to_owned(), (*a).to_owned()))
            .collect()
    }

    #[test]
    fn parses_the_driver_information() {
        assert_eq!(
            parse_gpu_information("0000:03:00.0", INFO),
            Some(GpuNode {
                bus: "0000:03:00.0".into(),
                minor: 0,
                excluded: false
            })
        );
        let excluded = INFO.replace("Excluded:\t No", "Excluded:\t Yes");
        assert!(parse_gpu_information("x", &excluded).unwrap().excluded);
        assert_eq!(parse_gpu_information("x", "Model: X\n"), None);
        assert_eq!(parse_gpu_information("x", "Device Minor: ?\n"), None);
        // Older drivers: no Bus Location line.
        let node = parse_gpu_information("0000:0A:00.0", "Device Minor: 3\n").unwrap();
        assert_eq!(node.bus, "0000:0a:00.0");
    }

    #[test]
    fn indexes_gpus_in_bus_order_skipping_excluded_ones() {
        let node = |bus: &str, minor, excluded| GpuNode {
            bus: bus.into(),
            minor,
            excluded,
        };
        let minors = minors_by_index(vec![
            node("0000:81:00.0", 0, false),
            node("0000:03:00.0", 2, false),
            node("0000:41:00.0", 1, true),
            node("0001:01:00.0", 3, false),
        ]);
        assert_eq!(minors, [2, 0, 3]);
    }

    #[test]
    fn reads_the_driver_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_gpu_minors(&tmp.path().join("missing")).unwrap(),
            Vec::<u32>::new()
        );
        for (bus, minor) in [("0000:65:00.0", 1), ("0000:17:00.0", 4)] {
            let dir = tmp.path().join(bus);
            fs::create_dir(&dir).unwrap();
            let text = INFO
                .replace("0000:03:00.0", bus)
                .replace("Minor: \t 0", &format!("Minor: \t {minor}"));
            fs::write(dir.join("information"), text).unwrap();
        }
        assert_eq!(read_gpu_minors(tmp.path()).unwrap(), [4, 1]);
        fs::write(tmp.path().join("0000:65:00.0/information"), "garbage").unwrap();
        assert!(read_gpu_minors(tmp.path()).is_err());
    }

    #[test]
    fn cpu_jobs_get_no_nvidia_device() {
        assert!(job_device_allow(&[], &[0, 1]).is_empty());
    }

    #[test]
    fn gpu_jobs_get_their_nodes_by_minor() {
        let shared = [
            ("/dev/nvidiactl", "rw"),
            ("/dev/nvidia-uvm", "rw"),
            ("/dev/nvidia-uvm-tools", "rw"),
            ("/dev/nvidia-modeset", "rw"),
            ("char-nvidia-uvm", "rw"),
        ];
        let mut want = rules(&[("/dev/nvidia0", "rw")]);
        want.extend(rules(&shared));
        assert_eq!(job_device_allow(&[GpuIndex(0)], &[0]), want);

        // Index 1 is minor 3, index 2 minor 0.
        let mut want = rules(&[("/dev/nvidia0", "rw"), ("/dev/nvidia3", "rw")]);
        want.extend(rules(&shared));
        assert_eq!(
            job_device_allow(&[GpuIndex(2), GpuIndex(1), GpuIndex(1)], &[5, 3, 0]),
            want
        );

        // A GPU unknown to the driver: only the shared nodes.
        assert_eq!(job_device_allow(&[GpuIndex(1)], &[0]), rules(&shared));
    }
}
