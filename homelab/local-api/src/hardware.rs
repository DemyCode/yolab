use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

pub const LABEL_NVIDIA: &str = "yolab.io/gpu-nvidia";
pub const LABEL_NVIDIA_LEGACY: &str = "yolab.io/gpu-nvidia-legacy";
pub const LABEL_AMD: &str = "yolab.io/gpu-amd";
pub const LABEL_AMD_ROCM: &str = "yolab.io/gpu-amd-rocm";
pub const LABEL_INTEL: &str = "yolab.io/gpu-intel";
pub const LABEL_INTEL_COMPUTE: &str = "yolab.io/gpu-intel-compute";
pub const LABEL_ACCELERATOR: &str = "yolab.io/accelerator";
pub const LABEL_VRAM_GIB: &str = "yolab.io/vram-gib";
pub const LABEL_RAM_GIB: &str = "yolab.io/ram-gib";
pub const LABEL_GAME_INPUT: &str = "yolab.io/game-input";

const NVIDIA_CDI_SPEC: &str = "var/run/cdi/nvidia-container-toolkit.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub nvidia: bool,
    pub nvidia_legacy: bool,
    pub amd: bool,
    pub amd_rocm: bool,
    pub amd_vulkan: bool,
    pub intel: bool,
    pub intel_compute: bool,
    pub vram_bytes: Option<u64>,
    pub ram_bytes: Option<u64>,
    pub game_input: bool,
}

impl Inventory {
    pub fn accelerator(&self) -> &'static str {
        if self.nvidia {
            "nvidia"
        } else if self.amd_rocm {
            "amd"
        } else if self.intel_compute {
            "intel"
        } else if self.amd_vulkan {
            "vulkan"
        } else if self.nvidia_legacy {
            "nvidia-legacy"
        } else {
            "cpu"
        }
    }
}

const FIRST_MAXWELL_DEVICE_ID: u16 = 0x1340;

fn nvidia_serves_apps(drm: &[DrmDevice]) -> bool {
    drm.iter()
        .filter(|d| d.driver == "nvidia")
        .filter_map(|d| d.device_id)
        .all(|id| id >= FIRST_MAXWELL_DEVICE_ID)
}

const KFD_NODES: &str = "sys/class/kfd/kfd/topology/nodes";
const FIRST_ROCM_GFX: u64 = 90000;

fn gfx_target_versions(root: &Path) -> Option<Vec<u64>> {
    let nodes = std::fs::read_dir(root.join(KFD_NODES)).ok()?;
    Some(
        nodes
            .filter_map(|n| n.ok())
            .filter_map(|n| std::fs::read_to_string(n.path().join("properties")).ok())
            .filter_map(|text| {
                text.lines().find_map(|l| {
                    l.strip_prefix("gfx_target_version ")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                })
            })
            .filter(|v| *v > 0)
            .collect(),
    )
}

pub fn probe(root: &Path) -> Inventory {
    let drm = drm_devices(root);
    let amd_vulkan = drm.iter().any(|d| d.driver == "amdgpu" && d.render_node);
    let nvidia_driven =
        root.join(NVIDIA_CDI_SPEC).is_file() && drm.iter().any(|d| d.driver == "nvidia");
    Inventory {
        nvidia: nvidia_driven && nvidia_serves_apps(&drm),
        nvidia_legacy: nvidia_driven && !nvidia_serves_apps(&drm),
        amd: amd_vulkan || drm.iter().any(|d| d.driver == "radeon" && d.render_node),
        amd_vulkan,
        amd_rocm: amd_vulkan
            && root.join("dev/kfd").exists()
            && gfx_target_versions(root)
                .is_none_or(|gfx| gfx.is_empty() || gfx.iter().any(|v| *v >= FIRST_ROCM_GFX)),
        intel: drm.iter().any(DrmDevice::is_intel_render),
        intel_compute: drm
            .iter()
            .any(|d| d.is_intel_render() && intel_computes(&d.driver, d.device_id)),
        vram_bytes: drm.iter().filter_map(|d| d.vram_bytes).max(),
        ram_bytes: std::fs::read_to_string(root.join("proc/meminfo"))
            .ok()
            .and_then(|t| mem_total_bytes(&t)),
        game_input: root.join("dev/uinput").exists(),
    }
}

