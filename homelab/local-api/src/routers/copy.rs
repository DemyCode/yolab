use std::time::Duration;

use serde_json::{json, Value};

use crate::error::Outcome;
use kube::Client;

const SNAPSHOT_CLASS: &str = "csi-cephfs-snapclass";
const CEPHFS_STORAGE_CLASS: &str = "yolab-cephfs";
const SNAPSHOT_WAIT_SECS: u64 = 300;
const CLONE_WAIT_SECS: u64 = 6 * 60 * 60;
const POLL_SECS: u64 = 5;
const REBASE_WAIT_SECS: u64 = 300;
const REBASE_IMAGE: &str =
    "ghcr.io/demycode/wg-register:main-latest@sha256:d32d0f30515ef94d3c89eb38a738d565a60f828953e920ba640d1eca73373bd9";
const COPY_LABEL: &str = "yolab.io/copy";
const COPY_SELECTOR: &str = "yolab.io/copy=true";
const ANN_DEST_NAMESPACE: &str = "yolab.io/copy-dest-namespace";
const ANN_DEST_PVC: &str = "yolab.io/copy-dest-pvc";
const SWEEP_TICK_SECS: u64 = 600;
const ABANDONED_AFTER_SECS: i64 = 12 * 3600;

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
    client: &Client,
    source_namespace: &str,
    dest_namespace: &str,
    instance_name: &str,
) -> anyhow::Result<()> {
    let sources = crate::routers::backup_common::user_pvcs(client)
        .await?
        .into_iter()
        .filter(|p| p.namespace == source_namespace)
        .collect::<Vec<_>>();

    for pvc in &sources {
        copy_one(
            client,
            source_namespace,
            dest_namespace,
            instance_name,
            &pvc.name,
        )
        .await?;
    }
    Ok(())
}

async fn copy_one(
    client: &Client,
    source_namespace: &str,
    dest_namespace: &str,
    instance_name: &str,
    pvc_name: &str,
) -> anyhow::Result<()> {
    let source = crate::k8s::get(
        client,
        &crate::k8s::reference("v1", "PersistentVolumeClaim", source_namespace, pvc_name),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("{source_namespace}/{pvc_name} does not exist"))?;
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

    let copy_name = format!(
        "yolab-copy-{}",
        crate::routers::backup_common::random_hex(4)
    );

    let source_instance = source_namespace.trim_start_matches("yolab-");
    let dest_name =
        crate::routers::backup_common::rebase_pvc_name(pvc_name, source_instance, instance_name);

    crate::k8s::apply(
        client,
        &source_snapshot_manifest(
            &copy_name,
            source_namespace,
            pvc_name,
            dest_namespace,
            &dest_name,
        ),
    )
    .await?;
    let mut guard = CleanupGuard {
        armed: true,
        client: client.clone(),
        name: copy_name.clone(),
        source_namespace: source_namespace.to_string(),
        dest_namespace: dest_namespace.to_string(),
    };
    wait_for_snapshot_ready(client, source_namespace, &copy_name).await?;
    let snap_ref = snapshot_ref_of(client, source_namespace, &copy_name).await?;

    crate::k8s::apply(
        client,
        &rebind_content_manifest(&copy_name, dest_namespace, &copy_name, &snap_ref),
    )
    .await?;
    crate::k8s::apply(
        client,
        &rebound_snapshot_manifest(&copy_name, dest_namespace, &copy_name),
    )
    .await?;
    wait_for_snapshot_ready(client, dest_namespace, &copy_name).await?;

    crate::k8s::apply(
        client,
        &destination_pvc_manifest(
            &dest_name,
            dest_namespace,
            instance_name,
            &volume.capacity,
            &volume.access_modes,
            &copy_name,
        ),
    )
    .await?;
    guard.armed = false;
    spawn_clone_cleanup(
        client.clone(),
        source_namespace.to_string(),
        dest_namespace.to_string(),
        dest_name.clone(),
        copy_name,
    );
    wait_for_pvc_bound(client, dest_namespace, &dest_name, CLONE_WAIT_SECS).await?;
    rebase_release_dir(
        client,
        dest_namespace,
        &dest_name,
        source_instance,
        instance_name,
    )
    .await?;
    Ok(())
}

