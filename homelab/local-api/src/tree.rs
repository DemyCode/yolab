use std::collections::BTreeSet;
use std::time::Duration;

use kube::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::host::Host;

pub(crate) const ROOT: &str = "/tree";
const MOUNT: &str = "/fs";
const ROOT_VOLUME: &str = "yolab-tree-root";
const CONTAINER: &str = "op";
const IMAGE: &str =
    "busybox:1.38@sha256:fd7dc98638c8e305f4dc34e979f1c0fdfdcaeb0fbf8fcff77ae834b6da3d7e6e";
const FILES_CLAIM_PREFIX: &str = "content-";
const VOLUME_SIZE: &str = "100Ti";
const KEEP_FINISHED_SECS: u64 = 600;
const DEADLINE: Duration = Duration::from_secs(6 * 3600);
const WAIT_MARGIN: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(3);
const LOG_LINES: i64 = 20;
const RENDER_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn clean_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Some(String::new());
    }
    if trimmed
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) fn folder_of(pv: &Value) -> Option<String> {
    let csi = &pv["spec"]["csi"];
    let secret = &csi["nodeStageSecretRef"];
    if csi["driver"].as_str() != Some(crate::csi::DRIVER)
        || csi["volumeAttributes"]["staticVolume"].as_str() != Some("true")
        || secret["name"].as_str() != Some(crate::csi::STATIC_SECRET)
        || secret["namespace"].as_str() != Some(crate::csi::NS)
    {
        return None;
    }
    let root_path = csi["volumeAttributes"]["rootPath"].as_str()?;
    let rest = root_path.strip_prefix(ROOT)?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    clean_path(rest)
}

pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.is_empty()
        || b.is_empty()
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn same_role(a: &str, b: &str) -> bool {
    a == b || (a.starts_with(FILES_CLAIM_PREFIX) && b.starts_with(FILES_CLAIM_PREFIX))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claimed {
    pub namespace: String,
    pub claim: String,
    pub folder: String,
}

pub(crate) fn claimed(volumes: &[Value]) -> Vec<Claimed> {
    volumes
        .iter()
        .filter(|v| v["kind"] == "PersistentVolume")
        .filter_map(|v| {
            let claim_ref = &v["spec"]["claimRef"];
            Some(Claimed {
                namespace: claim_ref["namespace"].as_str()?.to_string(),
                claim: claim_ref["name"].as_str()?.to_string(),
                folder: folder_of(v)?,
            })
        })
        .collect()
}

pub(crate) fn claimed_in(volumes: &[Value], namespace: &str) -> Vec<Claimed> {
    claimed(volumes)
        .into_iter()
        .filter(|c| c.namespace == namespace)
        .collect()
}

pub(crate) async fn volumes(client: &Client) -> anyhow::Result<Vec<Value>> {
    crate::k8s::list(client, "v1", "PersistentVolume", None, &Default::default()).await
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Planned {
    pub claimed: Claimed,
    pub volume: Value,
    pub request: Value,
}

pub(crate) fn planned(docs: &[Value], namespace: &str) -> anyhow::Result<Vec<Planned>> {
    let mut out = Vec::new();
    for volume in docs.iter().filter(|d| d["kind"] == "PersistentVolume") {
        let Some(claimed) = claimed_in(std::slice::from_ref(volume), namespace).pop() else {
            continue;
        };
        let request = docs
            .iter()
            .find(|d| {
                d["kind"] == "PersistentVolumeClaim"
                    && d["metadata"]["name"].as_str() == Some(claimed.claim.as_str())
                    && d["metadata"]["namespace"].as_str().unwrap_or(namespace) == namespace
            })
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the chart puts {} in the folder {:?} but renders no claim for it",
                    claimed.claim,
                    claimed.folder
                )
            })?;
        out.push(Planned {
            claimed,
            volume: volume.clone(),
            request,
        });
    }
    Ok(out)
}

pub(crate) async fn refuse_older_volumes(
    client: &Client,
    namespace: &str,
    planned: &[Planned],
) -> anyhow::Result<()> {
    for p in planned {
        let reference =
            crate::k8s::reference("v1", "PersistentVolumeClaim", namespace, &p.claimed.claim);
        let Some(live) = crate::k8s::get(client, &reference).await? else {
            continue;
        };
        if live["spec"]["volumeName"] != p.volume["metadata"]["name"] {
            anyhow::bail!(
                "{} keeps its data the old way, outside the file tree — uninstall this service and install it again to move it into the tree",
                p.claimed.claim
            );
        }
    }
    Ok(())
}

pub(crate) fn owned_by_release(mut object: Value, release: &str, namespace: &str) -> Value {
    object["metadata"]["labels"]["app.kubernetes.io/managed-by"] = json!("Helm");
    object["metadata"]["annotations"]["meta.helm.sh/release-name"] = json!(release);
    object["metadata"]["annotations"]["meta.helm.sh/release-namespace"] = json!(namespace);
    object
}

pub(crate) async fn render<H: Host>(
    host: &H,
    release: &str,
    chart_dir: &str,
    namespace: &str,
    values: &str,
) -> anyhow::Result<Vec<Planned>> {
    let out = host
        .run_cmd_bounded(
            "helm",
            &[
                "template",
                release,
                chart_dir,
                "--dependency-update",
                "--namespace",
                namespace,
                "--values",
                values,
            ],
            RENDER_TIMEOUT,
        )
        .await
        .map_err(|e| anyhow::anyhow!("could not read which folders {release} uses: {e}"))?;
    if !out.success {
        anyhow::bail!(
            "could not read which folders {release} uses: {}",
            out.stderr.trim()
        );
    }
    planned(&crate::k8s::documents(&out.stdout)?, namespace)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Make(Vec<String>),
    Remove {
        paths: Vec<String>,
        keep: Vec<String>,
    },
    Copy {
        from: String,
        to: String,
        leave_out: Vec<String>,
    },
}

