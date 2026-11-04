pub mod platform;
pub mod record;
pub mod stack;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::error::Outcome;
use crate::host::Host;
use crate::notify::Tunnel;
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};
use record::VdiskRecord;

pub const NAME: &str = "vdisks";

pub(crate) fn vdisk_of_block_target(target: &str) -> Option<i64> {
    if let Some(id) = record::id_of_osd_dev(target) {
        return Some(id);
    }
    let rest = target.strip_prefix("/dev/yolab_vdisk_")?;
    let (id, lv) = rest.split_once('/')?;
    (lv == "osd").then(|| id.parse().ok()).flatten()
}

fn block_target(root: &Path, osd_id: i64) -> Option<String> {
    std::fs::read_link(
        root.join("var/lib/ceph/osd")
            .join(format!("ceph-{osd_id}"))
            .join("block"),
    )
    .ok()
    .map(|p| p.to_string_lossy().to_string())
}

pub(crate) fn vdisk_of_osd(root: &Path, osd_id: i64) -> Option<i64> {
    block_target(root, osd_id).and_then(|t| vdisk_of_block_target(&t))
}

pub(crate) fn osd_on(root: &Path, vdisk: i64) -> Option<i64> {
    let dir = root.join("var/lib/ceph/osd");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("ceph-")?
                .parse::<i64>()
                .ok()
        })
        .find(|osd| vdisk_of_osd(root, *osd) == Some(vdisk))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlacementStep {
    CrushLocation,
    PrimaryAffinity,
    NoOut,
}

pub(crate) fn placement_steps(
    crush_location: &str,
    entry: &crate::ceph::model::OsdDumpEntry,
    want_location: &str,
) -> Vec<PlacementStep> {
    let mut steps = Vec::new();
    if crush_location.trim() != want_location {
        steps.push(PlacementStep::CrushLocation);
    }
    if entry.primary_affinity != Some(0.0) {
        steps.push(PlacementStep::PrimaryAffinity);
    }
    if !entry.has_own_noout() {
        steps.push(PlacementStep::NoOut);
    }
    steps
}

pub(crate) async fn ensure_placement<H: Host>(host: &H, osd_id: i64, vdisk: i64) -> Result<()> {
    let osd = format!("osd.{osd_id}");
    let want = record::crush_location(vdisk);
    let have = match host.ceph(&["config", "get", &osd, "crush_location"]).await {
        Ok(v) => v,
        Err(e) if e.is_not_found() => String::new(),
        Err(e) => return Err(e).context("read the OSD's CRUSH location"),
    };
    let dump = host.osd_dump().await.context("read the OSD map")?;
    let entry = dump
        .osds
        .iter()
        .find(|e| e.osd == osd_id)
        .with_context(|| format!("{osd} is not in the OSD map"))?;
    for step in placement_steps(&have, entry, &want) {
        match step {
            PlacementStep::CrushLocation => {
                host.ceph(&["config", "set", &osd, "crush_location", &want])
                    .await
                    .context("place the OSD in its own CRUSH host")?;
            }
            PlacementStep::PrimaryAffinity => {
                host.ceph(&["osd", "primary-affinity", &osd, "0"])
                    .await
                    .context("keep reads off the virtual disk")?;
            }
            PlacementStep::NoOut => {
                host.ceph(&["osd", "add-noout", &osd])
                    .await
                    .context("stop an internet outage from moving data off the virtual disk")?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn placement_allows_start<H: Host>(host: &H, root: &Path, osd_id: i64) -> bool {
    let Some(vdisk) = vdisk_of_osd(root, osd_id) else {
        return true;
    };
    match ensure_placement(host, osd_id, vdisk).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                "osd.{osd_id} (virtual disk {vdisk}): not starting it until its placement is set: {e:#}"
            );
            false
        }
    }
}

pub async fn detach_all<H: Host>(host: &H, root: &Path) -> Result<()> {
    let mut failed = Vec::new();
    for id in stack::attached_here(root)? {
        if let Err(e) = stack::detach(host, root, id).await {
            tracing::error!("virtual disk {id}: could not detach: {e:#}");
            failed.push(id);
        }
    }
    if !failed.is_empty() {
        anyhow::bail!("virtual disks {failed:?} are still attached");
    }
    Ok(())
}

pub(crate) const OFFLINE: &str =
    "The Storage Box is not answering. This virtual disk is offline until it does; your data \
     stays on your other disks.";
pub(crate) const PREPARING: &str = "Hetzner is preparing the Storage Box…";
pub(crate) const DRAINING: &str = "Moving your data off this virtual disk before deleting it…";
pub(crate) const CONNECTED: &str = "Connected";

pub struct VdisksController<H> {
    pub config: crate::config::Config,
    pub host: H,
    pub http: crate::http::Client,
}

impl<H: Host + 'static> Controller for VdisksController<H> {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph]
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let host = &self.host;
        let root = Path::new("/");
        let records = record::list(host)
            .await
            .context("read the virtual disk records")?;
        let mine: Vec<VdiskRecord> = records.into_iter().filter(|r| r.node == ctx.node).collect();

        for id in stack::attached_here(root)? {
            if !mine.iter().any(|r| r.id == id) {
                tracing::warn!(
                    "virtual disk {id}: attached here but no longer recorded — detaching it"
                );
                stack::detach(host, root, id)
                    .await
                    .warn_on_err(format!("detach unrecorded virtual disk {id}"));
            }
        }
        if mine.is_empty() {
            return Ok(Tick::Idle("no virtual disks on this machine".into()));
        }

        let tunnel = Tunnel::read(&self.config.config_path)?;
        let client = &self.http;
        let published = record::statuses(host)
            .await
            .ok_or_warn("read the virtual disk statuses")
            .unwrap_or_default();
        for rec in mine {
            let id = rec.id;
            let status = match step(host, root, client, &tunnel, rec).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("virtual disk {id}: {e:#}");
                    format!("Could not connect yet, retrying: {e:#}")
                }
            };
            if status.is_empty() {
                continue;
            }
            if published.get(&id.to_string()) != Some(&status) {
                record::set_status(host, id, &status)
                    .await
                    .warn_on_err(format!("publish the status of virtual disk {id}"));
            }
        }
        Ok(Tick::Done)
    }
}