fn spawn_clone_cleanup(
    client: Client,
    source_namespace: String,
    dest_namespace: String,
    dest_pvc: String,
    name: String,
) {
    tokio::spawn(async move {
        let _ = wait_for_pvc_bound(&client, &dest_namespace, &dest_pvc, CLONE_WAIT_SECS).await;
        cleanup(&client, &name, &source_namespace, &dest_namespace).await;
    });
}

fn source_snapshot_manifest(
    name: &str,
    namespace: &str,
    pvc: &str,
    dest_namespace: &str,
    dest_pvc: &str,
) -> Value {
    json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshot",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": { COPY_LABEL: "true" },
            "annotations": {
                ANN_DEST_NAMESPACE: dest_namespace,
                ANN_DEST_PVC: dest_pvc,
            }
        },
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
            "deletionPolicy": "Delete",
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

// Every chart mounts its data under a release-named directory inside its PVC
// (`subPath: {{ .Release.Name }}/...`). A clone is a byte copy, so it still
// carries the source release's directory; rename it onto the destination
// release or the new app starts on an empty volume and makes a fresh world.
fn rebase_job_manifest(
    name: &str,
    namespace: &str,
    pvc: &str,
    source_dir: &str,
    dest_dir: &str,
) -> Value {
    let command = format!(
        "if [ -d /data/{source_dir} ] && [ ! -e /data/{dest_dir} ]; then \
         mv /data/{source_dir} /data/{dest_dir}; fi"
    );
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": { "name": name, "namespace": namespace },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": REBASE_WAIT_SECS,
            "ttlSecondsAfterFinished": 600,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "rebase",
                        "image": REBASE_IMAGE,
                        "command": ["/bin/sh", "-c"],
                        "args": [command],
                        "volumeMounts": [{"name": "data", "mountPath": "/data"}]
                    }],
                    "volumes": [{
                        "name": "data",
                        "persistentVolumeClaim": {"claimName": pvc}
                    }]
                }
            }
        }
    })
}