impl Op {
    fn paths(&self) -> Vec<&str> {
        match self {
            Op::Make(paths) => paths.iter().map(String::as_str).collect(),
            Op::Remove { paths, keep } => paths.iter().chain(keep).map(String::as_str).collect(),
            Op::Copy {
                from,
                to,
                leave_out,
            } => [from.as_str(), to.as_str()]
                .into_iter()
                .chain(leave_out.iter().map(String::as_str))
                .collect(),
        }
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if let Some(bad) = self
            .paths()
            .into_iter()
            .find(|p| p.contains('\n') || clean_path(p).as_deref() != Some(*p))
        {
            return Err(format!("{bad:?} is not a folder in the file tree"));
        }
        match self {
            Op::Make(_) => Ok(()),
            Op::Remove { keep, .. } if keep.iter().any(String::is_empty) => {
                Err("keeping the whole file tree deletes nothing".to_string())
            }
            Op::Remove { .. } => Ok(()),
            Op::Copy { from, to, .. } if overlaps(from, to) => {
                Err(format!("{from:?} cannot be copied into {to:?}"))
            }
            Op::Copy { leave_out, .. } if leave_out.iter().any(String::is_empty) => {
                Err("leaving out everything copies nothing".to_string())
            }
            Op::Copy { .. } => Ok(()),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Op::Make(paths) | Op::Remove { paths, .. } => paths.is_empty(),
            Op::Copy { .. } => false,
        }
    }

    fn script(&self) -> String {
        let tree = format!("{MOUNT}{ROOT}");
        match self {
            Op::Make(_) => {
                format!("set -eu\nfor p in \"$@\"; do mkdir -p \"{tree}/$p\"; done\n")
            }
            Op::Remove { .. } => format!(
                "set -eu\n\
                 tree=\"{tree}\"\n\
                 n=\"$1\"\n\
                 shift\n\
                 doomed=\"\"\n\
                 while [ \"$n\" -gt 0 ]; do\n\
                 d=\"$tree/$1\"\n\
                 doomed=\"$doomed${{d%/}}\n\"\n\
                 shift\n\
                 n=$((n - 1))\n\
                 done\n\
                 kept=\"\"\n\
                 for k in \"$@\"; do kept=\"$kept$tree/$k\n\"; done\n\
                 verdict() {{\n\
                 local k v=delete\n\
                 while IFS= read -r k; do\n\
                 [ -n \"$k\" ] || continue\n\
                 if [ \"$1\" = \"$k\" ]; then v=keep; break; fi\n\
                 case \"$k\" in \"$1\"/*) v=descend ;; esac\n\
                 done <<KEPT\n\
                 $kept\n\
                 KEPT\n\
                 echo \"$v\"\n\
                 }}\n\
                 prune() {{\n\
                 local e\n\
                 for e in \"$1\"/* \"$1\"/.[!.]* \"$1\"/..?*; do\n\
                 [ -e \"$e\" ] || [ -L \"$e\" ] || continue\n\
                 case \"$(verdict \"$e\")\" in\n\
                 delete) rm -rf -- \"$e\" ;;\n\
                 descend) if [ -L \"$e\" ]; then rm -f -- \"$e\"; else prune \"$e\"; fi ;;\n\
                 esac\n\
                 done\n\
                 }}\n\
                 while IFS= read -r d; do\n\
                 [ -n \"$d\" ] || continue\n\
                 case \"$(verdict \"$d\")\" in\n\
                 delete) if [ \"$d\" = \"$tree\" ]; then prune \"$d\"; else rm -rf -- \"$d\"; fi ;;\n\
                 descend) prune \"$d\" ;;\n\
                 esac\n\
                 done <<DOOMED\n\
                 $doomed\n\
                 DOOMED\n"
            ),
            Op::Copy { .. } => format!(
                "set -eu\n\
                 from=\"$1\"\n\
                 to=\"$2\"\n\
                 src=\"{tree}/$1\"\n\
                 dst=\"{tree}/$2\"\n\
                 snap=\"$src/.snap/$3\"\n\
                 shift 3\n\
                 if [ ! -d \"$src\" ]; then echo \"$from is not a folder\" >&2; exit 2; fi\n\
                 if [ -d \"$dst\" ] && [ -n \"$(ls -A \"$dst\")\" ]; then\n\
                 echo \"$to already has files in it, so it is used as it is\"; exit 0\n\
                 fi\n\
                 mkdir \"$snap\"\n\
                 trap 'status=$?; rmdir \"$snap\"; exit $status' EXIT\n\
                 mkdir -p \"$dst\"\n\
                 ( set -o pipefail; tar -C \"$snap\" -cf - . | tar -C \"$dst\" -xpf - ) &\n\
                 copying=$!\n\
                 trap 'kill \"$copying\"; exit 143' TERM INT\n\
                 wait \"$copying\"\n\
                 for p in \"$@\"; do rm -rf -- \"$dst/$p\"; done\n"
            ),
        }
    }

