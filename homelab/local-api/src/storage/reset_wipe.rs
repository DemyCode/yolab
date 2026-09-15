//! Boot step: the part of a FORCE HEAL that destroys this machine's cluster state.
//!
//! A heal rebuilds the cluster from the machines that still answer as a fresh
//! installation (see `heal`). Every one of them first rebuilds its boot entry
//! from its rewritten config.toml, then sets `[node] wipe_condition = true` in
//! it, and restarts. This runs early in every boot — before the Ceph bootstrap, any
//! Ceph daemon, the image store or k3s — and turns the machine back into one that
//! has never been part of a cluster:
//!
//!   - every OSD on its disks is erased,
//!   - the Ceph state (mon store, keyrings, daemon directories) is removed,
//!   - the k3s state (etcd, agent certificates, kubelet) is removed.
//!
//! The boot then takes the ordinary install path: create the cluster, or join
//! the machine the heal chose.
//!
//! THE SYSTEM VOLUME'S VOLUME GROUP IS NEVER DESTROYED. ceph-volume reports an
//! OSD made on an existing logical volume with the physical volume under it as
//! its device — on node1 `/dev/sda2`, the partition holding the operating system.
//! Erasing "every OSD device" would therefore erase the OS. So volumes are erased
//! by their own path: those in a volume group ceph-volume made itself
//! (`ceph-…`) together with that group, any other in place.
//!
//! THE FLAG IS READ AT BOOT, never at build time: nothing in the NixOS
//! configuration depends on it, so setting it needs no rebuild, and clearing it
//! here takes effect for the very next boot.
//!
//! Idempotent: the flag is cleared only once everything is gone, so an
//! interrupted wipe simply runs again at the next boot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::ceph::destructive::{self, ZapWarrant};
use crate::host::Host;

/// The config.toml key, in `[node]`, that asks for the wipe.
pub const FLAG: &str = "wipe_condition";

/// Directories whose CONTENTS are removed, except the named entry. The
/// directories themselves stay, with the owners and modes tmpfiles gave them.
///
/// k3s keeps two things tmpfiles puts in place before this step runs: the
/// manifests it applies (`server/manifests`) and the kubelet drop-in
/// configuration (`agent/etc`).
const EMPTIED: &[(&str, Option<&str>)] = &[
    ("var/lib/ceph/mon", None),
    ("var/lib/ceph/mgr", None),
    ("var/lib/ceph/mds", None),
    ("var/lib/ceph/osd", None),
    ("var/lib/ceph/bootstrap-osd", None),
    ("var/lib/ceph/crash", None),
    ("var/lib/rancher/k3s/server", Some("manifests")),
    ("var/lib/rancher/k3s/agent", Some("etc")),
    ("var/lib/kubelet", None),
    ("var/lib/cni", None),
];

const REMOVED_FILES: &[&str] = &[
    "etc/ceph/ceph.client.admin.keyring",
    "var/lib/ceph/.yolab-set-noout",
    "etc/rancher/k3s/k3s.yaml",
    "etc/rancher/node/password",
    "var/lib/yolab/mesh-peers.json",
    // The heal's copies of what was before it: that cluster is gone.
    "var/lib/yolab/reset/config.toml.before",
    "var/lib/yolab/reset/system.before",
];

/// Whether `config` asks for the wipe. A missing key is no.
pub fn wipe_condition(config: &str) -> Result<bool> {
    let table: toml::Table = toml::from_str(config).context("config.toml is not TOML")?;
    match table.get("node").and_then(|n| n.get(FLAG)) {
        None => Ok(false),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("[node] {FLAG} is not true or false")),
    }
}

/// `config` with the flag set to `value`, everything else as it was.
pub fn with_wipe_condition(config: &str, value: bool) -> Result<String> {
    let mut table: toml::Table = toml::from_str(config).context("config.toml is not TOML")?;
    table
        .get_mut("node")
        .and_then(|n| n.as_table_mut())
        .context("config.toml has no [node]")?
        .insert(FLAG.into(), value.into());
    toml::to_string(&table).context("write config.toml")
}

/// `config_path`: this machine's config.toml.
pub async fn run<H: Host>(host: &H, root: &Path, config_path: &Path) -> Result<()> {
    let config = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    if !wipe_condition(&config)? {
        tracing::info!("reset-wipe: no heal asked for a wipe");
        return Ok(());
    }
    tracing::warn!("reset-wipe: wiping this machine's Ceph and k3s state for a FORCE HEAL");

    let raw = host
        .ceph_volume(&["lvm", "list", "--format", "json"])
        .await
        .context("list this machine's OSDs")?;
    for path in volumes_to_erase(&raw)? {
        tracing::warn!("reset-wipe: erasing {path}");
        destructive::zap(host, &path, ZapWarrant::MachineReset).await?;
    }

    for (dir, kept) in EMPTIED {
        empty_dir(&root.join(dir), *kept)?;
    }
    for file in REMOVED_FILES {
        remove_file(&root.join(file))?;
    }

    // Read again: only the flag changes, whatever else happened to the file.
    let config = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    crate::config::write_private_file(config_path, with_wipe_condition(&config, false)?.as_bytes())?;
    tracing::warn!("reset-wipe: done — this machine now boots as a fresh one");
    Ok(())
}

