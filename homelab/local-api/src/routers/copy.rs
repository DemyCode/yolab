use std::time::Duration;

use serde_json::{json, Value};

use crate::error::Outcome;

const SNAPSHOT_CLASS: &str = "csi-cephfs-snapclass";
const CEPHFS_STORAGE_CLASS: &str = "yolab-cephfs";
const SNAPSHOT_WAIT_SECS: u64 = 300;
const CLONE_WAIT_SECS: u64 = 6 * 60 * 60;
const POLL_SECS: u64 = 5;

#[derive(Debug, PartialEq)]
struct SourceVolume {
    capacity: String,
    access_modes: Vec<String>,
    storage_class: String,
}

#[derive(Debug, PartialEq)]
struct SnapshotRef {
    handle: String,
    driver: String,
}

pub(crate) async fn copy_live_volumes(
    source_namespace: &str,
    dest_namespace: &str,
    instance_name: &str,
) -> anyhow::Result<()> {
    let sources = crate::routers::backup_common::list_user_pvcs()
        .await?
        .into_iter()
        .filter(|p| p.namespace == source_namespace)
        .collect::<Vec<_>>();

    for pvc in &sources {
        copy_one(source_namespace, dest_namespace, instance_name, &pvc.name).await?;
    }
    Ok(())
}

async fn copy_one(
    source_namespace: &str,
    dest_namespace: &str,
    instance_name: &str,
    pvc_name: &str,
) -> anyhow::Result<()> {
    let source =
        crate::kubectl::get_json(&["get", "pvc", pvc_name, "-n", source_namespace, "-o", "json"])
            .await?;
    let volume = parse_source_volume(&source).ok_or_else(|| {
        anyhow::anyhow!("{source_namespace}/{pvc_name}: could not read its storage")
    })?;
    if volume.storage_class != CEPHFS_STORAGE_CLASS {
        anyhow::bail!(
            "{source_namespace}/{pvc_name} is on {}, not {CEPHFS_STORAGE_CLASS}, \
             so it cannot be copied directly",
            volume.storage_class
        );
    }

    let suffix = crate::routers::backup_common::random_hex(4);
    let src_snap = format!("yolab-copy-{suffix}");
    let dst_snap = format!("yolab-copy-{suffix}");
    let content = format!("yolab-copy-{suffix}");

    let source_instance = source_namespace.trim_start_matches("yolab-");
    let dest_name =
        crate::routers::backup_common::rebase_pvc_name(pvc_name, source_instance, instance_name);

    crate::kubectl::apply(
        &source_snapshot_manifest(&src_snap, source_namespace, pvc_name).to_string(),
    )
    .await?;
    let mut guard = CleanupGuard {
        armed: true,
        dest_namespace: dest_namespace.to_string(),
        dest_snapshot: dst_snap.clone(),
        content: content.clone(),
        source_namespace: source_namespace.to_string(),
        source_snapshot: src_snap.clone(),
    };
    wait_for_snapshot_ready(source_namespace, &src_snap).await?;
    let snap_ref = snapshot_ref_of(source_namespace, &src_snap).await?;

    crate::kubectl::apply(
        &rebind_content_manifest(&content, dest_namespace, &dst_snap, &snap_ref).to_string(),
    )
    .await?;
    crate::kubectl::apply(
        &rebound_snapshot_manifest(&dst_snap, dest_namespace, &content).to_string(),
    )
    .await?;
    wait_for_snapshot_ready(dest_namespace, &dst_snap).await?;

    crate::kubectl::apply(
        &destination_pvc_manifest(
            &dest_name,
            dest_namespace,
            instance_name,
            &volume.capacity,
            &volume.access_modes,
            &dst_snap,
        )
        .to_string(),
    )
    .await?;
    guard.armed = false;
    spawn_clone_cleanup(
        dest_namespace.to_string(),
        dest_name,
        dst_snap,
        content,
        source_namespace.to_string(),
        src_snap,
    );
    Ok(())
}