    fn args(&self, name: &str) -> Vec<String> {
        match self {
            Op::Make(paths) => paths.clone(),
            Op::Remove { paths, keep } => std::iter::once(paths.len().to_string())
                .chain(paths.iter().cloned())
                .chain(keep.iter().cloned())
                .collect(),
            Op::Copy {
                from,
                to,
                leave_out,
            } => [from.clone(), to.clone(), name.to_string()]
                .into_iter()
                .chain(leave_out.iter().cloned())
                .collect(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Op::Make(paths) => format!("make the folders {}", paths.join(", ")),
            Op::Remove { paths, .. } => format!(
                "delete the folders {}",
                paths
                    .iter()
                    .map(|p| if p.is_empty() { "/" } else { p.as_str() })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Op::Copy { from, to, .. } => format!("copy the folder {from} to {to}"),
        }
    }
}

fn root_volume() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": { "name": ROOT_VOLUME },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "capacity": { "storage": VOLUME_SIZE },
            "persistentVolumeReclaimPolicy": "Retain",
            "storageClassName": "",
            "volumeMode": "Filesystem",
            "claimRef": { "namespace": crate::csi::NS, "name": ROOT_VOLUME },
            "csi": {
                "driver": crate::csi::DRIVER,
                "volumeHandle": ROOT_VOLUME,
                "nodeStageSecretRef": {
                    "name": crate::csi::STATIC_SECRET,
                    "namespace": crate::csi::NS,
                },
                "volumeAttributes": {
                    "clusterID": crate::csi::CLUSTER_ID,
                    "fsName": crate::cephfs::FS_NAME,
                    "staticVolume": "true",
                    "rootPath": "/",
                },
            },
        },
    })
}

fn root_claim() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": { "name": ROOT_VOLUME, "namespace": crate::csi::NS },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": "",
            "volumeName": ROOT_VOLUME,
            "resources": { "requests": { "storage": VOLUME_SIZE } },
        },
    })
}

fn job(name: &str, op: &Op) -> Value {
    let mut command = vec![
        "sh".to_string(),
        "-c".to_string(),
        op.script(),
        "tree".to_string(),
    ];
    command.extend(op.args(name));
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": name,
            "namespace": crate::csi::NS,
            "labels": { "app.kubernetes.io/managed-by": "yolab" },
        },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": DEADLINE.as_secs(),
            "ttlSecondsAfterFinished": KEEP_FINISHED_SECS,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "securityContext": { "runAsUser": 0, "runAsGroup": 0 },
                    "containers": [{
                        "name": CONTAINER,
                        "image": IMAGE,
                        "command": command,
                        "volumeMounts": [{ "name": "tree", "mountPath": MOUNT }],
                    }],
                    "volumes": [{
                        "name": "tree",
                        "persistentVolumeClaim": { "claimName": ROOT_VOLUME },
                    }],
                },
            },
        },
    })
}

enum JobState {
    Running,
    Succeeded,
    Failed(String),
}

fn job_state(job: &Value) -> JobState {
    if job["status"]["succeeded"].as_u64().unwrap_or(0) > 0 {
        return JobState::Succeeded;
    }
    let failed = job["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["type"] == "Failed" && c["status"] == "True");
    match failed {
        Some(c) => JobState::Failed(
            c["message"]
                .as_str()
                .or(c["reason"].as_str())
                .unwrap_or("it failed")
                .to_string(),
        ),
        None => JobState::Running,
    }
}

async fn last_words(client: &Client, name: &str) -> anyhow::Result<Option<String>> {
    use k8s_openapi::api::core::v1::Pod;
    let pods = crate::k8s::list(
        client,
        "v1",
        "Pod",
        Some(crate::csi::NS),
        &kube::api::ListParams::default().labels(&format!("job-name={name}")),
    )
    .await?;
    let api = kube::Api::<Pod>::namespaced(client.clone(), crate::csi::NS);
    for pod in &pods {
        let Some(pod_name) = pod["metadata"]["name"].as_str() else {
            continue;
        };
        let params = kube::api::LogParams {
            container: Some(CONTAINER.to_string()),
            tail_lines: Some(LOG_LINES),
            ..Default::default()
        };
        let text = api.logs(pod_name, &params).await?;
        if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
            return Ok(Some(line.trim().to_string()));
        }
    }
    Ok(None)
}