struct DrmDevice {
    driver: String,
    render_node: bool,
    vram_bytes: Option<u64>,
    device_id: Option<u16>,
}

impl DrmDevice {
    fn is_intel_render(&self) -> bool {
        (self.driver == "i915" || self.driver == "xe") && self.render_node
    }
}

fn intel_computes(driver: &str, device_id: Option<u16>) -> bool {
    driver == "xe"
        || device_id.is_none_or(|id| INTEL_PRE_GEN9_DEVICE_IDS.binary_search(&id).is_err())
}

fn pci_device_id(text: &str) -> Option<u16> {
    u16::from_str_radix(text.trim().strip_prefix("0x")?, 16).ok()
}

const INTEL_PRE_GEN9_DEVICE_IDS: [u16; 140] = [
    0x0042, 0x0046, 0x0102, 0x0106, 0x010A, 0x0112, 0x0116, 0x0122, 0x0126, 0x0152, 0x0156, 0x015A,
    0x0162, 0x0166, 0x016A, 0x0402, 0x0406, 0x040A, 0x040B, 0x040E, 0x0412, 0x0416, 0x041A, 0x041B,
    0x041E, 0x0422, 0x0426, 0x042A, 0x042B, 0x042E, 0x0A02, 0x0A06, 0x0A0A, 0x0A0B, 0x0A0E, 0x0A12,
    0x0A16, 0x0A1A, 0x0A1B, 0x0A1E, 0x0A22, 0x0A26, 0x0A2A, 0x0A2B, 0x0A2E, 0x0C02, 0x0C06, 0x0C0A,
    0x0C0B, 0x0C0E, 0x0C12, 0x0C16, 0x0C1A, 0x0C1B, 0x0C1E, 0x0C22, 0x0C26, 0x0C2A, 0x0C2B, 0x0C2E,
    0x0D02, 0x0D06, 0x0D0A, 0x0D0B, 0x0D0E, 0x0D12, 0x0D16, 0x0D1A, 0x0D1B, 0x0D1E, 0x0D22, 0x0D26,
    0x0D2A, 0x0D2B, 0x0D2E, 0x0F30, 0x0F31, 0x0F32, 0x0F33, 0x1132, 0x1602, 0x1606, 0x160A, 0x160B,
    0x160D, 0x160E, 0x1612, 0x1616, 0x161A, 0x161B, 0x161D, 0x161E, 0x1622, 0x1626, 0x162A, 0x162B,
    0x162D, 0x162E, 0x1632, 0x1636, 0x163A, 0x163B, 0x163D, 0x163E, 0x22B0, 0x22B1, 0x22B2, 0x22B3,
    0x2562, 0x2572, 0x2582, 0x258A, 0x2592, 0x2772, 0x27A2, 0x27AE, 0x2972, 0x2982, 0x2992, 0x29A2,
    0x29B2, 0x29C2, 0x29D2, 0x2A02, 0x2A12, 0x2A42, 0x2E02, 0x2E12, 0x2E22, 0x2E32, 0x2E42, 0x2E92,
    0x3577, 0x3582, 0x358E, 0x7121, 0x7123, 0x7125, 0xA001, 0xA011,
];

fn drm_devices(root: &Path) -> Vec<DrmDevice> {
    let class = root.join("sys/class/drm");
    let Ok(entries) = std::fs::read_dir(&class) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_device_node(n))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let device = class.join(&name).join("device");
            let driver = driver_name(&device.join("driver"))?;
            let vram_bytes = std::fs::read_to_string(device.join("mem_info_vram_total"))
                .ok()
                .and_then(|t| t.trim().parse().ok());
            let device_id = std::fs::read_to_string(device.join("device"))
                .ok()
                .and_then(|t| pci_device_id(&t));
            Some(DrmDevice {
                driver,
                render_node: name.starts_with("renderD"),
                vram_bytes,
                device_id,
            })
        })
        .collect()
}