async fn rebase_release_dir(
    client: &Client,
    namespace: &str,
    pvc: &str,
    source_dir: &str,
    dest_dir: &str,
) -> anyhow::Result<()> {
    if source_dir == dest_dir {
        return Ok(());
    }
    let name = format!(
        "yolab-rebase-{}",
        crate::routers::backup_common::random_hex(4)
    );
    crate::k8s::apply(
        client,
        &rebase_job_manifest(&name, namespace, pvc, source_dir, dest_dir),
    )
    .await?;
    let reference = crate::k8s::reference("batch/v1", "Job", namespace, &name);
    let deadline = std::time::Instant::now() + Duration::from_secs(REBASE_WAIT_SECS);
    let outcome = loop {
        match crate::k8s::get(client, &reference).await {
            Ok(Some(job)) => {
                if job["status"]["succeeded"].as_i64().unwrap_or(0) >= 1 {
                    break Ok(());
                }
                if job["status"]["failed"].as_i64().unwrap_or(0) >= 1 {
                    break Err(anyhow::anyhow!(
                        "{namespace}/{pvc}: could not move {source_dir} to {dest_dir}"
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if std::time::Instant::now() > deadline {
            break Err(anyhow::anyhow!(
                "{namespace}/{pvc}: moving {source_dir} to {dest_dir} did not finish"
            ));
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    };
    crate::k8s::delete_with_dependents(client, &reference)
        .await
        .debug_on_err(format!("copy rebase: delete Job {name}"));
    outcome
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

fn volume_snapshot(namespace: &str, name: &str) -> Value {
    crate::k8s::reference(
        "snapshot.storage.k8s.io/v1",
        "VolumeSnapshot",
        namespace,
        name,
    )
}

fn volume_snapshot_content(name: &str) -> Value {
    crate::k8s::cluster_reference("snapshot.storage.k8s.io/v1", "VolumeSnapshotContent", name)
}

fn claim(namespace: &str, name: &str) -> Value {
    crate::k8s::reference("v1", "PersistentVolumeClaim", namespace, name)
}

async fn snapshot_ref_of(
    client: &Client,
    namespace: &str,
    snapshot: &str,
) -> anyhow::Result<SnapshotRef> {
    let snap = crate::k8s::get(client, &volume_snapshot(namespace, snapshot))
        .await?
        .ok_or_else(|| anyhow::anyhow!("{namespace}/{snapshot} is gone"))?;
    let content = bound_content_name(&snap)
        .ok_or_else(|| anyhow::anyhow!("{namespace}/{snapshot} has no bound snapshot content"))?;
    let vsc = crate::k8s::get(client, &volume_snapshot_content(&content))
        .await?
        .ok_or_else(|| anyhow::anyhow!("{content} is gone"))?;
    snapshot_ref(&vsc).ok_or_else(|| anyhow::anyhow!("{content} carries no snapshot handle"))
}

async fn wait_for_snapshot_ready(
    client: &Client,
    namespace: &str,
    snapshot: &str,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(SNAPSHOT_WAIT_SECS);
    loop {
        let v = crate::k8s::get(client, &volume_snapshot(namespace, snapshot))
            .await
            .ok()
            .flatten();
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

async fn wait_for_pvc_bound(
    client: &Client,
    namespace: &str,
    pvc: &str,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match crate::k8s::get(client, &claim(namespace, pvc)).await {
            Ok(None) => anyhow::bail!("{namespace}/{pvc} was removed before it bound"),
            Err(_) => {}
            Ok(Some(v)) => match v["status"]["phase"].as_str() {
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

async fn cleanup(client: &Client, name: &str, source_namespace: &str, dest_namespace: &str) {
    let steps = [
        volume_snapshot(dest_namespace, name),
        volume_snapshot_content(name),
        volume_snapshot(source_namespace, name),
    ];
    for step in steps {
        crate::k8s::delete_if_present(client, &step)
            .await
            .debug_on_err(format!(
                "copy cleanup: delete {} {name}",
                step["kind"].as_str().unwrap_or("object")
            ));
    }
}

struct CleanupGuard {
    armed: bool,
    client: Client,
    name: String,
    source_namespace: String,
    dest_namespace: String,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let client = self.client.clone();
        let args = (
            self.name.clone(),
            self.source_namespace.clone(),
            self.dest_namespace.clone(),
        );
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("copy cleanup: no runtime left to drop the temporary snapshots");
            return;
        };
        handle.spawn(async move {
            cleanup(&client, &args.0, &args.1, &args.2).await;
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Sweep {
    Keep,
    Clean(&'static str),
}

pub(crate) fn sweep_verdict(
    dest_namespace_exists: bool,
    dest_pvc_phase: Option<&str>,
    age_secs: i64,
) -> Sweep {
    if !dest_namespace_exists {
        return Sweep::Clean("the app it was copying into is gone");
    }
    if dest_pvc_phase == Some("Bound") {
        return Sweep::Clean("the copy finished");
    }
    if age_secs >= ABANDONED_AFTER_SECS {
        return Sweep::Clean("it was abandoned");
    }
    Sweep::Keep
}

fn age_secs(created: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> i64 {
    created
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0)
}

struct LeftoverCopy {
    name: String,
    namespace: String,
    dest_namespace: String,
    dest_pvc: String,
    age_secs: i64,
}

fn leftover_copies(list: &Value, now: chrono::DateTime<chrono::Utc>) -> Vec<LeftoverCopy> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let meta = &item["metadata"];
            let ann = &meta["annotations"];
            Some(LeftoverCopy {
                name: meta["name"].as_str()?.to_string(),
                namespace: meta["namespace"].as_str()?.to_string(),
                dest_namespace: ann[ANN_DEST_NAMESPACE].as_str()?.to_string(),
                dest_pvc: ann[ANN_DEST_PVC].as_str()?.to_string(),
                age_secs: age_secs(meta["creationTimestamp"].as_str(), now),
            })
        })
        .collect()
}

async fn namespace_exists(client: &Client, namespace: &str) -> bool {
    crate::k8s::exists(
        client,
        &crate::k8s::cluster_reference("v1", "Namespace", namespace),
    )
    .await
    .unwrap_or(true)
}

async fn pvc_phase(client: &Client, namespace: &str, pvc: &str) -> Option<String> {
    crate::k8s::get(client, &claim(namespace, pvc))
        .await
        .ok()
        .flatten()
        .and_then(|v| v["status"]["phase"].as_str().map(String::from))
}

pub struct CopySweeperController;

impl crate::runtime::Controller for CopySweeperController {
    fn name(&self) -> &'static str {
        "copy-sweeper"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(SWEEP_TICK_SECS)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        sweep_once(&crate::k8s::client().await?, chrono::Utc::now()).await
    }
}

async fn sweep_once(
    client: &Client,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<crate::runtime::Tick> {
    let labelled = kube::api::ListParams::default().labels(COPY_SELECTOR);
    let items = crate::k8s::list(
        client,
        "snapshot.storage.k8s.io/v1",
        "VolumeSnapshot",
        None,
        &labelled,
    )
    .await?;
    let list = json!({ "items": items });
    let mut swept = 0usize;
    for leftover in leftover_copies(&list, now) {
        let exists = namespace_exists(client, &leftover.dest_namespace).await;
        let phase = if exists {
            pvc_phase(client, &leftover.dest_namespace, &leftover.dest_pvc).await
        } else {
            None
        };
        let Sweep::Clean(why) = sweep_verdict(exists, phase.as_deref(), leftover.age_secs) else {
            continue;
        };
        tracing::info!(
            "copy sweeper: clearing {}/{} — {why}",
            leftover.namespace,
            leftover.name
        );
        cleanup(
            client,
            &leftover.name,
            &leftover.namespace,
            &leftover.dest_namespace,
        )
        .await;
        swept += 1;
    }
    if swept == 0 {
        Ok(crate::runtime::Tick::Idle(
            "no leftover copy snapshots".into(),
        ))
    } else {
        Ok(crate::runtime::Tick::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{api_server, list, serve, status};

    #[test]
    fn a_source_snapshot_points_at_the_pvc_and_the_snapclass() {
        let m = source_snapshot_manifest(
            "s",
            "yolab-gitea-ab12",
            "data",
            "yolab-gitea-cd34",
            "gitea-cd34-data",
        );
        assert_eq!(m["kind"], "VolumeSnapshot");
        assert_eq!(m["spec"]["volumeSnapshotClassName"], SNAPSHOT_CLASS);
        assert_eq!(m["spec"]["source"]["persistentVolumeClaimName"], "data");
    }

    #[test]
    fn a_source_snapshot_records_where_it_was_copying_to_so_it_can_be_swept() {
        let m = source_snapshot_manifest(
            "s",
            "yolab-gitea-ab12",
            "data",
            "yolab-gitea-cd34",
            "gitea-cd34-data",
        );
        assert_eq!(m["metadata"]["labels"][COPY_LABEL], "true");
        assert_eq!(
            m["metadata"]["annotations"][ANN_DEST_NAMESPACE],
            "yolab-gitea-cd34"
        );
        assert_eq!(
            m["metadata"]["annotations"][ANN_DEST_PVC],
            "gitea-cd34-data"
        );
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
        assert_eq!(
            m["spec"]["deletionPolicy"],
            "Delete",
            "whichever side is deleted first takes the ceph snapshot with it, so none outlives the copy"
        );
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
    fn the_rebase_job_moves_the_source_release_dir_onto_the_new_one() {
        let m = rebase_job_manifest(
            "yolab-rebase-abcd",
            "yolab-gitea-cd34",
            "gitea-cd34-data",
            "gitea-ab12",
            "gitea-cd34",
        );
        assert_eq!(m["kind"], "Job");
        let container = &m["spec"]["template"]["spec"]["containers"][0];
        let command = container["args"][0].as_str().unwrap();
        assert!(command.contains("/data/gitea-ab12"), "{command}");
        assert!(command.contains("/data/gitea-cd34"), "{command}");
        assert_eq!(container["volumeMounts"][0]["mountPath"], "/data");
        assert_eq!(
            m["spec"]["template"]["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"],
            "gitea-cd34-data"
        );
    }

    #[test]
    fn the_rebase_job_only_moves_data_and_leaves_identities_to_the_chart() {
        let m = rebase_job_manifest(
            "yolab-rebase-abcd",
            "yolab-gitea-cd34",
            "gitea-cd34-data",
            "gitea-ab12",
            "gitea-cd34",
        );
        let command = m["spec"]["template"]["spec"]["containers"][0]["args"][0]
            .as_str()
            .unwrap();
        assert!(!command.contains("rm "), "{command}");
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

    #[test]
    fn a_copy_still_running_is_left_alone() {
        assert_eq!(sweep_verdict(true, Some("Pending"), 60), Sweep::Keep);
    }

    #[test]
    fn a_finished_copy_has_its_temporary_snapshots_cleared() {
        assert!(matches!(
            sweep_verdict(true, Some("Bound"), 60),
            Sweep::Clean(_)
        ));
    }

    #[test]
    fn a_copy_whose_destination_was_rolled_back_is_cleared() {
        assert!(
            matches!(sweep_verdict(false, None, 60), Sweep::Clean(_)),
            "a failed install deletes the namespace — its snapshots must not outlive it"
        );
    }

    #[test]
    fn a_copy_nobody_is_driving_any_more_is_cleared_once_it_is_old() {
        assert_eq!(
            sweep_verdict(true, Some("Pending"), ABANDONED_AFTER_SECS - 1),
            Sweep::Keep,
            "a long clone of a large volume is not abandoned"
        );
        assert!(matches!(
            sweep_verdict(true, Some("Pending"), ABANDONED_AFTER_SECS),
            Sweep::Clean(_)
        ));
    }

    #[test]
    fn the_abandoned_cutoff_outlasts_the_longest_clone_we_wait_for() {
        assert!(
            ABANDONED_AFTER_SECS > CLONE_WAIT_SECS as i64,
            "sweeping sooner than the clone wait would cut a running copy off at the knees"
        );
    }

    #[test]
    fn leftovers_are_read_with_where_they_were_going_and_how_old_they_are() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let list = json!({"items": [{
            "metadata": {
                "name": "yolab-copy-abcd1234",
                "namespace": "yolab-gitea-ab12",
                "creationTimestamp": "2026-09-25T11:00:00Z",
                "annotations": {
                    ANN_DEST_NAMESPACE: "yolab-gitea-cd34",
                    ANN_DEST_PVC: "gitea-cd34-data",
                }
            }
        }]});
        let found = leftover_copies(&list, now);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dest_namespace, "yolab-gitea-cd34");
        assert_eq!(found[0].dest_pvc, "gitea-cd34-data");
        assert_eq!(found[0].age_secs, 3600);
    }

    #[test]
    fn a_snapshot_without_the_copy_annotations_is_not_touched() {
        let now = chrono::Utc::now();
        let list = json!({"items": [{
            "metadata": {"name": "someone-elses", "namespace": "yolab-gitea-ab12"}
        }]});
        assert!(leftover_copies(&list, now).is_empty());
    }

    #[test]
    fn a_leftover_with_no_creation_stamp_is_not_treated_as_ancient() {
        let now = chrono::Utc::now();
        let list = json!({"items": [{
            "metadata": {
                "name": "yolab-copy-abcd1234",
                "namespace": "yolab-gitea-ab12",
                "annotations": {
                    ANN_DEST_NAMESPACE: "yolab-gitea-cd34",
                    ANN_DEST_PVC: "gitea-cd34-data",
                }
            }
        }]});
        assert_eq!(leftover_copies(&list, now)[0].age_secs, 0);
    }

    #[test]
    fn the_sweeper_selector_matches_the_label_the_copy_writes() {
        assert_eq!(COPY_SELECTOR, format!("{COPY_LABEL}=true"));
    }

    fn leftover_list(created: &str) -> Value {
        json!({"items": [{
            "metadata": {
                "name": "yolab-copy-abcd1234",
                "namespace": "yolab-gitea-ab12",
                "creationTimestamp": created,
                "annotations": {
                    ANN_DEST_NAMESPACE: "yolab-gitea-cd34",
                    ANN_DEST_PVC: "gitea-cd34-data",
                }
            }
        }]})
    }

    fn at(stamp: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    const DEST_PVC: &str =
        "/api/v1/namespaces/yolab-gitea-cd34/persistentvolumeclaims/gitea-cd34-data";
    const DEST_NS: &str = "/api/v1/namespaces/yolab-gitea-cd34";

    async fn deletions(server: &wiremock::MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "DELETE")
            .map(|r| r.url.path().to_string())
            .collect()
    }

    async fn deletes_accepted(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn cleanup_clears_the_destination_snapshot_then_its_content_then_the_source() {
        let (server, client) = api_server().await;
        deletes_accepted(&server).await;
        cleanup(
            &client,
            "yolab-copy-abcd1234",
            "yolab-gitea-ab12",
            "yolab-gitea-cd34",
        )
        .await;

        assert_eq!(
            deletions(&server).await,
            vec![
                "/apis/snapshot.storage.k8s.io/v1/namespaces/yolab-gitea-cd34/volumesnapshots/yolab-copy-abcd1234",
                "/apis/snapshot.storage.k8s.io/v1/volumesnapshotcontents/yolab-copy-abcd1234",
                "/apis/snapshot.storage.k8s.io/v1/namespaces/yolab-gitea-ab12/volumesnapshots/yolab-copy-abcd1234",
            ],
            "the content is retained, so it has to go before the source snapshot it was cut from"
        );
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_not_mistaken_for_a_deleted_namespace() {
        let (gone, client) = api_server().await;
        serve(&gone, DEST_NS, 404, status(404, "NotFound")).await;
        assert!(!namespace_exists(&client, "yolab-gitea-cd34").await);

        let (down, client) = api_server().await;
        serve(&down, DEST_NS, 503, status(503, "ServiceUnavailable")).await;
        assert!(
            namespace_exists(&client, "yolab-gitea-cd34").await,
            "reading an unreachable cluster as a deleted namespace would sweep a live copy"
        );
    }

    #[tokio::test]
    async fn a_pvc_phase_is_read_and_a_pvc_that_is_gone_has_none() {
        let (bound, client) = api_server().await;
        serve(
            &bound,
            DEST_PVC,
            200,
            json!({ "metadata": { "name": "gitea-cd34-data" }, "status": { "phase": "Bound" } }),
        )
        .await;
        assert_eq!(
            pvc_phase(&client, "yolab-gitea-cd34", "gitea-cd34-data").await,
            Some("Bound".to_string())
        );

        let (missing, client) = api_server().await;
        serve(&missing, DEST_PVC, 404, status(404, "NotFound")).await;
        assert_eq!(
            pvc_phase(&client, "yolab-gitea-cd34", "gitea-cd34-data").await,
            None
        );
    }

    const DATA_PVC: &str = "/api/v1/namespaces/yolab-gitea-cd34/persistentvolumeclaims/data";

    #[tokio::test]
    async fn a_destination_pvc_that_was_removed_ends_the_wait_instead_of_spinning() {
        let (server, client) = api_server().await;
        serve(&server, DATA_PVC, 404, status(404, "NotFound")).await;
        let e = wait_for_pvc_bound(&client, "yolab-gitea-cd34", "data", 60)
            .await
            .expect_err("a removed pvc never binds");
        assert!(e.to_string().contains("removed before it bound"));
    }

    #[tokio::test]
    async fn a_bound_destination_pvc_ends_the_wait() {
        let (server, client) = api_server().await;
        serve(
            &server,
            DATA_PVC,
            200,
            json!({ "metadata": { "name": "data" }, "status": { "phase": "Bound" } }),
        )
        .await;
        assert!(wait_for_pvc_bound(&client, "yolab-gitea-cd34", "data", 60)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_destination_pvc_that_will_never_bind_ends_the_wait() {
        let (server, client) = api_server().await;
        serve(
            &server,
            DATA_PVC,
            200,
            json!({ "metadata": { "name": "data" }, "status": { "phase": "Lost" } }),
        )
        .await;
        let e = wait_for_pvc_bound(&client, "yolab-gitea-cd34", "data", 60)
            .await
            .expect_err("a lost pvc never binds");
        assert!(e.to_string().contains("Lost"));
    }

    async fn leftover_in(server: &wiremock::MockServer, created: &str) {
        let item = leftover_list(created)["items"][0].clone();
        serve(
            server,
            "/apis/snapshot.storage.k8s.io/v1/volumesnapshots",
            200,
            list("VolumeSnapshot", vec![item]),
        )
        .await;
    }

    #[tokio::test]
    async fn the_sweeper_clears_a_copy_whose_destination_has_bound() {
        let (server, client) = api_server().await;
        leftover_in(&server, "2026-09-25T11:00:00Z").await;
        serve(
            &server,
            DEST_NS,
            200,
            json!({ "metadata": { "name": "yolab-gitea-cd34" } }),
        )
        .await;
        serve(
            &server,
            DEST_PVC,
            200,
            json!({ "metadata": { "name": "gitea-cd34-data" }, "status": { "phase": "Bound" } }),
        )
        .await;
        deletes_accepted(&server).await;

        let tick = sweep_once(&client, at("2026-09-25T12:00:00Z"))
            .await
            .unwrap();
        assert!(matches!(tick, crate::runtime::Tick::Done));
        assert_eq!(deletions(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn the_sweeper_leaves_a_copy_that_is_still_running() {
        let (server, client) = api_server().await;
        leftover_in(&server, "2026-09-25T11:00:00Z").await;
        serve(
            &server,
            DEST_NS,
            200,
            json!({ "metadata": { "name": "yolab-gitea-cd34" } }),
        )
        .await;
        serve(
            &server,
            DEST_PVC,
            200,
            json!({ "metadata": { "name": "gitea-cd34-data" }, "status": { "phase": "Pending" } }),
        )
        .await;
        deletes_accepted(&server).await;

        let tick = sweep_once(&client, at("2026-09-25T12:00:00Z"))
            .await
            .unwrap();
        assert!(matches!(tick, crate::runtime::Tick::Idle(_)));
        assert!(
            deletions(&server).await.is_empty(),
            "a clone still being filled must not have its snapshots pulled out from under it"
        );
    }

    #[tokio::test]
    async fn a_copy_names_its_snapshot_after_the_source_and_points_the_clone_at_it() {
        let (server, client) = api_server().await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-gitea-ab12/persistentvolumeclaims/data",
            200,
            json!({ "metadata": { "name": "data" }, "spec": {
                "storageClassName": "local-path", "accessModes": ["ReadWriteOnce"],
                "resources": { "requests": { "storage": "1Gi" } }
            } }),
        )
        .await;
        let e = copy_one(
            &client,
            "yolab-gitea-ab12",
            "yolab-gitea-cd34",
            "gitea-cd34",
            "data",
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("cannot be copied directly"), "{e}");
        assert!(crate::k8s::testing::patched(&server).await.is_empty());
    }
}