async fn step<H: Host>(
    host: &H,
    root: &Path,
    client: &crate::http::Client,
    tunnel: &Tunnel,
    mut rec: VdiskRecord,
) -> Result<String> {
    if !rec.is_provisioned() {
        if rec.removing {
            finish_removal(host, root, client, tunnel, &rec).await?;
            return Ok(String::new());
        }
        let found = platform::list(client, tunnel)
            .await?
            .into_iter()
            .find(|v| v.id == rec.id);
        match found {
            Some(v) if v.is_ready() => {
                rec.host = v.host;
                rec.username = v.username;
                rec.password = v.password;
                record::save(host, &rec).await?;
            }
            Some(_) => return Ok(PREPARING.into()),
            None => anyhow::bail!("the platform has no Storage Box for this virtual disk"),
        }
    }

    if rec.removing {
        let known = host.osd_ids().await.context("list the cluster's OSDs")?;
        let tree = host
            .ceph_json(&["osd", "tree"])
            .await
            .context("read the CRUSH tree")?;
        let mut holding = crush_children(&tree, &record::crush_bucket(rec.id));
        holding.extend(osd_on(root, rec.id));
        holding.extend(rec.osd_id);
        if !removal_must_wait(&holding, &known) {
            finish_removal(host, root, client, tunnel, &rec).await?;
            return Ok(String::new());
        }
    }

    let mnt_up = stack::is_mounted(host, &stack::run_dir(root, rec.id).join("mnt")).await?;
    if mnt_up && !stack::reachable(host, root, rec.id).await {
        tracing::warn!(
            "virtual disk {}: the Storage Box stopped answering — detaching",
            rec.id
        );
        stack::detach(host, root, rec.id).await?;
        return Ok(OFFLINE.into());
    }
    if !stack::is_attached(root, rec.id) {
        stack::attach(host, root, &rec).await?;
        crate::runtime::wake("disks");
    }

    if let Some(osd) = osd_on(root, rec.id) {
        if rec.osd_id != Some(osd) {
            rec.osd_id = Some(osd);
            record::save(host, &rec).await?;
        }
    }

    if rec.removing {
        return Ok(DRAINING.into());
    }
    Ok(CONNECTED.into())
}