/// The paths to erase for every OSD in a `ceph-volume lvm list --format json`.
///
/// A volume in a group ceph-volume created (`ceph-…`) is erased by its logical
/// volume path, which `zap` destroys together with the group. Any other volume —
/// the system volume carved out by the installer — is erased by its
/// device-mapper name, which `zap` never destroys.
fn volumes_to_erase(raw: &str) -> Result<BTreeSet<String>> {
    let start = raw
        .find('{')
        .context("ceph-volume lvm list printed no JSON")?;
    let listing: Value =
        serde_json::from_str(&raw[start..]).context("ceph-volume lvm list is not JSON")?;
    let osds = listing
        .as_object()
        .context("ceph-volume lvm list is not an object")?;
    let mut out = BTreeSet::new();
    for volumes in osds.values() {
        for volume in volumes.as_array().into_iter().flatten() {
            let lv_path = volume["lv_path"]
                .as_str()
                .context("an OSD volume without an lv_path")?;
            let (vg, lv) = vg_and_lv(volume, lv_path)
                .with_context(|| format!("cannot tell the volume group of {lv_path}"))?;
            if vg.starts_with("ceph-") {
                out.insert(lv_path.to_string());
            } else {
                out.insert(format!(
                    "/dev/mapper/{}-{}",
                    vg.replace('-', "--"),
                    lv.replace('-', "--")
                ));
            }
        }
    }
    Ok(out)
}

fn vg_and_lv(volume: &Value, lv_path: &str) -> Option<(String, String)> {
    if let (Some(vg), Some(lv)) = (volume["vg_name"].as_str(), volume["lv_name"].as_str()) {
        return Some((vg.to_string(), lv.to_string()));
    }
    let mut parts = lv_path.strip_prefix("/dev/")?.split('/');
    let (vg, lv) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !vg.is_empty() && !lv.is_empty())
        .then(|| (vg.to_string(), lv.to_string()))
}