fn spawn_clone_cleanup(
    dest_namespace: String,
    dest_pvc: String,
    dest_snapshot: String,
    content: String,
    source_namespace: String,
    source_snapshot: String,
) {
    tokio::spawn(async move {
        let _ = wait_for_pvc_bound(&dest_namespace, &dest_pvc, CLONE_WAIT_SECS).await;
        cleanup(
            &dest_namespace,
            &dest_snapshot,
            &content,
            &source_namespace,
            &source_snapshot,
        )
        .await;
    });
}

fn source_snapshot_manifest(name: &str, namespace: &str, pvc: &str) -> Value {
    json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshot",
        "metadata": { "name": name, "namespace": namespace },
        "spec": {
            "volumeSnapshotClassName": SNAPSHOT_CLASS,
            "source": { "persistentVolumeClaimName": pvc }
        }
    })
}

fn rebind_content_manifest(
    name: &str,
    dest_namespace: &str,
    snapshot_name: &str,
    snap_ref: &SnapshotRef,
) -> Value {
    json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshotContent",
        "metadata": { "name": name },
        "spec": {
            "deletionPolicy": "Retain",
            "driver": snap_ref.driver,
            "source": { "snapshotHandle": snap_ref.handle },
            "volumeSnapshotRef": {
                "kind": "VolumeSnapshot",
                "name": snapshot_name,
                "namespace": dest_namespace,
            }
        }
    })
}

fn rebound_snapshot_manifest(name: &str, namespace: &str, content_name: &str) -> Value {
    json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshot",
        "metadata": { "name": name, "namespace": namespace },
        "spec": { "source": { "volumeSnapshotContentName": content_name } }
    })
}

fn destination_pvc_manifest(
    name: &str,
    namespace: &str,
    release: &str,
    capacity: &str,
    access_modes: &[String],
    snapshot_name: &str,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "Helm" },
            "annotations": {
                "meta.helm.sh/release-name": release,
                "meta.helm.sh/release-namespace": namespace,
            }
        },
        "spec": {
            "accessModes": access_modes,
            "storageClassName": CEPHFS_STORAGE_CLASS,
            "resources": { "requests": { "storage": capacity } },
            "dataSource": {
                "kind": "VolumeSnapshot",
                "name": snapshot_name,
                "apiGroup": "snapshot.storage.k8s.io",
            }
        }
    })
}

fn parse_source_volume(pvc: &Value) -> Option<SourceVolume> {
    let spec = pvc["spec"].as_object()?;
    Some(SourceVolume {
        capacity: pvc["spec"]["resources"]["requests"]["storage"]
            .as_str()?
            .to_string(),
        access_modes: spec
            .get("accessModes")?
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(String::from))
            .collect(),
        storage_class: spec.get("storageClassName")?.as_str()?.to_string(),
    })
}

fn bound_content_name(snapshot: &Value) -> Option<String> {
    snapshot["status"]["boundVolumeSnapshotContentName"]
        .as_str()
        .map(String::from)
}

fn snapshot_ref(vsc: &Value) -> Option<SnapshotRef> {
    let handle = vsc["status"]["snapshotHandle"]
        .as_str()
        .or_else(|| vsc["spec"]["source"]["snapshotHandle"].as_str())?;
    Some(SnapshotRef {
        handle: handle.to_string(),
        driver: vsc["spec"]["driver"].as_str()?.to_string(),
    })
}

async fn snapshot_ref_of(namespace: &str, snapshot: &str) -> anyhow::Result<SnapshotRef> {
    let snap = crate::kubectl::get_json(&[
        "get",
        "volumesnapshot",
        snapshot,
        "-n",
        namespace,
        "-o",
        "json",
    ])
    .await?;
    let content = bound_content_name(&snap)
        .ok_or_else(|| anyhow::anyhow!("{namespace}/{snapshot} has no bound snapshot content"))?;
    let vsc =
        crate::kubectl::get_json(&["get", "volumesnapshotcontent", &content, "-o", "json"]).await?;
    snapshot_ref(&vsc).ok_or_else(|| anyhow::anyhow!("{content} carries no snapshot handle"))
}