pub(crate) fn crush_children(tree: &serde_json::Value, bucket: &str) -> Vec<i64> {
    tree["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["name"].as_str() == Some(bucket))
        .and_then(|n| n["children"].as_array())
        .map(|c| {
            c.iter()
                .filter_map(|x| x.as_i64())
                .filter(|id| *id >= 0)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn removal_must_wait(holding: &[i64], known: &[i64]) -> bool {
    holding.iter().any(|osd| known.contains(osd))
}

async fn finish_removal<H: Host>(
    host: &H,
    root: &Path,
    client: &crate::http::Client,
    tunnel: &Tunnel,
    rec: &VdiskRecord,
) -> Result<()> {
    stack::detach(host, root, rec.id).await?;
    platform::delete(client, tunnel, rec.id).await?;
    let bucket = record::crush_bucket(rec.id);
    match host.ceph(&["osd", "crush", "rm", &bucket]).await {
        Ok(_) => {}
        Err(e) if e.is_not_found() => {}
        Err(e) => tracing::warn!(
            "virtual disk {}: its empty CRUSH host {bucket} stays: {e}",
            rec.id
        ),
    }
    crate::routers::disks::forget_disk(host, &rec.node, &rec.disk_id())
        .await
        .map_err(anyhow::Error::msg)?;
    record::forget(host, rec.id).await?;
    std::fs::remove_dir_all(stack::run_dir(root, rec.id))
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
        .context("remove the virtual disk's run directory")?;
    tracing::info!("virtual disk {}: deleted", rec.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ceph::model::OsdDumpEntry;

    fn entry(json: &str) -> OsdDumpEntry {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_virtual_disk_osd_is_recognised_from_either_lvm_path() {
        assert_eq!(vdisk_of_block_target("/dev/yolab_vdisk_4/osd"), Some(4));
        assert_eq!(
            vdisk_of_block_target("/dev/mapper/yolab_vdisk_4-osd"),
            Some(4)
        );
        assert_eq!(vdisk_of_block_target("/dev/pool/ceph"), None);
        assert_eq!(vdisk_of_block_target("/dev/yolab_vdisk_4/other"), None);
    }

    #[test]
    fn a_fresh_virtual_disk_osd_needs_every_placement_step() {
        let e = entry(r#"{"osd":5,"up":0,"in":1,"primary_affinity":1,"state":["exists","new"]}"#);
        assert_eq!(
            placement_steps("", &e, "root=default host=vdisk-4"),
            vec![
                PlacementStep::CrushLocation,
                PlacementStep::PrimaryAffinity,
                PlacementStep::NoOut
            ]
        );
    }

    #[test]
    fn a_placed_virtual_disk_osd_needs_nothing() {
        let e = entry(
            r#"{"osd":5,"up":1,"in":1,"primary_affinity":0,"state":["exists","up","noout"]}"#,
        );
        assert!(placement_steps(
            "root=default host=vdisk-4\n",
            &e,
            "root=default host=vdisk-4"
        )
        .is_empty());
    }

    #[test]
    fn the_box_is_not_deleted_while_the_cluster_still_has_its_osd() {
        assert!(removal_must_wait(&[5], &[1, 5]));
        assert!(!removal_must_wait(&[5], &[1, 2]));
        assert!(!removal_must_wait(&[], &[1, 2]));
    }

    #[test]
    fn the_osds_in_a_virtual_disks_crush_host_are_read_from_the_tree() {
        let tree = serde_json::json!({"nodes": [
            {"id": -1, "name": "default", "type": "root", "children": [-3, -9]},
            {"id": -3, "name": "node1", "type": "host", "children": [0, 1]},
            {"id": -9, "name": "vdisk-4", "type": "host", "children": [6]},
            {"id": 6, "name": "osd.6", "type": "osd"}
        ]});
        assert_eq!(crush_children(&tree, "vdisk-4"), vec![6]);
        assert_eq!(crush_children(&tree, "vdisk-5"), Vec::<i64>::new());
    }

    #[test]
    fn a_disk_attached_without_a_record_is_detected_from_the_run_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("run/yolab/vdisks/12/mnt")).unwrap();
        std::fs::create_dir_all(root.path().join("run/yolab/vdisks/3")).unwrap();
        assert_eq!(stack::attached_here(root.path()).unwrap(), vec![3, 12]);
    }

    #[test]
    fn the_osd_on_a_virtual_disk_is_found_from_its_block_link_even_when_detached() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("var/lib/ceph/osd/ceph-8");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/dev/yolab_vdisk_4/osd", dir.join("block")).unwrap();
        let other = root.path().join("var/lib/ceph/osd/ceph-2");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink("/dev/pool/ceph", other.join("block")).unwrap();
        assert_eq!(osd_on(root.path(), 4), Some(8));
        assert_eq!(vdisk_of_osd(root.path(), 2), None);
        assert_eq!(osd_on(root.path(), 5), None);
    }
}