fn empty_dir(dir: &Path, keep: Option<&str>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        if keep.is_some_and(|k| entry.file_name() == k) {
            continue;
        }
        let path: PathBuf = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() && !kind.is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use serde_json::json;

    /// node1's real listing: the system OSD on the installer's `pool/ceph`
    /// volume, whose `devices` is the OS partition.
    fn listing() -> String {
        json!({
            "0": [{
                "devices": ["/dev/sda2"],
                "lv_name": "ceph",
                "lv_path": "/dev/pool/ceph",
                "vg_name": "pool",
                "type": "block",
            }],
            "1": [{
                "devices": ["/dev/sdb"],
                "lv_name": "osd-block-0d0a35b2",
                "lv_path": "/dev/ceph-da2f6e97/osd-block-0d0a35b2",
                "type": "block",
            }],
        })
        .to_string()
    }

    #[test]
    fn the_system_volume_is_erased_by_name_and_never_through_its_device() {
        let paths = volumes_to_erase(&listing()).unwrap();
        assert_eq!(
            paths,
            BTreeSet::from([
                "/dev/ceph-da2f6e97/osd-block-0d0a35b2".to_string(),
                "/dev/mapper/pool-ceph".to_string(),
            ])
        );
        assert!(!paths.iter().any(|p| p.contains("sda")));
    }

    #[test]
    fn dashes_in_a_device_mapper_name_are_doubled() {
        let raw = json!({"0": [{"lv_path": "/dev/my-vg/my-lv"}]}).to_string();
        assert_eq!(
            volumes_to_erase(&raw).unwrap(),
            BTreeSet::from(["/dev/mapper/my--vg-my--lv".to_string()])
        );
    }

    #[test]
    fn a_listing_it_cannot_read_is_an_error_not_nothing_to_erase() {
        assert!(volumes_to_erase("no json here").is_err());
        assert!(volumes_to_erase(r#"{"0": [{"devices": ["/dev/sdb"]}]}"#).is_err());
        assert!(volumes_to_erase(r#"--> noise{}"#).unwrap().is_empty());
    }

    const CONFIG: &str = "var/lib/yolab/machine/config.toml";

    /// A config.toml with the flag set to `wipe`.
    fn config(root: &Path, wipe: bool) -> PathBuf {
        let path = root.join(CONFIG);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("[node]\nsub_ipv6_private = \"fd00::1\"\n{FLAG} = {wipe}\n[node.k3s]\ntoken = \"t\"\n"),
        )
        .unwrap();
        path
    }

    fn flag(path: &Path) -> bool {
        wipe_condition(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_flag_is_read_and_written_without_touching_the_rest() {
        assert!(!wipe_condition("[node]\n").unwrap(), "missing is no");
        assert!(wipe_condition("[node]\nwipe_condition = true\n").unwrap());
        assert!(wipe_condition("[node]\nwipe_condition = \"yes\"\n").is_err());
        assert!(wipe_condition("not toml {{").is_err());
        let set = with_wipe_condition("[node]\nnode_id = \"n1\"\n[node.k3s]\ntoken = \"t\"\n", true).unwrap();
        let t: toml::Table = toml::from_str(&set).unwrap();
        assert_eq!(t["node"]["wipe_condition"].as_bool(), Some(true));
        assert_eq!(t["node"]["node_id"].as_str(), Some("n1"));
        assert_eq!(t["node"]["k3s"]["token"].as_str(), Some("t"));
    }

    #[tokio::test]
    async fn without_the_flag_nothing_is_touched() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("var/lib/ceph/mon/ceph-node1")).unwrap();
        let cfg = config(root.path(), false);
        let host = FakeHost::new();
        run(&host, root.path(), &cfg).await.unwrap();
        assert!(host.calls().is_empty());
        assert!(root.path().join("var/lib/ceph/mon/ceph-node1").exists());
    }

    #[tokio::test]
    async fn the_wipe_erases_every_osd_and_the_state_then_clears_its_flag() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for dir in [
            "var/lib/ceph/mon/ceph-node1",
            "var/lib/ceph/mgr/ceph-node1",
            "var/lib/rancher/k3s/server/db",
            "var/lib/rancher/k3s/server/manifests",
            "var/lib/rancher/k3s/agent/etc/kubelet.conf.d",
            "var/lib/rancher/k3s/agent/client-ca",
            "var/lib/yolab/machine",
            "etc/ceph",
        ] {
            std::fs::create_dir_all(r.join(dir)).unwrap();
        }
        std::fs::write(r.join("var/lib/ceph/mon/ceph-node1/keyring"), "k").unwrap();
        std::fs::write(r.join("etc/ceph/ceph.client.admin.keyring"), "k").unwrap();
        std::fs::write(r.join("etc/ceph/ceph.conf"), "conf").unwrap();
        std::fs::write(r.join("var/lib/yolab/heal.json"), "{}").unwrap();
        let cfg = config(r, true);
        let host = FakeHost::new()
            .ok("ceph-volume lvm list", &listing())
            .ok("ceph-volume lvm zap", "");

        run(&host, r, &cfg).await.unwrap();

        assert!(host.ran("ceph-volume lvm zap --destroy /dev/ceph-da2f6e97/osd-block-0d0a35b2"));
        assert!(host.ran("ceph-volume lvm zap /dev/mapper/pool-ceph"));
        assert!(!host.ran("zap --destroy /dev/mapper") && !host.ran("sda"));
        assert!(r.join("var/lib/ceph/mon").exists(), "the directory stays");
        assert!(!r.join("var/lib/ceph/mon/ceph-node1").exists());
        assert!(!r.join("var/lib/ceph/mgr/ceph-node1").exists());
        assert!(!r.join("var/lib/rancher/k3s/server/db").exists());
        assert!(r.join("var/lib/rancher/k3s/server/manifests").exists());
        assert!(!r.join("var/lib/rancher/k3s/agent/client-ca").exists());
        assert!(r.join("var/lib/rancher/k3s/agent/etc/kubelet.conf.d").exists());
        assert!(!r.join("etc/ceph/ceph.client.admin.keyring").exists());
        assert!(r.join("etc/ceph/ceph.conf").exists());
        assert!(!flag(&cfg), "the next boot wipes nothing");
        assert!(
            std::fs::read_to_string(&cfg).unwrap().contains("fd00::1"),
            "the rest of config.toml stays"
        );
        assert!(r.join("var/lib/yolab/heal.json").exists(), "the heal's record stays");
    }

    #[tokio::test]
    async fn a_failed_erase_keeps_the_flag_for_the_next_boot() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path(), true);
        let host = FakeHost::new()
            .ok("ceph-volume lvm list", &listing())
            .fail("ceph-volume lvm zap", "device busy");
        assert!(run(&host, root.path(), &cfg).await.is_err());
        assert!(flag(&cfg), "the next boot tries again");
    }
}
