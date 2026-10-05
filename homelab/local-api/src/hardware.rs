use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

pub const LABEL_NVIDIA: &str = "yolab.io/gpu-nvidia";
pub const LABEL_AMD: &str = "yolab.io/gpu-amd";
pub const LABEL_INTEL: &str = "yolab.io/gpu-intel";
pub const LABEL_ACCELERATOR: &str = "yolab.io/accelerator";
pub const LABEL_VRAM_GIB: &str = "yolab.io/vram-gib";
pub const LABEL_RAM_GIB: &str = "yolab.io/ram-gib";
pub const LABEL_GAME_INPUT: &str = "yolab.io/game-input";

const NVIDIA_CDI_SPEC: &str = "var/run/cdi/nvidia-container-toolkit.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub nvidia: bool,
    pub amd: bool,
    pub intel: bool,
    pub vram_bytes: Option<u64>,
    pub ram_bytes: Option<u64>,
    pub game_input: bool,
}

impl Inventory {
    pub fn accelerator(&self) -> &'static str {
        if self.nvidia {
            "nvidia"
        } else if self.amd {
            "amd"
        } else if self.intel {
            "intel"
        } else {
            "cpu"
        }
    }
}

pub fn probe(root: &Path) -> Inventory {
    let drm = drm_devices(root);
    Inventory {
        nvidia: root.join(NVIDIA_CDI_SPEC).is_file(),
        amd: root.join("dev/kfd").exists() && drm.iter().any(|d| d.driver == "amdgpu"),
        intel: drm
            .iter()
            .any(|d| (d.driver == "i915" || d.driver == "xe") && d.render_node),
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
}

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
            Some(DrmDevice {
                driver,
                render_node: name.starts_with("renderD"),
                vram_bytes,
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
        (LABEL_AMD, flag(inv.amd)),
        (LABEL_INTEL, flag(inv.intel)),
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
    pub accelerator: Option<String>,
    pub vram_gib: Option<u64>,
    pub ram_gib: Option<u64>,
    pub game_input: bool,
}

impl NodeHardware {
    pub fn from_labels(labels: &Value) -> Self {
        let get = |k: &str| labels.get(k).and_then(Value::as_str);
        let number = |k: &str| get(k).and_then(|v| v.parse().ok());
        NodeHardware {
            accelerator: get(LABEL_ACCELERATOR).map(String::from),
            vram_gib: number(LABEL_VRAM_GIB),
            ram_gib: number(LABEL_RAM_GIB),
            game_input: get(LABEL_GAME_INPUT) == Some("true"),
        }
    }
}

pub struct HardwareLabelsController;

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
        let client = crate::k8s::client().await?;
        let node = kube::Api::<Node>::all(client.clone())
            .get(&ctx.node)
            .await?;
        let current = node.metadata.labels.unwrap_or_default();
        let inventory = probe(Path::new("/"));
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
    fn an_amd_gpu_counts_only_with_the_compute_device_rocm_needs() {
        let display_only = Machine::new().drm("renderD128", "amdgpu");
        assert!(!probe(display_only.path()).amd);

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

    #[test]
    fn an_intel_gpu_counts_only_with_a_render_node() {
        assert!(!probe(Machine::new().drm("card0", "i915").path()).intel);
        assert!(probe(Machine::new().drm("renderD128", "i915").path()).intel);
        assert!(probe(Machine::new().drm("renderD128", "xe").path()).intel);
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
            intel: true,
            ..Default::default()
        };
        assert_eq!(amd_and_intel.accelerator(), "amd");
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
    fn a_node_reads_back_the_hardware_its_labels_describe() {
        let inv = Inventory {
            amd: true,
            vram_bytes: Some(24 << 30),
            ram_bytes: Some(64 << 30),
            game_input: true,
            ..Default::default()
        };
        let labels: serde_json::Map<String, Value> = labels(&inv)
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), Value::String(v?))))
            .collect();
        assert_eq!(
            NodeHardware::from_labels(&Value::Object(labels)),
            NodeHardware {
                accelerator: Some("amd".into()),
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

    #[test]
    fn labels_this_node_does_not_own_are_left_alone() {
        let current = BTreeMap::from([("kubernetes.io/hostname".to_string(), "node1".to_string())]);
        let patch = label_patch("node1", &current, &labels(&Inventory::default())).unwrap();
        assert!(patch["metadata"]["labels"]
            .get("kubernetes.io/hostname")
            .is_none());
    }
}