fn is_device_node(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix("renderD").is_some_and(digits)
        || name.strip_prefix("card").is_some_and(digits)
}

fn driver_name(link: &Path) -> Option<String> {
    let target = std::fs::read_link(link).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

pub fn mem_total_bytes(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

fn gib(bytes: u64) -> String {
    ((bytes + (1 << 29)) >> 30).to_string()
}

pub fn labels(inv: &Inventory) -> BTreeMap<&'static str, Option<String>> {
    let flag = |on: bool| on.then(|| "true".to_string());
    BTreeMap::from([
        (LABEL_NVIDIA, flag(inv.nvidia)),
        (LABEL_NVIDIA_LEGACY, flag(inv.nvidia_legacy)),
        (LABEL_AMD, flag(inv.amd)),
        (LABEL_AMD_ROCM, flag(inv.amd_rocm)),
        (LABEL_INTEL, flag(inv.intel)),
        (LABEL_INTEL_COMPUTE, flag(inv.intel_compute)),
        (LABEL_ACCELERATOR, Some(inv.accelerator().to_string())),
        (LABEL_VRAM_GIB, inv.vram_bytes.map(gib)),
        (LABEL_RAM_GIB, inv.ram_bytes.map(gib)),
        (LABEL_GAME_INPUT, flag(inv.game_input)),
    ])
}

pub fn label_patch(
    node: &str,
    current: &BTreeMap<String, String>,
    want: &BTreeMap<&'static str, Option<String>>,
) -> Option<Value> {
    let changes: serde_json::Map<String, Value> = want
        .iter()
        .filter(|(k, v)| current.get(**k) != v.as_ref())
        .map(|(k, v)| {
            (
                k.to_string(),
                v.as_ref().map_or(Value::Null, |s| Value::String(s.clone())),
            )
        })
        .collect();
    if changes.is_empty() {
        return None;
    }
    Some(json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": { "name": node, "labels": changes },
    }))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NodeHardware {
    pub arch: Option<String>,
    pub accelerator: Option<String>,
    pub video_gpu: bool,
    pub vram_gib: Option<u64>,
    pub ram_gib: Option<u64>,
    pub game_input: bool,
}

impl NodeHardware {
    pub fn from_labels(labels: &Value) -> Self {
        let get = |k: &str| labels.get(k).and_then(Value::as_str);
        let number = |k: &str| get(k).and_then(|v| v.parse().ok());
        NodeHardware {
            arch: get("kubernetes.io/arch").map(String::from),
            accelerator: get(LABEL_ACCELERATOR).map(String::from),
            video_gpu: get(LABEL_INTEL) == Some("true") || get(LABEL_AMD) == Some("true"),
            vram_gib: number(LABEL_VRAM_GIB),
            ram_gib: number(LABEL_RAM_GIB),
            game_input: get(LABEL_GAME_INPUT) == Some("true"),
        }
    }
}

pub struct HardwareLabelsController {
    pub kube: crate::k8s::Kube,
    pub root: std::path::PathBuf,
}

impl HardwareLabelsController {
    pub fn real() -> Self {
        Self {
            kube: crate::k8s::Kube::from_environment(),
            root: "/".into(),
        }
    }
}