async fn wait_for_snapshot_ready(namespace: &str, snapshot: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(SNAPSHOT_WAIT_SECS);
    loop {
        let v = crate::kubectl::get_json(&[
            "get",
            "volumesnapshot",
            snapshot,
            "-n",
            namespace,
            "-o",
            "json",
        ])
        .await
        .ok();
        if let Some(err) = v
            .as_ref()
            .and_then(|v| v["status"]["error"]["message"].as_str())
        {
            anyhow::bail!("snapshot {namespace}/{snapshot} failed: {err}");
        }
        if v.as_ref()
            .and_then(|v| v["status"]["readyToUse"].as_bool())
            .unwrap_or(false)
        {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "snapshot {namespace}/{snapshot} was not ready after {SNAPSHOT_WAIT_SECS}s"
            );
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
}

async fn wait_for_pvc_bound(namespace: &str, pvc: &str, timeout_secs: u64) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let v = crate::kubectl::get_json(&["get", "pvc", pvc, "-n", namespace, "-o", "json"]).await;
        match v {
            Err(e) if crate::kubectl::is_not_found(&e) => {
                anyhow::bail!("{namespace}/{pvc} was removed before it bound")
            }
            Err(_) => {}
            Ok(v) => match v["status"]["phase"].as_str() {
                Some("Bound") => return Ok(()),
                Some(other) if other != "Pending" => {
                    anyhow::bail!("{namespace}/{pvc} entered {other} instead of binding")
                }
                _ => {}
            },
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("{namespace}/{pvc} did not bind after {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
}

async fn cleanup(
    dest_namespace: &str,
    dest_snapshot: &str,
    content: &str,
    source_namespace: &str,
    source_snapshot: &str,
) {
    let steps: [(&str, &str, Option<&str>); 3] = [
        ("volumesnapshot", dest_snapshot, Some(dest_namespace)),
        ("volumesnapshotcontent", content, None),
        ("volumesnapshot", source_snapshot, Some(source_namespace)),
    ];
    for (kind, name, namespace) in steps {
        let mut args = vec!["delete", kind, name, "--ignore-not-found", "--wait=false"];
        if let Some(ns) = namespace {
            args.push("-n");
            args.push(ns);
        }
        crate::kubectl::run(&args)
            .await
            .debug_on_err(format!("copy cleanup: delete {kind} {name}"));
    }
}

struct CleanupGuard {
    armed: bool,
    dest_namespace: String,
    dest_snapshot: String,
    content: String,
    source_namespace: String,
    source_snapshot: String,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let args = (
            self.dest_namespace.clone(),
            self.dest_snapshot.clone(),
            self.content.clone(),
            self.source_namespace.clone(),
            self.source_snapshot.clone(),
        );
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("copy cleanup: no runtime left to drop the temporary snapshots");
            return;
        };
        handle.spawn(async move {
            cleanup(&args.0, &args.1, &args.2, &args.3, &args.4).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_snapshot_points_at_the_pvc_and_the_snapclass() {
        let m = source_snapshot_manifest("s", "yolab-gitea-ab12", "data");
        assert_eq!(m["kind"], "VolumeSnapshot");
        assert_eq!(m["spec"]["volumeSnapshotClassName"], SNAPSHOT_CLASS);
        assert_eq!(m["spec"]["source"]["persistentVolumeClaimName"], "data");
    }

    #[test]
    fn a_rebind_keeps_the_handle_but_retargets_the_destination_namespace() {
        let m = rebind_content_manifest(
            "c",
            "yolab-gitea-cd34",
            "s",
            &SnapshotRef {
                handle: "0xabc".into(),
                driver: "rook-ceph.cephfs.csi.ceph.com".into(),
            },
        );
        assert_eq!(m["spec"]["source"]["snapshotHandle"], "0xabc");
        assert_eq!(m["spec"]["deletionPolicy"], "Retain");
        assert_eq!(m["spec"]["volumeSnapshotRef"]["name"], "s");
        assert_eq!(
            m["spec"]["volumeSnapshotRef"]["namespace"],
            "yolab-gitea-cd34"
        );
    }

    #[test]
    fn a_rebound_snapshot_binds_to_its_content() {
        let m = rebound_snapshot_manifest("s", "yolab-gitea-cd34", "c");
        assert_eq!(m["kind"], "VolumeSnapshot");
        assert_eq!(m["spec"]["source"]["volumeSnapshotContentName"], "c");
    }

    #[test]
    fn the_destination_pvc_is_helm_owned_and_cloned_from_the_snapshot() {
        let m = destination_pvc_manifest(
            "data",
            "yolab-gitea-cd34",
            "gitea-cd34",
            "20Gi",
            &["ReadWriteMany".into()],
            "s",
        );
        assert_eq!(
            m["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "Helm"
        );
        assert_eq!(
            m["metadata"]["annotations"]["meta.helm.sh/release-name"],
            "gitea-cd34"
        );
        assert_eq!(m["spec"]["dataSource"]["kind"], "VolumeSnapshot");
        assert_eq!(m["spec"]["dataSource"]["name"], "s");
        assert_eq!(
            m["spec"]["dataSource"]["apiGroup"],
            "snapshot.storage.k8s.io"
        );
        assert_eq!(m["spec"]["resources"]["requests"]["storage"], "20Gi");
    }

    #[test]
    fn a_source_volume_is_read_with_capacity_modes_and_class() {
        let pvc = json!({
            "spec": {
                "accessModes": ["ReadWriteMany"],
                "storageClassName": "yolab-cephfs",
                "resources": { "requests": { "storage": "20Gi" } }
            }
        });
        assert_eq!(
            parse_source_volume(&pvc),
            Some(SourceVolume {
                capacity: "20Gi".into(),
                access_modes: vec!["ReadWriteMany".into()],
                storage_class: "yolab-cephfs".into(),
            })
        );
    }

    #[test]
    fn a_source_volume_that_lacks_storage_class_is_not_read() {
        let pvc = json!({
            "spec": {
                "accessModes": ["ReadWriteMany"],
                "resources": { "requests": { "storage": "20Gi" } }
            }
        });
        assert_eq!(parse_source_volume(&pvc), None);
    }

    #[test]
    fn the_snapshot_ref_prefers_the_drivers_own_status_handle() {
        let vsc = json!({
            "spec": {
                "driver": "rook-ceph.cephfs.csi.ceph.com",
                "source": { "volumeHandle": "source-volume-handle" }
            },
            "status": { "snapshotHandle": "cephfs-snapshot-id" }
        });
        assert_eq!(
            snapshot_ref(&vsc),
            Some(SnapshotRef {
                handle: "cephfs-snapshot-id".into(),
                driver: "rook-ceph.cephfs.csi.ceph.com".into(),
            })
        );
    }

    #[test]
    fn the_snapshot_ref_falls_back_to_the_spec_handle() {
        let vsc = json!({
            "spec": {
                "driver": "rook-ceph.cephfs.csi.ceph.com",
                "source": { "snapshotHandle": "abc-123" }
            }
        });
        assert_eq!(
            snapshot_ref(&vsc),
            Some(SnapshotRef {
                handle: "abc-123".into(),
                driver: "rook-ceph.cephfs.csi.ceph.com".into(),
            })
        );
        assert_eq!(snapshot_ref(&json!({})), None);
    }

    #[test]
    fn a_snapshot_reports_its_bound_content_only_once_bound() {
        assert_eq!(bound_content_name(&json!({})), None);
        assert_eq!(
            bound_content_name(&json!({"status": {"boundVolumeSnapshotContentName": "c"}})),
            Some("c".into())
        );
    }
}