pub(crate) async fn run(client: &Client, op: &Op) -> anyhow::Result<()> {
    op.check().map_err(|e| anyhow::anyhow!("{e}"))?;
    if op.is_empty() {
        return Ok(());
    }
    crate::k8s::apply(client, &root_volume()).await?;
    crate::k8s::apply(client, &root_claim()).await?;
    let name = format!(
        "yolab-tree-{}",
        crate::routers::backup_common::random_hex(4)
    );
    crate::k8s::create(client, &job(&name, op)).await?;
    let reference = crate::k8s::reference("batch/v1", "Job", crate::csi::NS, &name);
    let reference = &reference;
    let job_name = name.as_str();
    let failure = crate::poll::until(DEADLINE + WAIT_MARGIN, POLL, move || async move {
        match crate::k8s::get(client, reference).await {
            Ok(Some(found)) => match job_state(&found) {
                JobState::Succeeded => crate::poll::Step::Done(None),
                JobState::Failed(why) => crate::poll::Step::Done(Some(why)),
                JobState::Running => {
                    crate::poll::Step::Pending(anyhow::anyhow!("it is still running"))
                }
            },
            Ok(None) => crate::poll::Step::Failed(anyhow::anyhow!("its job {job_name} vanished")),
            Err(e) => crate::poll::Step::Pending(e),
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("could not {}: {e}", op.describe()))?;
    let Some(why) = failure else {
        return Ok(());
    };
    let said = last_words(client, &name).await.map_err(|e| {
        anyhow::anyhow!(
            "could not {}: {why} (its log could not be read: {e})",
            op.describe()
        )
    })?;
    match said {
        Some(line) => anyhow::bail!("could not {}: {line}", op.describe()),
        None => anyhow::bail!("could not {}: {why}", op.describe()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub folder: String,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct DataVerdict {
    pub delete: Vec<String>,
    pub shared: Vec<Kept>,
    pub kept: Vec<Kept>,
}

impl DataVerdict {
    pub(crate) fn doomed(&self, with_shared: bool) -> Vec<String> {
        let shared = self
            .shared
            .iter()
            .filter(|_| with_shared)
            .map(|s| &s.folder);
        self.delete.iter().chain(shared).cloned().collect()
    }

    pub(crate) fn removal(&self, with_shared: bool) -> Op {
        Op::Remove {
            paths: self.doomed(with_shared),
            keep: self
                .kept
                .iter()
                .map(|k| k.folder.clone())
                .filter(|k| !k.is_empty())
                .collect(),
        }
    }
}

fn inside(inner: &str, outer: &str) -> bool {
    inner != outer
        && (outer.is_empty()
            || inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/')))
}

pub(crate) fn service_folders(volumes: &[Value], namespace: &str) -> Vec<String> {
    let folders: BTreeSet<String> = claimed_in(volumes, namespace)
        .into_iter()
        .map(|c| c.folder)
        .collect();
    folders
        .iter()
        .filter(|f| !folders.iter().any(|outer| inside(f, outer)))
        .cloned()
        .collect()
}

fn instance_of(namespace: &str) -> String {
    namespace
        .strip_prefix("yolab-")
        .unwrap_or(namespace)
        .to_string()
}

pub(crate) fn data_verdict(folders: &[String], volumes: &[Value], namespace: &str) -> DataVerdict {
    let others: Vec<Claimed> = claimed(volumes)
        .into_iter()
        .filter(|c| c.namespace != namespace)
        .collect();
    let users = |pick: &dyn Fn(&Claimed) -> bool| -> Vec<String> {
        others
            .iter()
            .filter(|c| pick(c))
            .map(|c| instance_of(&c.namespace))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let mut verdict = DataVerdict::default();
    let mut kept: BTreeSet<String> = BTreeSet::new();
    for folder in folders.iter().collect::<BTreeSet<_>>() {
        for c in others
            .iter()
            .filter(|c| &c.folder == folder || inside(&c.folder, folder))
        {
            kept.insert(c.folder.clone());
        }
        if kept.contains(folder) {
            continue;
        }
        let above = users(&|c| inside(folder, &c.folder));
        if above.is_empty() {
            verdict.delete.push(folder.clone());
        } else {
            verdict.shared.push(Kept {
                folder: folder.clone(),
                used_by: above,
            });
        }
    }
    verdict.kept = kept
        .into_iter()
        .map(|folder| Kept {
            used_by: users(&|c| c.folder == folder),
            folder,
        })
        .collect();
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{api_server, list, serve, serve_logs};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn volume_in(namespace: &str, claim: &str, root_path: &str) -> Value {
        json!({
            "kind": "PersistentVolume",
            "metadata": { "name": format!("{namespace}.{claim}") },
            "spec": {
                "claimRef": {"namespace": namespace, "name": claim},
                "csi": {
                    "driver": "cephfs.csi.ceph.com",
                    "volumeHandle": format!("{namespace}.{claim}"),
                    "nodeStageSecretRef": {"name": "yolab-cephfs-static", "namespace": "ceph-csi"},
                    "volumeAttributes": {
                        "clusterID": "yolab",
                        "fsName": "yolab-fs",
                        "staticVolume": "true",
                        "rootPath": root_path,
                    },
                },
            },
        })
    }

    fn volume(root_path: &str) -> Value {
        volume_in("yolab-jellyfin", "jellyfin-data", root_path)
    }

    fn request_in(namespace: &str, claim: &str) -> Value {
        json!({
            "kind": "PersistentVolumeClaim",
            "metadata": { "name": claim, "namespace": namespace },
            "spec": { "volumeName": format!("{namespace}.{claim}") },
        })
    }

    #[test]
    fn a_folder_of_the_tree_is_read_from_its_volume() {
        assert_eq!(
            folder_of(&volume("/tree/jellyfin")).as_deref(),
            Some("jellyfin")
        );
        assert_eq!(
            folder_of(&volume("/tree/movies-tv.files")).as_deref(),
            Some("movies-tv.files")
        );
    }

    #[test]
    fn the_whole_tree_is_a_folder_too() {
        assert_eq!(folder_of(&volume("/tree")).as_deref(), Some(""));
        assert_eq!(folder_of(&volume("/tree/")).as_deref(), Some(""));
    }

    #[test]
    fn a_volume_outside_the_tree_is_not_a_folder_of_it() {
        for outside in [
            "/",
            "/volumes/csi/csi-vol-1",
            "/treehouse",
            "/tree/../volumes",
            "/tree/a//b",
            "/tree/./a",
        ] {
            assert_eq!(folder_of(&volume(outside)), None, "{outside}");
        }
    }

    #[test]
    fn a_volume_mounted_with_other_credentials_is_not_a_folder_of_the_tree() {
        let mut v = volume("/tree/jellyfin");
        v["spec"]["csi"]["nodeStageSecretRef"]["name"] = json!("csi-cephfs-node");
        assert_eq!(folder_of(&v), None);
        let mut v = volume("/tree/jellyfin");
        v["spec"]["csi"]["driver"] = json!("rbd.csi.ceph.com");
        assert_eq!(folder_of(&v), None);
        let mut v = volume("/tree/jellyfin");
        v["spec"]["csi"]["volumeAttributes"]["staticVolume"] = json!("false");
        assert_eq!(folder_of(&v), None);
    }

    #[test]
    fn a_path_is_cleaned_of_its_slashes_and_refused_when_it_climbs() {
        assert_eq!(clean_path("/media/").as_deref(), Some("media"));
        assert_eq!(
            clean_path("radarr/movies").as_deref(),
            Some("radarr/movies")
        );
        assert_eq!(clean_path("").as_deref(), Some(""));
        assert_eq!(clean_path("a/../b"), None);
        assert_eq!(clean_path(".."), None);
        assert_eq!(clean_path("a//b"), None);
        assert_eq!(clean_path("./a"), None);
    }

    #[test]
    fn folders_overlap_when_one_holds_the_other() {
        assert!(overlaps("media", "media"));
        assert!(overlaps("media", "media/movies"));
        assert!(overlaps("media/movies", "media"));
        assert!(overlaps("", "media"));
        assert!(overlaps("media", ""));
        assert!(!overlaps("media", "media2"));
        assert!(!overlaps("media2", "media"));
        assert!(!overlaps("jellyfin", "jellyfin.files"));
    }

    #[test]
    fn a_files_folder_plays_the_same_part_whatever_folder_it_names() {
        assert!(same_role("jellyfin-data", "jellyfin-data"));
        assert!(same_role("content-1a2b3c4d", "content-99999999"));
        assert!(!same_role("jellyfin-data", "content-99999999"));
        assert!(!same_role("jellyfin-data", "jellyfin-cache"));
    }

    #[test]
    fn the_folders_of_a_namespace_are_read_from_the_volumes_bound_to_it() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in("yolab-jellyfin", "content-1a2b3c4d", "/tree/media"),
            volume_in("yolab-radarr", "radarr-data", "/tree/radarr"),
            volume_in("yolab-jellyfin", "old", "/volumes/csi/csi-vol-1"),
        ];
        let found = claimed_in(&volumes, "yolab-jellyfin");
        assert_eq!(
            found
                .iter()
                .map(|c| (c.claim.as_str(), c.folder.as_str()))
                .collect::<Vec<_>>(),
            vec![("jellyfin-data", "jellyfin"), ("content-1a2b3c4d", "media")]
        );
    }

    #[test]
    fn each_folder_a_chart_renders_comes_with_its_claim() {
        let docs = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            request_in("yolab-jellyfin", "jellyfin-data"),
            json!({ "kind": "Deployment", "metadata": { "name": "jellyfin" } }),
        ];
        let found = planned(&docs, "yolab-jellyfin").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].claimed.folder, "jellyfin");
        assert_eq!(found[0].request["metadata"]["name"], "jellyfin-data");
        assert_eq!(
            found[0].volume["metadata"]["name"],
            "yolab-jellyfin.jellyfin-data"
        );
    }

    #[test]
    fn a_rendered_folder_without_its_claim_is_an_error() {
        let docs = vec![volume_in(
            "yolab-jellyfin",
            "jellyfin-data",
            "/tree/jellyfin",
        )];
        let e = planned(&docs, "yolab-jellyfin").unwrap_err().to_string();
        assert!(e.contains("jellyfin-data"), "{e}");
    }

    #[test]
    fn a_rendered_claim_without_a_namespace_is_the_releases_own() {
        let mut request = request_in("yolab-jellyfin", "jellyfin-data");
        request["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("namespace");
        let docs = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            request,
        ];
        assert_eq!(planned(&docs, "yolab-jellyfin").unwrap().len(), 1);
    }

    async fn live_claim(volume_name: Option<&str>) -> Result<(), String> {
        let (server, kube) = api_server().await;
        let at = "/api/v1/namespaces/yolab-jellyfin/persistentvolumeclaims/jellyfin-data";
        match volume_name {
            Some(name) => {
                let mut live = request_in("yolab-jellyfin", "jellyfin-data");
                live["apiVersion"] = json!("v1");
                live["spec"]["volumeName"] = json!(name);
                serve(&server, at, 200, live).await
            }
            None => {
                serve(
                    &server,
                    at,
                    404,
                    crate::k8s::testing::status(404, "NotFound"),
                )
                .await
            }
        }
        let docs = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            request_in("yolab-jellyfin", "jellyfin-data"),
        ];
        let folders = planned(&docs, "yolab-jellyfin").unwrap();
        refuse_older_volumes(&kube, "yolab-jellyfin", &folders)
            .await
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn a_service_installed_before_the_tree_must_be_installed_again() {
        let e = live_claim(Some("pvc-0f1e2d")).await.unwrap_err();
        assert!(e.contains("install it again"), "{e}");
    }

    #[tokio::test]
    async fn a_service_already_in_the_tree_or_not_yet_installed_goes_ahead() {
        assert_eq!(
            live_claim(Some("yolab-jellyfin.jellyfin-data")).await,
            Ok(())
        );
        assert_eq!(live_claim(None).await, Ok(()));
    }

    #[test]
    fn what_yolab_creates_ahead_of_helm_is_adopted_by_the_release() {
        let owned = owned_by_release(
            request_in("yolab-jellyfin2", "jellyfin-data"),
            "jellyfin",
            "yolab-jellyfin2",
        );
        assert_eq!(
            owned["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "Helm"
        );
        assert_eq!(
            owned["metadata"]["annotations"]["meta.helm.sh/release-name"],
            "jellyfin"
        );
        assert_eq!(
            owned["metadata"]["annotations"]["meta.helm.sh/release-namespace"],
            "yolab-jellyfin2"
        );
    }

    fn remove(paths: &[&str], keep: &[&str]) -> Op {
        Op::Remove {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            keep: keep.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn keeping_the_whole_tree_is_refused_and_a_newline_never_reaches_a_job() {
        assert!(remove(&["jellyfin"], &[""]).check().is_err());
        assert!(remove(&["jelly\nfin"], &[]).check().is_err());
        assert!(remove(&[""], &["jellyfin"]).check().is_ok());
    }

    #[test]
    fn a_folder_is_never_copied_into_itself() {
        let copy = |from: &str, to: &str| Op::Copy {
            from: from.into(),
            to: to.into(),
            leave_out: vec![],
        };
        assert!(copy("jellyfin", "jellyfin").check().is_err());
        assert!(copy("jellyfin", "jellyfin/copy").check().is_err());
        assert!(copy("jellyfin/data", "jellyfin").check().is_err());
        assert!(copy("", "jellyfin2").check().is_err());
        assert!(copy("jellyfin", "jellyfin2").check().is_ok());
    }

    #[test]
    fn leaving_everything_out_of_a_copy_is_refused() {
        let op = Op::Copy {
            from: "jellyfin".into(),
            to: "jellyfin2".into(),
            leave_out: vec![String::new()],
        };
        assert!(op.check().is_err());
    }

    #[test]
    fn a_service_pointed_at_the_whole_tree_can_be_installed() {
        assert!(Op::Make(vec![String::new()]).check().is_ok());
    }

    #[test]
    fn only_clean_folders_reach_a_job() {
        for bad in ["/jellyfin", "jellyfin/", "a/../b", "a//b", "."] {
            assert!(Op::Make(vec![bad.into()]).check().is_err(), "{bad}");
            assert!(remove(&[bad], &[]).check().is_err(), "{bad}");
            assert!(remove(&["jellyfin"], &[bad]).check().is_err(), "{bad}");
        }
        let op = Op::Copy {
            from: "jellyfin".into(),
            to: "jellyfin2".into(),
            leave_out: vec!["../media".into()],
        };
        assert!(op.check().is_err());
    }

    #[test]
    fn the_root_mount_of_the_jobs_is_not_a_folder_of_the_tree() {
        let v = root_volume();
        assert_eq!(v["spec"]["csi"]["volumeAttributes"]["rootPath"], "/");
        assert_eq!(v["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert_eq!(v["spec"]["claimRef"]["namespace"], crate::csi::NS);
        assert_eq!(
            v["spec"]["claimRef"]["name"],
            root_claim()["metadata"]["name"]
        );
        assert_eq!(root_claim()["spec"]["volumeName"], v["metadata"]["name"]);
        assert_eq!(folder_of(&v), None);
        assert!(claimed(&[v]).is_empty());
    }

    #[test]
    fn a_job_never_hands_kubelet_a_group_to_reown_the_tree_with() {
        let j = job("yolab-tree-1", &Op::Make(vec!["jellyfin".into()]));
        let pod = &j["spec"]["template"]["spec"];
        assert!(pod["securityContext"].get("fsGroup").is_none(), "{pod}");
        assert_eq!(pod["securityContext"]["runAsUser"], 0);
        assert_eq!(pod["containers"][0]["volumeMounts"][0]["mountPath"], "/fs");
        assert_eq!(
            pod["volumes"][0]["persistentVolumeClaim"]["claimName"],
            ROOT_VOLUME
        );
    }

    #[test]
    fn a_job_runs_once_and_is_cleared_after_a_while() {
        let j = job("yolab-tree-1", &remove(&["jellyfin"], &[]));
        assert_eq!(j["spec"]["backoffLimit"], 0);
        assert_eq!(j["spec"]["template"]["spec"]["restartPolicy"], "Never");
        assert_eq!(j["spec"]["ttlSecondsAfterFinished"], KEEP_FINISHED_SECS);
        assert_eq!(j["spec"]["activeDeadlineSeconds"], DEADLINE.as_secs());
        assert_eq!(j["metadata"]["namespace"], crate::csi::NS);
    }

    #[test]
    fn making_folders_waits_as_long_as_copying_them_so_a_slow_disk_only_slows_an_install() {
        let make = job("yolab-tree-1", &Op::Make(vec!["immich".into()]));
        let copy = job(
            "yolab-tree-2",
            &Op::Copy {
                from: "immich".into(),
                to: "immich-2".into(),
                leave_out: vec![],
            },
        );
        assert_eq!(
            make["spec"]["activeDeadlineSeconds"],
            copy["spec"]["activeDeadlineSeconds"]
        );
        assert!(DEADLINE >= Duration::from_secs(3600));
    }

    #[test]
    fn folders_reach_the_script_as_arguments_never_as_script_text() {
        let j = job(
            "yolab-tree-1",
            &Op::Make(vec!["my movies".into(), "a;b".into()]),
        );
        let command: Vec<&str> = j["spec"]["template"]["spec"]["containers"][0]["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(command[0..2], ["sh", "-c"]);
        assert!(!command[2].contains("my movies"));
        assert_eq!(command[3..], ["tree", "my movies", "a;b"]);
        assert!(command[2].contains("mkdir -p \"/fs/tree/$p\""));
    }

    #[test]
    fn a_copy_reads_a_snapshot_named_after_its_job_and_leaves_out_what_it_was_told() {
        let op = Op::Copy {
            from: "jellyfin".into(),
            to: "jellyfin2".into(),
            leave_out: vec!["media".into()],
        };
        let j = job("yolab-tree-1", &op);
        let command = &j["spec"]["template"]["spec"]["containers"][0]["command"];
        assert_eq!(command[3], "tree");
        assert_eq!(command[4], "jellyfin");
        assert_eq!(command[5], "jellyfin2");
        assert_eq!(command[6], "yolab-tree-1");
        assert_eq!(command[7], "media");
        let script = command[2].as_str().unwrap();
        assert!(script.contains("snap=\"$src/.snap/$3\""), "{script}");
        assert!(script.contains("rmdir \"$snap\""), "{script}");
        assert!(
            script.contains("already has files in it, so it is used as it is"),
            "{script}"
        );
        assert!(script.contains("rm -rf -- \"$dst/$p\""), "{script}");
    }

    fn verdict(volumes: &[Value], namespace: &str) -> DataVerdict {
        data_verdict(&service_folders(volumes, namespace), volumes, namespace)
    }

    fn kept(folder: &str, used_by: &[&str]) -> Kept {
        Kept {
            folder: folder.into(),
            used_by: used_by.iter().map(|u| u.to_string()).collect(),
        }
    }

    #[test]
    fn a_services_folders_are_listed_once_with_the_ones_inside_folded_in() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in("yolab-jellyfin", "jellyfin-cache", "/tree/jellyfin"),
            volume_in("yolab-jellyfin", "content-1a2b3c4d", "/tree/jellyfin/media"),
            volume_in("yolab-jellyfin", "content-99999999", "/tree/Movies"),
            volume_in("yolab-radarr", "radarr-data", "/tree/radarr"),
        ];
        assert_eq!(
            service_folders(&volumes, "yolab-jellyfin"),
            vec!["Movies", "jellyfin"]
        );
    }

    #[test]
    fn a_service_at_the_root_has_the_whole_tree_as_its_one_folder() {
        let volumes = vec![
            volume_in("yolab-filebrowser", "filebrowser-data", "/tree/filebrowser"),
            volume_in("yolab-filebrowser", "content-1a2b3c4d", "/tree"),
        ];
        assert_eq!(service_folders(&volumes, "yolab-filebrowser"), vec![""]);
    }

    #[test]
    fn data_nobody_else_sees_is_deleted_without_a_question() {
        let volumes = vec![volume_in(
            "yolab-jellyfin",
            "jellyfin-data",
            "/tree/jellyfin",
        )];
        assert_eq!(
            verdict(&volumes, "yolab-jellyfin"),
            DataVerdict {
                delete: vec!["jellyfin".into()],
                ..Default::default()
            }
        );
    }

    #[test]
    fn data_another_service_sees_from_above_is_asked_about() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in("yolab-filebrowser", "content-1a2b3c4d", "/tree"),
        ];
        assert_eq!(
            verdict(&volumes, "yolab-jellyfin"),
            DataVerdict {
                shared: vec![kept("jellyfin", &["filebrowser"])],
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_folder_another_service_mounts_is_never_deleted() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in("yolab-jellyfin", "content-1a2b3c4d", "/tree/Movies"),
            volume_in("yolab-radarr", "content-1a2b3c4d", "/tree/Movies"),
        ];
        assert_eq!(
            verdict(&volumes, "yolab-jellyfin"),
            DataVerdict {
                delete: vec!["jellyfin".into()],
                kept: vec![kept("Movies", &["radarr"])],
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_folder_another_service_mounts_inside_survives_the_rest_being_deleted() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in(
                "yolab-radarr",
                "content-1a2b3c4d",
                "/tree/jellyfin/media/movies",
            ),
        ];
        let verdict = verdict(&volumes, "yolab-jellyfin");
        assert_eq!(
            verdict.kept,
            vec![kept("jellyfin/media/movies", &["radarr"])]
        );
        assert_eq!(
            verdict.removal(false),
            Op::Remove {
                paths: vec!["jellyfin".into()],
                keep: vec!["jellyfin/media/movies".into()],
            }
        );
    }

    #[test]
    fn shared_data_is_deleted_only_when_asked() {
        let volumes = vec![
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
            volume_in("yolab-filebrowser", "content-1a2b3c4d", "/tree"),
        ];
        let verdict = verdict(&volumes, "yolab-jellyfin");
        assert!(verdict.removal(false).is_empty());
        assert_eq!(
            verdict.removal(true),
            Op::Remove {
                paths: vec!["jellyfin".into()],
                keep: vec![],
            }
        );
    }

    #[test]
    fn a_service_at_the_root_takes_everything_only_it_sees() {
        let volumes = vec![
            volume_in("yolab-filebrowser", "content-1a2b3c4d", "/tree"),
            volume_in("yolab-jellyfin", "jellyfin-data", "/tree/jellyfin"),
        ];
        let verdict = verdict(&volumes, "yolab-filebrowser");
        assert_eq!(verdict.delete, vec![""]);
        assert_eq!(verdict.kept, vec![kept("jellyfin", &["jellyfin"])]);
    }

    #[test]
    fn two_services_at_the_root_never_delete_the_tree() {
        let volumes = vec![
            volume_in("yolab-filebrowser", "content-1a2b3c4d", "/tree"),
            volume_in("yolab-filebrowser2", "content-1a2b3c4d", "/tree"),
        ];
        let verdict = verdict(&volumes, "yolab-filebrowser");
        assert_eq!(verdict.kept, vec![kept("", &["filebrowser2"])]);
        assert!(verdict.removal(true).is_empty());
    }

    #[test]
    fn folders_remembered_at_uninstall_are_judged_against_what_is_mounted_now() {
        let volumes = vec![volume_in("yolab-radarr", "content-1a2b3c4d", "/tree/media")];
        let folders = vec!["jellyfin".to_string(), "media".to_string()];
        let verdict = data_verdict(&folders, &volumes, "yolab-jellyfin");
        assert_eq!(verdict.delete, vec!["jellyfin"]);
        assert_eq!(verdict.kept, vec![kept("media", &["radarr"])]);
    }

    fn remove_in(root: &std::path::Path, paths: &[&str], keep: &[&str]) {
        let op = Op::Remove {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            keep: keep.iter().map(|k| k.to_string()).collect(),
        };
        op.check().unwrap();
        let script = op
            .script()
            .replace(&format!("{MOUNT}{ROOT}"), root.to_str().unwrap());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("tree")
            .args(op.args("yolab-tree-1"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn tree_with(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        dir
    }

    #[test]
    fn removing_a_folder_keeps_only_what_another_service_mounts_inside_it() {
        let dir = tree_with(&[
            "jellyfin/config/db",
            "jellyfin/.hidden",
            "jellyfin/media/movies/film",
            "jellyfin/media/shows/episode",
            "other/file",
        ]);
        remove_in(dir.path(), &["jellyfin"], &["jellyfin/media/movies"]);
        let root = dir.path();
        assert!(root.join("jellyfin/media/movies/film").exists());
        assert!(!root.join("jellyfin/media/shows").exists());
        assert!(!root.join("jellyfin/config").exists());
        assert!(!root.join("jellyfin/.hidden").exists());
        assert!(root.join("other/file").exists());
    }

    #[test]
    fn removing_the_root_empties_the_tree_but_what_others_mount() {
        let dir = tree_with(&["test/file", "jellyfin/config/db", "filebrowser/state"]);
        remove_in(dir.path(), &[""], &["jellyfin"]);
        let root = dir.path();
        assert!(root.exists());
        assert!(root.join("jellyfin/config/db").exists());
        assert!(!root.join("test").exists());
        assert!(!root.join("filebrowser").exists());
    }

    #[test]
    fn removing_folders_with_spaces_and_none_left_inside_deletes_them_whole() {
        let dir = tree_with(&["my movies/film", "notes/a"]);
        remove_in(dir.path(), &["my movies", "notes"], &[]);
        assert!(!dir.path().join("my movies").exists());
        assert!(!dir.path().join("notes").exists());
    }

    fn job_at(server_status: Value) -> Value {
        json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": { "name": "yolab-tree-x", "namespace": "ceph-csi" },
            "status": server_status,
        })
    }

    async fn accepting(server: &wiremock::MockServer, job_status: Value) {
        crate::k8s::testing::accept_patches(server).await;
        Mock::given(method("POST"))
            .and(path("/apis/batch/v1/namespaces/ceph-csi/jobs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(job_at(json!({}))))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(
                "^/apis/batch/v1/namespaces/ceph-csi/jobs/yolab-tree-[0-9a-f]+$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(job_at(job_status)))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn a_job_that_succeeds_is_the_op_done() {
        let (server, kube) = api_server().await;
        accepting(&server, json!({ "succeeded": 1 })).await;
        run(&kube, &Op::Make(vec!["jellyfin".into()]))
            .await
            .unwrap();
        let patched = crate::k8s::testing::patched(&server).await;
        assert!(patched
            .iter()
            .any(|p| p["metadata"]["name"] == ROOT_VOLUME && p["kind"] == "PersistentVolume"));
        assert!(patched
            .iter()
            .any(|p| p["metadata"]["name"] == ROOT_VOLUME && p["kind"] == "PersistentVolumeClaim"));
    }

    #[tokio::test]
    async fn a_job_that_fails_says_what_its_script_said() {
        let (server, kube) = api_server().await;
        accepting(
            &server,
            json!({ "conditions": [
                { "type": "Failed", "status": "True", "reason": "BackoffLimitExceeded" }
            ] }),
        )
        .await;
        serve(
            &server,
            "/api/v1/namespaces/ceph-csi/pods",
            200,
            list(
                "Pod",
                vec![json!({ "metadata": { "name": "yolab-tree-x-abcde" } })],
            ),
        )
        .await;
        serve_logs(
            &server,
            "/api/v1/namespaces/ceph-csi/pods/yolab-tree-x-abcde",
            CONTAINER,
            "jellyfin2 already has files in it\n",
        )
        .await;
        let op = Op::Copy {
            from: "jellyfin".into(),
            to: "jellyfin2".into(),
            leave_out: vec![],
        };
        let e = run(&kube, &op).await.unwrap_err().to_string();
        assert!(e.contains("copy the folder jellyfin to jellyfin2"), "{e}");
        assert!(e.contains("already has files in it"), "{e}");
    }

    #[tokio::test]
    async fn a_job_that_fails_without_a_word_says_why_kubernetes_stopped_it() {
        let (server, kube) = api_server().await;
        accepting(
            &server,
            json!({ "conditions": [{
                "type": "Failed", "status": "True", "reason": "DeadlineExceeded",
                "message": "Job was active longer than specified deadline",
            }] }),
        )
        .await;
        serve(
            &server,
            "/api/v1/namespaces/ceph-csi/pods",
            200,
            list("Pod", vec![]),
        )
        .await;
        let e = run(&kube, &remove(&["jellyfin"], &[]))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("delete the folders jellyfin"), "{e}");
        assert!(e.contains("longer than specified deadline"), "{e}");
    }

    #[tokio::test]
    async fn nothing_to_make_starts_no_job() {
        let (server, kube) = api_server().await;
        run(&kube, &Op::Make(vec![])).await.unwrap();
        assert!(server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty());
    }

    #[tokio::test]
    async fn a_refused_op_starts_no_job() {
        let (server, kube) = api_server().await;
        assert!(run(&kube, &remove(&["jellyfin"], &[""])).await.is_err());
        assert!(server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty());
    }

    #[tokio::test]
    async fn the_folders_a_chart_uses_are_read_from_its_render() {
        use crate::host::fake::FakeHost;
        let rendered = format!(
            "{}\n---\n{}\n",
            serde_json::to_string(&volume_in(
                "yolab-jellyfin",
                "jellyfin-data",
                "/tree/jellyfin"
            ))
            .unwrap(),
            serde_json::to_string(&request_in("yolab-jellyfin", "jellyfin-data")).unwrap(),
        );
        let host = FakeHost::new().ok("helm template", &rendered);
        let found = render(
            &host,
            "jellyfin",
            "/charts/jellyfin",
            "yolab-jellyfin",
            "/v.yaml",
        )
        .await
        .unwrap();
        assert_eq!(found[0].claimed.folder, "jellyfin");
        assert!(host.ran(
            "helm template jellyfin /charts/jellyfin --dependency-update --namespace yolab-jellyfin --values /v.yaml"
        ));
    }

    #[tokio::test]
    async fn a_chart_that_cannot_render_stops_the_install() {
        use crate::host::fake::FakeHost;
        let host = FakeHost::new().fail("helm template", "\"//\" is not a folder in the file tree");
        let e = render(&host, "jellyfin", "/c", "yolab-jellyfin", "/v.yaml")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a folder in the file tree"), "{e}");
    }
}