impl crate::runtime::Controller for HardwareLabelsController {
    fn name(&self) -> &'static str {
        "hardware-labels"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        use k8s_openapi::api::core::v1::Node;
        let client = self.kube.client().await?;
        let node = kube::Api::<Node>::all(client.clone())
            .get(&ctx.node)
            .await?;
        let current = node.metadata.labels.unwrap_or_default();
        let inventory = probe(&self.root);
        if let Some(patch) = label_patch(&ctx.node, &current, &labels(&inventory)) {
            tracing::info!(
                "hardware: {} now offers {}",
                ctx.node,
                inventory.accelerator()
            );
            crate::k8s::merge_patch(&client, &patch).await?;
        }
        Ok(crate::runtime::Tick::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Machine(tempfile::TempDir);

    impl Machine {
        fn new() -> Self {
            Machine(tempfile::tempdir().unwrap())
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
        fn file(self, rel: &str, body: &str) -> Self {
            let p = self.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
            self
        }
        fn drm(self, node: &str, driver: &str) -> Self {
            let device = self.path().join("sys/class/drm").join(node).join("device");
            std::fs::create_dir_all(&device).unwrap();
            symlink(
                format!("../../../../bus/pci/drivers/{driver}"),
                device.join("driver"),
            )
            .unwrap();
            self
        }
        fn pci(self, node: &str, driver: &str, device_id: &str) -> Self {
            let rel = format!("sys/class/drm/{node}/device/device");
            self.drm(node, driver).file(&rel, &format!("{device_id}\n"))
        }
    }

    #[test]
    fn an_nvidia_gpu_counts_only_once_its_cdi_spec_exists() {
        let without_spec = Machine::new().drm("card0", "nvidia");
        assert!(!probe(without_spec.path()).nvidia);

        let with_spec = Machine::new()
            .drm("card0", "nvidia")
            .file(NVIDIA_CDI_SPEC, "{}");
        let inv = probe(with_spec.path());
        assert!(inv.nvidia);
        assert_eq!(inv.accelerator(), "nvidia");
    }

    #[test]
    fn rocm_needs_the_compute_device_but_video_and_vulkan_do_not() {
        let display_only = Machine::new().drm("renderD128", "amdgpu");
        assert!(probe(display_only.path()).amd);
        assert!(!probe(display_only.path()).amd_rocm);
        assert!(!probe(Machine::new().drm("card0", "amdgpu").path()).amd);

        let compute = Machine::new()
            .drm("renderD128", "amdgpu")
            .file("dev/kfd", "")
            .file(
                "sys/class/drm/card0/device/mem_info_vram_total",
                "17163091968\n",
            )
            .drm("card0", "amdgpu");
        let inv = probe(compute.path());
        assert!(inv.amd);
        assert_eq!(inv.vram_bytes, Some(17_163_091_968));
        assert_eq!(labels(&inv)[LABEL_VRAM_GIB].as_deref(), Some("16"));
    }

    fn kfd_gpu(machine: Machine, node: &str, gfx: u64) -> Machine {
        machine.file(
            &format!("{KFD_NODES}/{node}/properties"),
            &format!("cpu_cores_count 0\nsimd_count 64\ngfx_target_version {gfx}\n"),
        )
    }

    fn amd_machine() -> Machine {
        Machine::new()
            .drm("renderD128", "amdgpu")
            .file("dev/kfd", "")
            .file(
                &format!("{KFD_NODES}/0/properties"),
                "cpu_cores_count 8\ngfx_target_version 0\n",
            )
    }

    #[test]
    fn a_kepler_card_is_driven_but_not_offered_to_apps() {
        let gt710 = Machine::new()
            .pci("card0", "nvidia", "0x128b")
            .file(NVIDIA_CDI_SPEC, "{}");
        let inv = probe(gt710.path());
        assert!(!inv.nvidia);
        assert!(inv.nvidia_legacy);
        assert_eq!(inv.accelerator(), "nvidia-legacy");
        assert_eq!(
            labels(&inv)
                .get(LABEL_NVIDIA_LEGACY)
                .cloned()
                .flatten()
                .as_deref(),
            Some("true")
        );
        assert_eq!(labels(&inv).get(LABEL_NVIDIA).cloned().flatten(), None);

        let gtx1080 = Machine::new()
            .pci("card0", "nvidia", "0x1b80")
            .file(NVIDIA_CDI_SPEC, "{}");
        assert_eq!(probe(gtx1080.path()).accelerator(), "nvidia");
        assert!(!probe(gtx1080.path()).nvidia_legacy);
    }

    #[test]
    fn a_vega_or_newer_amd_gpu_runs_rocm() {
        let inv = probe(kfd_gpu(amd_machine(), "1", 90006).path());
        assert!(inv.amd_rocm);
        assert_eq!(inv.accelerator(), "amd");
        let rdna3 = probe(kfd_gpu(amd_machine(), "1", 110000).path());
        assert_eq!(rdna3.accelerator(), "amd");
    }

    #[test]
    fn an_amd_gpu_older_than_vega_runs_vulkan_because_rocm_skips_it() {
        let polaris = probe(kfd_gpu(amd_machine(), "1", 80003).path());
        assert!(polaris.amd);
        assert!(!polaris.amd_rocm);
        assert_eq!(polaris.accelerator(), "vulkan");
    }

    #[test]
    fn a_gcn1_card_without_a_compute_device_still_runs_vulkan() {
        let inv = probe(Machine::new().drm("renderD128", "amdgpu").path());
        assert!(inv.amd);
        assert!(!inv.amd_rocm);
        assert_eq!(inv.accelerator(), "vulkan");
        assert_eq!(labels(&inv)[LABEL_AMD].as_deref(), Some("true"));
        assert_eq!(labels(&inv)[LABEL_AMD_ROCM], None);
    }

    #[test]
    fn a_pre_gcn_radeon_decodes_video_but_has_no_vulkan_for_ai() {
        let inv = probe(Machine::new().drm("renderD128", "radeon").path());
        assert!(inv.amd);
        assert!(!inv.amd_vulkan);
        assert_eq!(inv.accelerator(), "cpu");
        assert_eq!(labels(&inv)[LABEL_AMD].as_deref(), Some("true"));
    }

    #[test]
    fn rocm_is_assumed_when_the_kernel_reports_no_gpu_architecture() {
        let inv = probe(amd_machine().path());
        assert!(inv.amd_rocm);
        assert_eq!(inv.accelerator(), "amd");
    }

    #[test]
    fn an_intel_gpu_counts_only_with_a_render_node() {
        assert!(!probe(Machine::new().drm("card0", "i915").path()).intel);
        assert!(probe(Machine::new().drm("renderD128", "i915").path()).intel);
        assert!(probe(Machine::new().drm("renderD128", "xe").path()).intel);
    }

    #[test]
    fn a_haswell_gpu_decodes_video_but_is_not_an_accelerator() {
        let inv = probe(Machine::new().pci("renderD128", "i915", "0x0416").path());
        assert!(inv.intel);
        assert!(!inv.intel_compute);
        assert_eq!(inv.accelerator(), "cpu");
        let labels = labels(&inv);
        assert_eq!(labels[LABEL_INTEL].as_deref(), Some("true"));
        assert_eq!(labels[LABEL_INTEL_COMPUTE], None);
        assert_eq!(labels[LABEL_ACCELERATOR].as_deref(), Some("cpu"));
    }

    #[test]
    fn a_gen9_or_newer_intel_gpu_is_an_accelerator() {
        for (driver, id) in [("i915", "0x1916"), ("i915", "0x5917"), ("i915", "0x9a49")] {
            let inv = probe(Machine::new().pci("renderD128", driver, id).path());
            assert!(inv.intel_compute, "{driver} {id}");
            assert_eq!(inv.accelerator(), "intel", "{driver} {id}");
        }
    }

    #[test]
    fn the_xe_driver_only_drives_gpus_that_compute() {
        let inv = probe(Machine::new().pci("renderD128", "xe", "0x0416").path());
        assert!(inv.intel_compute);
    }

    #[test]
    fn an_intel_gpu_whose_pci_id_cannot_be_read_keeps_counting_as_an_accelerator() {
        let inv = probe(Machine::new().drm("renderD128", "i915").path());
        assert!(inv.intel_compute);
        let garbled = Machine::new().pci("renderD128", "i915", "not-hex");
        assert!(probe(garbled.path()).intel_compute);
    }

    #[test]
    fn the_pre_gen9_table_is_sorted_so_binary_search_finds_every_entry() {
        assert!(INTEL_PRE_GEN9_DEVICE_IDS.windows(2).all(|w| w[0] < w[1]));
        for id in INTEL_PRE_GEN9_DEVICE_IDS {
            assert!(!intel_computes("i915", Some(id)), "{id:#06x}");
        }
    }

    #[test]
    fn the_pre_gen9_table_ends_where_skylake_begins() {
        for skylake in [0x1902, 0x1906, 0x1912, 0x1916, 0x191B, 0x1926, 0x193B] {
            assert!(intel_computes("i915", Some(skylake)), "{skylake:#06x}");
        }
        for broadwell in [0x1602, 0x1616, 0x162B, 0x163E] {
            assert!(!intel_computes("i915", Some(broadwell)), "{broadwell:#06x}");
        }
    }

    #[test]
    fn pci_ids_are_read_as_sysfs_writes_them() {
        assert_eq!(pci_device_id("0x0416\n"), Some(0x0416));
        assert_eq!(pci_device_id("0x9a49"), Some(0x9A49));
        assert_eq!(pci_device_id("0416"), None);
        assert_eq!(pci_device_id(""), None);
    }

    #[test]
    fn the_compute_label_is_the_one_the_immich_chart_reads() {
        assert_eq!(LABEL_INTEL_COMPUTE, "yolab.io/gpu-intel-compute");
    }

    #[test]
    fn connectors_are_not_mistaken_for_gpus() {
        let m = Machine::new().drm("card0-HDMI-A-1", "i915");
        assert_eq!(probe(m.path()), Inventory::default());
    }

    #[test]
    fn the_strongest_accelerator_names_the_machine() {
        let both = Inventory {
            nvidia: true,
            intel: true,
            ..Default::default()
        };
        assert_eq!(both.accelerator(), "nvidia");
        let amd_and_intel = Inventory {
            amd: true,
            amd_rocm: true,
            intel: true,
            ..Default::default()
        };
        assert_eq!(amd_and_intel.accelerator(), "amd");
        let legacy_and_intel = Inventory {
            nvidia_legacy: true,
            intel: true,
            intel_compute: true,
            ..Default::default()
        };
        assert_eq!(legacy_and_intel.accelerator(), "intel");
        let legacy = Inventory {
            nvidia_legacy: true,
            ..Default::default()
        };
        assert_eq!(legacy.accelerator(), "nvidia-legacy");
        assert_eq!(Inventory::default().accelerator(), "cpu");
    }

    #[test]
    fn ram_is_read_from_meminfo_and_rounded_to_whole_gib() {
        let m = Machine::new().file(
            "proc/meminfo",
            "MemTotal:       32765432 kB\nMemFree: 1 kB\n",
        );
        let inv = probe(m.path());
        assert_eq!(inv.ram_bytes, Some(32_765_432 * 1024));
        assert_eq!(labels(&inv)[LABEL_RAM_GIB].as_deref(), Some("31"));
    }

    #[test]
    fn game_input_follows_dev_uinput() {
        assert!(!probe(Machine::new().path()).game_input);
        assert!(probe(Machine::new().file("dev/uinput", "").path()).game_input);
    }

    #[test]
    fn a_gpu_that_went_away_has_its_label_removed() {
        let current = BTreeMap::from([
            (LABEL_NVIDIA.to_string(), "true".to_string()),
            (LABEL_ACCELERATOR.to_string(), "nvidia".to_string()),
        ]);
        let patch = label_patch("node1", &current, &labels(&Inventory::default())).unwrap();
        assert_eq!(patch["metadata"]["name"], "node1");
        assert_eq!(patch["metadata"]["labels"][LABEL_NVIDIA], Value::Null);
        assert_eq!(patch["metadata"]["labels"][LABEL_ACCELERATOR], "cpu");
    }

    #[test]
    fn labels_that_already_match_send_no_patch() {
        let inv = Inventory {
            intel: true,
            game_input: true,
            ..Default::default()
        };
        let current: BTreeMap<String, String> = labels(&inv)
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), v?)))
            .collect();
        assert_eq!(label_patch("node1", &current, &labels(&inv)), None);
    }

    #[test]
    fn a_haswell_machine_reads_back_as_a_video_only_gpu() {
        let inv = Inventory {
            intel: true,
            ..Default::default()
        };
        let read = |inv: &Inventory| {
            let labels: serde_json::Map<String, Value> = labels(inv)
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_string(), Value::String(v?))))
                .collect();
            NodeHardware::from_labels(&Value::Object(labels))
        };
        let haswell = read(&inv);
        assert_eq!(haswell.accelerator.as_deref(), Some("cpu"));
        assert!(haswell.video_gpu);
        assert!(!read(&Inventory::default()).video_gpu);
    }

    #[test]
    fn a_node_reads_back_the_hardware_its_labels_describe() {
        let inv = Inventory {
            amd: true,
            amd_rocm: true,
            vram_bytes: Some(24 << 30),
            ram_bytes: Some(64 << 30),
            game_input: true,
            ..Default::default()
        };
        let mut labels: serde_json::Map<String, Value> = labels(&inv)
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), Value::String(v?))))
            .collect();
        labels.insert("kubernetes.io/arch".into(), "arm64".into());
        assert_eq!(
            NodeHardware::from_labels(&Value::Object(labels)),
            NodeHardware {
                arch: Some("arm64".into()),
                accelerator: Some("amd".into()),
                video_gpu: true,
                vram_gib: Some(24),
                ram_gib: Some(64),
                game_input: true,
            }
        );
    }

    #[test]
    fn a_node_that_was_never_labelled_reports_nothing_rather_than_cpu() {
        assert_eq!(
            NodeHardware::from_labels(&json!({})),
            NodeHardware::default()
        );
        assert_eq!(
            NodeHardware::from_labels(&Value::Null),
            NodeHardware::default()
        );
    }

    #[tokio::test]
    async fn a_tick_labels_the_node_with_what_this_machine_offers() {
        use crate::runtime::Controller as _;
        let (server, client) = crate::k8s::testing::api_server().await;
        crate::k8s::testing::serve(
            &server,
            "/api/v1/nodes/node1",
            200,
            json!({ "apiVersion": "v1", "kind": "Node", "metadata": { "name": "node1", "labels": {} } }),
        )
        .await;
        crate::k8s::testing::accept_patches(&server).await;
        let machine = Machine::new()
            .drm("renderD128", "i915")
            .file("dev/uinput", "");
        let controller = HardwareLabelsController {
            kube: crate::k8s::Kube::with(client),
            root: machine.path().to_path_buf(),
        };

        controller
            .reconcile(&crate::runtime::Ctx {
                node: "node1".into(),
            })
            .await
            .unwrap();

        let patches = crate::k8s::testing::patched(&server).await;
        assert_eq!(patches.len(), 1);
        let labels = &patches[0]["metadata"]["labels"];
        assert_eq!(labels[LABEL_INTEL], "true");
        assert_eq!(labels[LABEL_ACCELERATOR], "intel");
        assert_eq!(labels[LABEL_GAME_INPUT], "true");
    }

    #[test]
    fn labels_this_node_does_not_own_are_left_alone() {
        let current = BTreeMap::from([("kubernetes.io/hostname".to_string(), "node1".to_string())]);
        let patch = label_patch("node1", &current, &labels(&Inventory::default())).unwrap();
        assert!(patch["metadata"]["labels"]
            .get("kubernetes.io/hostname")
            .is_none());
    }
}
