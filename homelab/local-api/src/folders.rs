use std::collections::BTreeSet;

use kube::api::ListParams;
use kube::Client;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::appschema::AppSchema;

pub(crate) const NAMESPACE: &str = "yolab-folders";
pub(crate) const STATIC_SECRET: &str = "yolab-cephfs-static";
pub(crate) const LABEL_FOLDER: &str = "yolab.io/folder";
const LABEL_APP_NAMESPACE: &str = "yolab.io/folder-namespace";
const LABEL_KIND: &str = "yolab.io/folder-kind";
pub(crate) const KIND_FILES: &str = "files";
pub(crate) const KIND_STATE: &str = "state";
pub(crate) const LABEL_STATE: &str = "yolab.io/state-folder";
pub(crate) const LABEL_STATE_NAMESPACE: &str = "yolab.io/state-namespace";
const LABEL_STATE_ORIGIN: &str = "yolab.io/state-origin";
const ANN_TITLE: &str = "yolab.io/folder-title";
pub(crate) const ANN_STATE_APP: &str = "yolab.io/state-app";
pub(crate) const ANN_STATE_RELEASE: &str = "yolab.io/state-release";
pub(crate) const ANN_STATE_CLAIM: &str = "yolab.io/state-claim";
const FORMAT: &str = "folder";
const DRIVER: &str = "rook-ceph.cephfs.csi.ceph.com";
const CSI_NAMESPACE: &str = "rook-ceph";
const MAX_NAME: usize = 40;
pub(crate) const NEW_FOLDER_SIZE: &str = "1Ti";

#[derive(Serialize, Debug, PartialEq)]
pub struct Folder {
    pub name: String,
    pub title: String,
    pub size: String,
    pub ready: bool,
    pub used_by: Vec<String>,
    pub kind: String,
    pub app: Option<String>,
}

fn kind_of(claim: &Value) -> &str {
    claim["metadata"]["labels"][LABEL_KIND]
        .as_str()
        .unwrap_or(KIND_FILES)
}

pub(crate) fn name_from_title(title: &str) -> Option<String> {
    let mut name = String::new();
    for c in title.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name: String = name.chars().take(MAX_NAME).collect();
    let name = name.trim_end_matches('-').to_string();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn free_name(wanted: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            let stem: String = wanted.chars().take(MAX_NAME - suffix.len()).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded range always has a free name")
}

pub(crate) fn claim_name(folder: &str) -> String {
    format!("folder-{folder}")
}

pub(crate) fn app_volume_name(namespace: &str, folder: &str) -> String {
    format!("{namespace}.folder-{folder}")
}

pub(crate) fn is_mount(
    namespace: &str,
    labels: Option<&std::collections::BTreeMap<String, String>>,
    volume_name: Option<&str>,
) -> bool {
    labels.is_some_and(|l| l.contains_key(LABEL_FOLDER))
        || volume_name.is_some_and(|v| v.starts_with(&app_volume_name(namespace, "")))
}

pub(crate) fn files_claim(folder: &str, title: &str, storage: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim_name(folder),
            "namespace": NAMESPACE,
            "labels": { LABEL_FOLDER: folder, LABEL_KIND: KIND_FILES },
            "annotations": { ANN_TITLE: title },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": crate::routers::copy::CEPHFS_STORAGE_CLASS,
            "resources": { "requests": { "storage": storage } },
        },
    })
}

pub(crate) struct Source {
    root_path: String,
    capacity: String,
}

fn source_of(owner_volume: &Value) -> Option<Source> {
    let attributes = &owner_volume["spec"]["csi"]["volumeAttributes"];
    let root_path = attributes["subvolumePath"]
        .as_str()
        .or_else(|| attributes["rootPath"].as_str())
        .filter(|p| p.starts_with('/'))?;
    Some(Source {
        root_path: root_path.to_string(),
        capacity: owner_volume["spec"]["capacity"]["storage"]
            .as_str()?
            .to_string(),
    })
}

pub(crate) fn app_volume(folder: &str, namespace: &str, source: &Source) -> Value {
    let name = app_volume_name(namespace, folder);
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {
            "name": name,
            "labels": { LABEL_FOLDER: folder, LABEL_APP_NAMESPACE: namespace },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "capacity": { "storage": source.capacity },
            "persistentVolumeReclaimPolicy": "Retain",
            "storageClassName": "",
            "volumeMode": "Filesystem",
            "claimRef": { "namespace": namespace, "name": claim_name(folder) },
            "csi": {
                "driver": DRIVER,
                "volumeHandle": name,
                "nodeStageSecretRef": { "name": STATIC_SECRET, "namespace": CSI_NAMESPACE },
                "volumeAttributes": {
                    "clusterID": CSI_NAMESPACE,
                    "fsName": crate::cephfs::FS_NAME,
                    "staticVolume": "true",
                    "rootPath": source.root_path,
                },
            },
        },
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateOwner {
    pub app: String,
    pub release: String,
    pub claim: String,
}

pub(crate) fn state_owner(
    claim: &str,
    app_id: &str,
    release: &str,
    members: &[String],
) -> StateOwner {
    let member = claim
        .strip_prefix(release)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| {
            members
                .iter()
                .filter(|m| rest.starts_with(&format!("{m}-")))
                .max_by_key(|m| m.len())
        });
    match member {
        Some(member) => StateOwner {
            app: member.clone(),
            release: format!("{release}-{member}"),
            claim: claim.to_string(),
        },
        None => StateOwner {
            app: app_id.to_string(),
            release: release.to_string(),
            claim: claim.to_string(),
        },
    }
}

pub(crate) fn state_folder_name(instance: &str, claim: &str) -> String {
    let joined = if claim.starts_with(instance) {
        claim.to_string()
    } else {
        format!("{instance}-{claim}")
    };
    name_from_title(&joined).unwrap_or_else(|| "app-data".to_string())
}

pub(crate) fn owner_volume_name(folder: &str) -> String {
    format!("{NAMESPACE}.state-{folder}")
}

pub(crate) fn state_owner_volume(folder: &str, driver: &str, source: &Source) -> Value {
    let name = owner_volume_name(folder);
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {
            "name": name,
            "labels": { LABEL_FOLDER: folder },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "capacity": { "storage": source.capacity },
            "persistentVolumeReclaimPolicy": "Retain",
            "storageClassName": "",
            "volumeMode": "Filesystem",
            "claimRef": { "namespace": NAMESPACE, "name": claim_name(folder) },
            "csi": {
                "driver": driver,
                "volumeHandle": name,
                "nodeStageSecretRef": { "name": STATIC_SECRET, "namespace": CSI_NAMESPACE },
                "volumeAttributes": {
                    "clusterID": CSI_NAMESPACE,
                    "fsName": crate::cephfs::FS_NAME,
                    "staticVolume": "true",
                    "rootPath": source.root_path,
                },
            },
        },
    })
}

pub(crate) fn state_owner_claim(
    folder: &str,
    title: &str,
    owner: &StateOwner,
    capacity: &str,
) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim_name(folder),
            "namespace": NAMESPACE,
            "labels": { LABEL_FOLDER: folder, LABEL_KIND: KIND_STATE },
            "annotations": {
                ANN_TITLE: title,
                ANN_STATE_APP: owner.app,
                ANN_STATE_RELEASE: owner.release,
                ANN_STATE_CLAIM: owner.claim,
            },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": "",
            "volumeName": owner_volume_name(folder),
            "resources": { "requests": { "storage": capacity } },
        },
    })
}

pub(crate) fn kept_for(volume: &str, folder: &str, namespace: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {
            "name": volume,
            "labels": {
                LABEL_STATE: folder,
                LABEL_STATE_NAMESPACE: namespace,
                LABEL_STATE_ORIGIN: "true",
            },
        },
        "spec": { "persistentVolumeReclaimPolicy": "Retain" },
    })
}

pub(crate) fn adoptable(claim: &Value, volume: &Value) -> bool {
    let namespace = claim["metadata"]["namespace"].as_str().unwrap_or_default();
    let name = claim["metadata"]["name"].as_str().unwrap_or_default();
    let labels: Option<std::collections::BTreeMap<String, String>> =
        serde_json::from_value(claim["metadata"]["labels"].clone()).ok();
    claim["status"]["phase"].as_str() == Some("Bound")
        && !name.is_empty()
        && !name.starts_with("volsync-")
        && !is_mount(
            namespace,
            labels.as_ref(),
            claim["spec"]["volumeName"].as_str(),
        )
        && volume["spec"]["csi"]["driver"]
            .as_str()
            .is_some_and(|d| d.contains("cephfs"))
        && source_of(volume).is_some()
}

pub(crate) fn reuse_volume(
    app_ns: &str,
    claim: &str,
    folder: &str,
    driver: &str,
    source: &Source,
) -> Value {
    let name = format!("{app_ns}.{claim}");
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": {
            "name": name,
            "labels": { LABEL_STATE: folder, LABEL_STATE_NAMESPACE: app_ns },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "capacity": { "storage": source.capacity },
            "persistentVolumeReclaimPolicy": "Retain",
            "storageClassName": crate::routers::copy::CEPHFS_STORAGE_CLASS,
            "volumeMode": "Filesystem",
            "claimRef": { "namespace": app_ns, "name": claim },
            "csi": {
                "driver": driver,
                "volumeHandle": name,
                "nodeStageSecretRef": { "name": STATIC_SECRET, "namespace": CSI_NAMESPACE },
                "volumeAttributes": {
                    "clusterID": CSI_NAMESPACE,
                    "fsName": crate::cephfs::FS_NAME,
                    "staticVolume": "true",
                    "rootPath": source.root_path,
                },
            },
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeptData {
    pub folder: String,
    pub claim: String,
    pub volume: String,
}

pub(crate) fn kept_data_of(
    claims: &[Value],
    volumes: &[Value],
    release: &str,
    members: &[String],
) -> Vec<KeptData> {
    let mut releases: BTreeSet<String> = members.iter().map(|m| format!("{release}-{m}")).collect();
    releases.insert(release.to_string());
    claims
        .iter()
        .filter_map(|claim| {
            let labels = &claim["metadata"]["labels"];
            if labels[LABEL_KIND].as_str() != Some(KIND_STATE) {
                return None;
            }
            let ann = &claim["metadata"]["annotations"];
            if !ann[ANN_STATE_RELEASE]
                .as_str()
                .is_some_and(|r| releases.contains(r))
            {
                return None;
            }
            let folder = labels[LABEL_FOLDER].as_str()?;
            if !users(volumes, folder).is_empty() {
                return None;
            }
            Some(KeptData {
                folder: folder.to_string(),
                claim: ann[ANN_STATE_CLAIM]
                    .as_str()
                    .filter(|c| !c.is_empty())?
                    .to_string(),
                volume: claim["spec"]["volumeName"].as_str()?.to_string(),
            })
        })
        .collect()
}

pub(crate) async fn kept_data(
    client: &Client,
    release: &str,
    members: &[String],
) -> anyhow::Result<Vec<KeptData>> {
    let (claims, volumes) = tokio::try_join!(owner_claims(client), folder_volumes(client))?;
    Ok(kept_data_of(&claims, &volumes, release, members))
}

pub(crate) async fn reuse_data_folders(
    client: &Client,
    app_ns: &str,
    release: &str,
    members: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut reused = Vec::new();
    for kept in kept_data(client, release, members).await? {
        let (folder, pvc, volume_name) = (&kept.folder, &kept.claim, &kept.volume);
        let Some(volume) = crate::k8s::get(
            client,
            &crate::k8s::cluster_reference("v1", "PersistentVolume", volume_name),
        )
        .await?
        else {
            continue;
        };
        let Some(source) = source_of(&volume) else {
            continue;
        };
        let driver = volume["spec"]["csi"]["driver"].as_str().unwrap_or(DRIVER);
        crate::k8s::apply(client, &reuse_volume(app_ns, pvc, folder, driver, &source)).await?;
        reused.push(folder.to_string());
    }
    Ok(reused)
}

pub(crate) fn state_folder_of(volume: &Value) -> Option<&str> {
    volume["metadata"]["labels"][LABEL_STATE]
        .as_str()
        .filter(|f| !f.is_empty())
}

pub(crate) struct AppVolumes<'a> {
    pub namespace: &'a str,
    pub app_id: &'a str,
    pub release: &'a str,
    pub members: &'a [String],
}

async fn ensure_state_owner(
    client: &Client,
    folder: &str,
    volume: &Value,
    owner: &StateOwner,
) -> anyhow::Result<()> {
    let source = source_of(volume)
        .ok_or_else(|| anyhow::anyhow!("{folder}: could not read where its files live"))?;
    let driver = volume["spec"]["csi"]["driver"].as_str().unwrap_or(DRIVER);
    let owner_volume = state_owner_volume(folder, driver, &source);
    if !crate::k8s::exists(client, &owner_volume).await? {
        crate::k8s::apply(client, &owner_volume).await?;
    }
    let owner_claim = state_owner_claim(folder, folder, owner, &source.capacity);
    if !crate::k8s::exists(client, &owner_claim).await? {
        crate::k8s::apply(client, &owner_claim).await?;
    }
    Ok(())
}

pub(crate) async fn adopt(client: &Client, app: &AppVolumes<'_>) -> anyhow::Result<Vec<String>> {
    crate::k8s::apply(
        client,
        &json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": NAMESPACE } }),
    )
    .await?;
    let claims = crate::k8s::list(
        client,
        "v1",
        "PersistentVolumeClaim",
        Some(app.namespace),
        &ListParams::default(),
    )
    .await?;
    let mut taken: BTreeSet<String> = owner_claims(client)
        .await?
        .iter()
        .filter_map(|c| {
            c["metadata"]["labels"][LABEL_FOLDER]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    let instance = app
        .namespace
        .strip_prefix("yolab-")
        .unwrap_or(app.namespace);
    let mut adopted = Vec::new();
    for claim in &claims {
        let Some(volume_name) = claim["spec"]["volumeName"]
            .as_str()
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(volume) = crate::k8s::get(
            client,
            &crate::k8s::cluster_reference("v1", "PersistentVolume", volume_name),
        )
        .await?
        else {
            continue;
        };
        let claim_name = claim["metadata"]["name"].as_str().unwrap_or_default();
        let owner = state_owner(claim_name, app.app_id, app.release, app.members);
        if let Some(folder) = state_folder_of(&volume) {
            if volume["metadata"]["labels"][LABEL_STATE_ORIGIN].as_str() == Some("true") {
                ensure_state_owner(client, folder, &volume, &owner).await?;
            }
            continue;
        }
        if !adoptable(claim, &volume) {
            continue;
        }
        let folder = free_name(&state_folder_name(instance, claim_name), &taken);
        taken.insert(folder.clone());
        crate::k8s::merge_patch(client, &kept_for(volume_name, &folder, app.namespace)).await?;
        ensure_state_owner(client, &folder, &volume, &owner).await?;
        adopted.push(folder);
    }
    Ok(adopted)
}

pub(crate) async fn forget_mounts_of(client: &Client, namespace: &str) -> anyhow::Result<()> {
    for volume in folder_volumes(client).await? {
        let labels = &volume["metadata"]["labels"];
        if labels[LABEL_STATE_NAMESPACE].as_str() != Some(namespace)
            || labels[LABEL_STATE_ORIGIN].as_str() == Some("true")
            || volume["status"]["phase"].as_str() == Some("Bound")
        {
            continue;
        }
        if let Some(name) = volume["metadata"]["name"].as_str() {
            crate::k8s::delete_if_present(
                client,
                &crate::k8s::cluster_reference("v1", "PersistentVolume", name),
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) fn folder_fields(config_schema: &Value) -> BTreeSet<String> {
    fn walk(node: &Value, found: &mut BTreeSet<String>) {
        match node {
            Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, spec) in props {
                        if spec["format"].as_str() == Some(FORMAT) {
                            found.insert(name.clone());
                        }
                    }
                }
                for child in map.values() {
                    walk(child, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, found)),
            _ => {}
        }
    }
    let mut found = BTreeSet::new();
    walk(config_schema, &mut found);
    found
}

pub(crate) fn wanted(app: &AppSchema, config: &Map<String, Value>) -> BTreeSet<String> {
    let settings = app.with_defaults(config);
    folder_fields(&app.config())
        .iter()
        .filter_map(|field| settings.get(field)?.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) async fn attach(
    client: &Client,
    namespace: &str,
    folders: &BTreeSet<String>,
) -> anyhow::Result<()> {
    for folder in folders {
        anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
        let source = source(client, folder).await?;
        crate::k8s::apply(client, &app_volume(folder, namespace, &source)).await?;
    }
    Ok(())
}

async fn source(client: &Client, folder: &str) -> anyhow::Result<Source> {
    let claim = crate::k8s::get(
        client,
        &crate::k8s::reference(
            "v1",
            "PersistentVolumeClaim",
            NAMESPACE,
            &claim_name(folder),
        ),
    )
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "the folder {folder} does not exist — pick another one in the app's settings"
        )
    })?;
    let volume = claim["spec"]["volumeName"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("the folder {folder} is still being created — try again in a minute")
        })?;
    let owner = crate::k8s::get(
        client,
        &crate::k8s::cluster_reference("v1", "PersistentVolume", volume),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("the folder {folder} has lost its storage ({volume})"))?;
    source_of(&owner)
        .ok_or_else(|| anyhow::anyhow!("the folder {folder}: could not read where its files live"))
}

fn attached(volumes: &[Value], namespace: &str) -> Vec<(String, String)> {
    volumes
        .iter()
        .filter(|pv| pv["metadata"]["labels"][LABEL_APP_NAMESPACE].as_str() == Some(namespace))
        .filter_map(|pv| {
            Some((
                pv["metadata"]["name"].as_str()?.to_string(),
                pv["metadata"]["labels"][LABEL_FOLDER].as_str()?.to_string(),
            ))
        })
        .collect()
}

async fn folder_volumes(client: &Client) -> anyhow::Result<Vec<Value>> {
    crate::k8s::list(
        client,
        "v1",
        "PersistentVolume",
        None,
        &ListParams::default(),
    )
    .await
}

pub(crate) async fn release_unused(
    client: &Client,
    namespace: &str,
    keep: &BTreeSet<String>,
) -> anyhow::Result<()> {
    for (volume, folder) in attached(&folder_volumes(client).await?, namespace) {
        if !keep.contains(&folder) {
            crate::k8s::delete_if_present(
                client,
                &crate::k8s::cluster_reference("v1", "PersistentVolume", &volume),
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn release_all(client: &Client, namespace: &str) -> anyhow::Result<()> {
    release_unused(client, namespace, &BTreeSet::new()).await
}

fn user_of<'a>(pv: &'a Value, folder: &str) -> Option<&'a str> {
    let labels = &pv["metadata"]["labels"];
    if labels[LABEL_FOLDER].as_str() == Some(folder) {
        return labels[LABEL_APP_NAMESPACE].as_str();
    }
    if labels[LABEL_STATE].as_str() == Some(folder)
        && pv["status"]["phase"].as_str() == Some("Bound")
    {
        return labels[LABEL_STATE_NAMESPACE].as_str();
    }
    None
}

fn users(volumes: &[Value], folder: &str) -> Vec<String> {
    let mut users: Vec<String> = volumes
        .iter()
        .filter_map(|pv| user_of(pv, folder))
        .map(|ns| ns.strip_prefix("yolab-").unwrap_or(ns).to_string())
        .collect();
    users.sort();
    users.dedup();
    users
}

fn folder_of(claim: &Value, volumes: &[Value]) -> Option<Folder> {
    let name = claim["metadata"]["labels"][LABEL_FOLDER]
        .as_str()?
        .to_string();
    Some(Folder {
        title: claim["metadata"]["annotations"][ANN_TITLE]
            .as_str()
            .unwrap_or(&name)
            .to_string(),
        size: claim["spec"]["resources"]["requests"]["storage"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        ready: claim["status"]["phase"].as_str() == Some("Bound"),
        used_by: users(volumes, &name),
        kind: kind_of(claim).to_string(),
        app: claim["metadata"]["annotations"][ANN_STATE_APP]
            .as_str()
            .map(str::to_string),
        name,
    })
}

fn instance_of(namespace: &str) -> String {
    namespace
        .strip_prefix("yolab-")
        .unwrap_or(namespace)
        .to_string()
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Kept {
    pub title: String,
    pub used_by: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct OnRemoval {
    pub deleted: Vec<String>,
    pub kept: Vec<Kept>,
    #[serde(skip)]
    delete: Vec<String>,
}

pub(crate) fn on_removal(claims: &[Value], volumes: &[Value], namespace: &str) -> OnRemoval {
    let me = instance_of(namespace);
    let mut plan = OnRemoval::default();
    for folder in claims.iter().filter_map(|c| folder_of(c, volumes)) {
        if !folder.used_by.contains(&me) {
            continue;
        }
        let mut others = folder.used_by;
        others.retain(|u| u != &me);
        if others.is_empty() {
            plan.deleted.push(folder.title);
            plan.delete.push(folder.name);
        } else {
            plan.kept.push(Kept {
                title: folder.title,
                used_by: others,
            });
        }
    }
    plan
}

pub(crate) async fn doomed_spaces(client: &Client, namespace: &str) -> anyhow::Result<Vec<String>> {
    let (claims, volumes) = tokio::try_join!(owner_claims(client), folder_volumes(client))?;
    let plan = on_removal(&claims, &volumes, namespace);
    tracing::info!(
        "{namespace}: on removal, deleting the spaces {:?} and keeping {:?}",
        plan.deleted,
        plan.kept
    );
    Ok(plan.delete)
}

pub(crate) async fn let_go(
    client: &Client,
    namespace: &str,
    doomed: &[String],
) -> anyhow::Result<()> {
    let (_claims, volumes) = tokio::try_join!(owner_claims(client), folder_volumes(client))?;
    release_all(client, namespace).await?;
    for folder in doomed {
        delete_folder(client, folder, &volumes).await?;
    }
    forget_mounts_of(client, namespace).await
}

pub(crate) async fn owner_claim_of(client: &Client, folder: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    crate::k8s::get(
        client,
        &crate::k8s::reference(
            "v1",
            "PersistentVolumeClaim",
            NAMESPACE,
            &claim_name(folder),
        ),
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("the folder {folder} does not exist"))
}

pub(crate) async fn create_named(
    client: &Client,
    folder: &str,
    title: &str,
    storage: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    ensure_namespace(client).await?;
    crate::k8s::create(client, &files_claim(folder, title, storage)).await?;
    Ok(())
}

pub(crate) async fn wait_until_ready(client: &Client, folder: &str) -> anyhow::Result<()> {
    crate::poll::until(
        std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(5),
        move || async move {
            match owner_claim_of(client, folder).await {
                Ok(claim) if claim["status"]["phase"].as_str() == Some("Bound") => {
                    crate::poll::Step::Done(())
                }
                Ok(_) => crate::poll::Step::Pending(anyhow::anyhow!(
                    "the folder {folder} is still being created"
                )),
                Err(e) => crate::poll::Step::Pending(e),
            }
        },
    )
    .await
}

async fn owner_claims(client: &Client) -> anyhow::Result<Vec<Value>> {
    crate::k8s::list(
        client,
        "v1",
        "PersistentVolumeClaim",
        Some(NAMESPACE),
        &ListParams::default().labels(LABEL_FOLDER),
    )
    .await
}

pub(crate) async fn list(client: &Client) -> anyhow::Result<Vec<Folder>> {
    let (claims, volumes) = tokio::try_join!(owner_claims(client), folder_volumes(client))?;
    let mut folders: Vec<Folder> = claims
        .iter()
        .filter_map(|c| folder_of(c, &volumes))
        .collect();
    folders.sort_by_key(|f| f.title.to_lowercase());
    Ok(folders)
}

async fn ensure_namespace(client: &Client) -> anyhow::Result<()> {
    crate::k8s::apply(
        client,
        &json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": NAMESPACE } }),
    )
    .await?;
    Ok(())
}

pub(crate) async fn create(client: &Client, title: &str) -> anyhow::Result<String> {
    let title = title.trim();
    let wanted = name_from_title(title).ok_or_else(|| {
        anyhow::anyhow!("a folder needs a name with at least one letter or number")
    })?;
    ensure_namespace(client).await?;
    let taken: BTreeSet<String> = list(client).await?.into_iter().map(|f| f.name).collect();
    let name = free_name(&wanted, &taken);
    crate::k8s::create(client, &files_claim(&name, title, NEW_FOLDER_SIZE)).await?;
    Ok(name)
}

pub(crate) enum Removal {
    Removed,
    InUse(Vec<String>),
}

pub(crate) fn origins_of<'a>(volumes: &'a [Value], folder: &str) -> Vec<&'a str> {
    volumes
        .iter()
        .filter(|pv| {
            let labels = &pv["metadata"]["labels"];
            labels[LABEL_STATE].as_str() == Some(folder)
                && labels[LABEL_STATE_ORIGIN].as_str() == Some("true")
        })
        .filter_map(|pv| pv["metadata"]["name"].as_str())
        .collect()
}

pub(crate) fn delete_when_released(volume: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": { "name": volume },
        "spec": { "persistentVolumeReclaimPolicy": "Delete" },
    })
}

async fn delete_folder(client: &Client, folder: &str, volumes: &[Value]) -> anyhow::Result<()> {
    crate::k8s::delete_if_present(
        client,
        &crate::k8s::reference(
            "v1",
            "PersistentVolumeClaim",
            NAMESPACE,
            &claim_name(folder),
        ),
    )
    .await?;
    let owner = owner_volume_name(folder);
    if volumes
        .iter()
        .any(|pv| pv["metadata"]["name"].as_str() == Some(owner.as_str()))
    {
        crate::k8s::delete_if_present(
            client,
            &crate::k8s::cluster_reference("v1", "PersistentVolume", &owner),
        )
        .await?;
    }
    for origin in origins_of(volumes, folder) {
        crate::k8s::merge_patch(client, &delete_when_released(origin)).await?;
    }
    Ok(())
}

pub(crate) async fn remove(client: &Client, folder: &str) -> anyhow::Result<Removal> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    let volumes = folder_volumes(client).await?;
    let in_use = users(&volumes, folder);
    if !in_use.is_empty() {
        return Ok(Removal::InUse(in_use));
    }
    delete_folder(client, folder, &volumes).await?;
    Ok(Removal::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_volume(path: Value) -> Value {
        json!({
            "spec": {
                "capacity": { "storage": "1Ti" },
                "csi": { "volumeAttributes": { "subvolumePath": path } },
            }
        })
    }

    fn app_pv(namespace: &str, folder: &str) -> Value {
        app_volume(
            folder,
            namespace,
            &source_of(&owner_volume(json!("/volumes/csi/csi-vol-1/abc"))).unwrap(),
        )
    }

    #[test]
    fn a_title_becomes_a_name_people_can_still_recognise() {
        assert_eq!(name_from_title("Movies & TV").as_deref(), Some("movies-tv"));
        assert_eq!(
            name_from_title("  Photos 2026 ").as_deref(),
            Some("photos-2026")
        );
        assert_eq!(name_from_title("Été / Ski").as_deref(), Some("t-ski"));
        assert_eq!(name_from_title("&&&"), None);
        let long = name_from_title(&"a ".repeat(60)).unwrap();
        assert!(is_name(&long), "{long}");
    }

    #[test]
    fn a_taken_name_gets_the_next_free_number() {
        let taken: BTreeSet<String> = ["movies", "movies-2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(free_name("movies", &taken), "movies-3");
        assert_eq!(free_name("photos", &taken), "photos");
        let long = "a".repeat(MAX_NAME);
        let taken: BTreeSet<String> = [long.clone()].into_iter().collect();
        assert!(is_name(&free_name(&long, &taken)));
    }

    #[test]
    fn only_plain_names_reach_a_volume_path() {
        assert!(is_name("movies-tv"));
        let long = "a".repeat(MAX_NAME + 1);
        for bad in ["", "-x", "x-", "Movies", "a/b", "a.b", long.as_str()] {
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn an_app_mounts_the_folders_own_files_and_deleting_the_mount_never_deletes_them() {
        let pv = app_pv("yolab-jellyfin", "movies");
        assert_eq!(pv["metadata"]["name"], "yolab-jellyfin.folder-movies");
        assert_eq!(pv["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert_eq!(
            pv["spec"]["csi"]["volumeAttributes"]["staticVolume"],
            "true"
        );
        assert_eq!(
            pv["spec"]["csi"]["volumeAttributes"]["rootPath"],
            "/volumes/csi/csi-vol-1/abc"
        );
        assert_eq!(pv["spec"]["csi"]["volumeHandle"], pv["metadata"]["name"]);
        assert_eq!(pv["spec"]["capacity"]["storage"], "1Ti");
    }

    #[test]
    fn only_the_app_it_was_made_for_can_claim_a_mount() {
        let pv = app_pv("yolab-jellyfin", "movies");
        assert_eq!(
            pv["spec"]["claimRef"],
            json!({ "namespace": "yolab-jellyfin", "name": "folder-movies" })
        );
        assert_eq!(pv["spec"]["storageClassName"], "");
    }

    #[test]
    fn a_folder_mount_is_recognised_even_when_the_chart_forgot_its_label() {
        let labelled: std::collections::BTreeMap<String, String> =
            [(LABEL_FOLDER.to_string(), "movies".to_string())]
                .into_iter()
                .collect();
        assert!(is_mount("yolab-a", Some(&labelled), None));
        assert!(is_mount("yolab-a", None, Some("yolab-a.folder-movies")));
        assert!(!is_mount("yolab-a", None, Some("yolab-b.folder-movies")));
        assert!(!is_mount("yolab-a", None, Some("pvc-0b1c")));
        assert!(!is_mount("yolab-a", None, None));
    }

    #[test]
    fn a_volume_without_a_readable_path_is_not_a_source() {
        assert!(source_of(&owner_volume(json!(null))).is_none());
        assert!(source_of(&owner_volume(json!("relative/path"))).is_none());
        assert!(source_of(&json!({})).is_none());
    }

    #[test]
    fn the_folder_lives_on_the_shared_filesystem_and_is_not_an_app_volume() {
        let claim = files_claim("movies-tv", "Movies & TV", NEW_FOLDER_SIZE);
        assert_eq!(claim["metadata"]["namespace"], NAMESPACE);
        assert_eq!(claim["metadata"]["labels"][LABEL_FOLDER], "movies-tv");
        assert_eq!(claim["metadata"]["annotations"][ANN_TITLE], "Movies & TV");
        assert_eq!(
            claim["spec"]["resources"]["requests"]["storage"],
            NEW_FOLDER_SIZE
        );
        assert_eq!(claim["spec"]["storageClassName"], "yolab-cephfs");
    }

    #[test]
    fn folder_fields_are_found_also_behind_a_switch() {
        let schema = json!({
            "type": "object",
            "properties": {
                "media": { "type": "string", "format": "folder" },
                "subdomain": { "type": "string" },
            },
            "dependencies": { "downloads_on": { "oneOf": [
                { "properties": { "downloads": { "type": "string", "format": "folder" } } }
            ]}},
        });
        assert_eq!(
            folder_fields(&schema).into_iter().collect::<Vec<_>>(),
            vec!["downloads", "media"]
        );
    }

    #[test]
    fn an_empty_folder_field_means_the_app_keeps_its_files_to_itself() {
        let app = AppSchema::new(json!({
            "properties": { "config": { "type": "object", "properties": {
                "media": { "type": "string", "format": "folder", "default": "" },
                "photos": { "type": "string", "format": "folder", "default": "photos" },
                "music": { "type": "string", "format": "folder" },
            }}}
        }));
        let mut config = Map::new();
        config.insert("media".into(), json!("movies-tv"));
        config.insert("music".into(), json!("  "));
        assert_eq!(
            wanted(&app, &config).into_iter().collect::<Vec<_>>(),
            vec!["movies-tv", "photos"]
        );
        assert!(wanted(&AppSchema::new(Value::Null), &config).is_empty());
    }

    #[test]
    fn a_folder_lists_the_apps_that_use_it_by_their_names() {
        let volumes = vec![
            app_pv("yolab-sonarr", "movies"),
            app_pv("yolab-jellyfin", "movies"),
            app_pv("yolab-immich", "photos"),
        ];
        assert_eq!(users(&volumes, "movies"), vec!["jellyfin", "sonarr"]);
        assert!(users(&volumes, "docs").is_empty());
        assert_eq!(
            attached(&volumes, "yolab-sonarr"),
            vec![(
                "yolab-sonarr.folder-movies".to_string(),
                "movies".to_string()
            )]
        );
    }

    #[test]
    fn a_folder_shows_its_title_size_and_whether_it_is_ready() {
        let mut claim = files_claim("movies-tv", "Movies & TV", "500Gi");
        claim["status"] = json!({ "phase": "Bound" });
        let folder = folder_of(&claim, &[app_pv("yolab-jellyfin", "movies-tv")]).unwrap();
        assert_eq!(
            folder,
            Folder {
                name: "movies-tv".into(),
                title: "Movies & TV".into(),
                size: "500Gi".into(),
                ready: true,
                used_by: vec!["jellyfin".into()],
                kind: KIND_FILES.into(),
                app: None,
            }
        );
        claim["status"] = json!({ "phase": "Pending" });
        assert!(!folder_of(&claim, &[]).unwrap().ready);
    }

    #[test]
    fn a_folder_survives_until_its_last_user_leaves() {
        let claims = vec![files_claim("movies", "Movies", NEW_FOLDER_SIZE)];
        let shared = vec![
            app_pv("yolab-sonarr", "movies"),
            app_pv("yolab-jellyfin", "movies"),
        ];
        let not_last = on_removal(&claims, &shared, "yolab-sonarr");
        assert!(not_last.deleted.is_empty());
        assert_eq!(
            not_last.kept,
            vec![Kept {
                title: "Movies".into(),
                used_by: vec!["jellyfin".into()],
            }]
        );
        let alone = vec![app_pv("yolab-sonarr", "movies")];
        let last = on_removal(&claims, &alone, "yolab-sonarr");
        assert_eq!(last.deleted, vec!["Movies"]);
        assert!(last.kept.is_empty());
    }

    #[test]
    fn an_apps_own_space_goes_when_its_last_user_goes() {
        let claims = vec![kept_claim("jellyfin-data", "jellyfin", "jellyfin-data")];
        let alone = vec![state_pv("jellyfin-data", "yolab-jellyfin", "Bound")];
        assert_eq!(
            on_removal(&claims, &alone, "yolab-jellyfin").deleted,
            vec!["jellyfin-data"]
        );
        let watched = vec![
            state_pv("jellyfin-data", "yolab-jellyfin", "Bound"),
            app_pv("yolab-filebrowser", "jellyfin-data"),
        ];
        let plan = on_removal(&claims, &watched, "yolab-jellyfin");
        assert!(plan.deleted.is_empty());
        assert_eq!(
            plan.kept,
            vec![Kept {
                title: "jellyfin-data".into(),
                used_by: vec!["filebrowser".into()],
            }]
        );
        assert!(on_removal(&claims, &watched, "yolab-elsewhere")
            .delete
            .is_empty());
    }

    fn members(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_apps_data_belongs_to_the_app_and_release_that_made_it() {
        assert_eq!(
            state_owner("jellyfin-data", "jellyfin", "jellyfin", &[]),
            StateOwner {
                app: "jellyfin".into(),
                release: "jellyfin".into(),
                claim: "jellyfin-data".into(),
            }
        );
    }

    #[test]
    fn data_inside_a_pack_belongs_to_the_app_of_the_pack_that_made_it() {
        let packed = state_owner(
            "media-sonarr-data",
            "movies-tv",
            "media",
            &members(&["jellyfin", "sonarr", "radarr"]),
        );
        assert_eq!(
            packed,
            StateOwner {
                app: "sonarr".into(),
                release: "media-sonarr".into(),
                claim: "media-sonarr-data".into(),
            }
        );
        let shared = state_owner("media-keys", "movies-tv", "media", &members(&["sonarr"]));
        assert_eq!(shared.app, "movies-tv");
    }

    #[test]
    fn an_apps_data_folder_is_named_after_the_app() {
        assert_eq!(
            state_folder_name("jellyfin", "jellyfin-data"),
            "jellyfin-data"
        );
        assert_eq!(
            state_folder_name("notes", "data-postgres-0"),
            "notes-data-postgres-0"
        );
        assert!(is_name(&state_folder_name(&"a".repeat(60), "data")));
    }

    fn app_claim(name: &str, phase: &str, volume: &str) -> Value {
        json!({
            "metadata": { "name": name, "namespace": "yolab-jellyfin" },
            "spec": { "volumeName": volume },
            "status": { "phase": phase },
        })
    }

    fn provisioned(path: &str) -> Value {
        json!({
            "metadata": { "name": "pvc-123" },
            "spec": {
                "capacity": { "storage": "50Gi" },
                "persistentVolumeReclaimPolicy": "Delete",
                "csi": {
                    "driver": "cephfs.csi.ceph.com",
                    "volumeAttributes": { "subvolumePath": path },
                },
            },
        })
    }

    #[test]
    fn only_an_apps_own_bound_cephfs_volume_becomes_a_folder() {
        let volume = provisioned("/volumes/csi/csi-vol-1/abc");
        assert!(adoptable(
            &app_claim("jellyfin-data", "Bound", "pvc-123"),
            &volume
        ));
        assert!(!adoptable(
            &app_claim("jellyfin-data", "Pending", "pvc-123"),
            &volume
        ));
        assert!(!adoptable(
            &app_claim("volsync-x", "Bound", "pvc-123"),
            &volume
        ));
        assert!(!adoptable(
            &app_claim("folder-movies", "Bound", "yolab-jellyfin.folder-movies"),
            &volume
        ));
        let mut block = volume.clone();
        block["spec"]["csi"]["driver"] = json!("rbd.csi.ceph.com");
        assert!(!adoptable(
            &app_claim("jellyfin-data", "Bound", "pvc-123"),
            &block
        ));
        assert!(!adoptable(
            &app_claim("jellyfin-data", "Bound", "pvc-123"),
            &provisioned("relative")
        ));
    }

    #[test]
    fn adopting_keeps_the_files_when_the_app_goes_and_remembers_whose_they_are() {
        let patch = kept_for("pvc-123", "jellyfin-data", "yolab-jellyfin");
        assert_eq!(patch["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert_eq!(patch["metadata"]["labels"][LABEL_STATE], "jellyfin-data");
        assert_eq!(
            patch["metadata"]["labels"][LABEL_STATE_NAMESPACE],
            "yolab-jellyfin"
        );
        assert_eq!(patch["metadata"]["labels"][LABEL_STATE_ORIGIN], "true");
    }

    #[test]
    fn an_adopted_folder_points_at_the_same_files_without_copying_them() {
        let source = source_of(&provisioned("/volumes/csi/csi-vol-1/abc")).unwrap();
        let volume = state_owner_volume("jellyfin-data", "cephfs.csi.ceph.com", &source);
        assert_eq!(
            volume["metadata"]["name"],
            "yolab-folders.state-jellyfin-data"
        );
        assert_eq!(
            volume["spec"]["csi"]["volumeAttributes"]["rootPath"],
            "/volumes/csi/csi-vol-1/abc"
        );
        assert_eq!(volume["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert_eq!(volume["spec"]["csi"]["driver"], "cephfs.csi.ceph.com");
        let owner = StateOwner {
            app: "jellyfin".into(),
            release: "jellyfin".into(),
            claim: "jellyfin-data".into(),
        };
        let claim = state_owner_claim("jellyfin-data", "jellyfin-data", &owner, "50Gi");
        assert_eq!(claim["spec"]["volumeName"], volume["metadata"]["name"]);
        assert_eq!(claim["spec"]["storageClassName"], "");
        assert_eq!(claim["metadata"]["labels"][LABEL_KIND], KIND_STATE);
        assert_eq!(
            claim["metadata"]["annotations"][ANN_STATE_RELEASE],
            "jellyfin"
        );
        assert_eq!(
            claim["metadata"]["annotations"][ANN_STATE_CLAIM],
            "jellyfin-data"
        );
        assert!(source_of(&volume).is_some());
    }

    fn state_pv(folder: &str, namespace: &str, phase: &str) -> Value {
        let mut pv = provisioned("/volumes/csi/csi-vol-1/abc");
        pv["metadata"]["labels"] =
            kept_for("pvc-123", folder, namespace)["metadata"]["labels"].clone();
        pv["status"] = json!({ "phase": phase });
        pv
    }

    #[test]
    fn an_app_uses_its_data_folder_only_while_it_is_installed() {
        let volumes = vec![state_pv("jellyfin-data", "yolab-jellyfin", "Bound")];
        assert_eq!(users(&volumes, "jellyfin-data"), vec!["jellyfin"]);
        let gone = vec![state_pv("jellyfin-data", "yolab-jellyfin", "Released")];
        assert!(users(&gone, "jellyfin-data").is_empty());
    }

    fn kept_claim(folder: &str, release: &str, claim: &str) -> Value {
        let owner = StateOwner {
            app: "a".into(),
            release: release.into(),
            claim: claim.into(),
        };
        let mut c = state_owner_claim(folder, folder, &owner, "50Gi");
        c["spec"]["volumeName"] = json!(owner_volume_name(folder));
        c
    }

    #[test]
    fn data_kept_after_an_uninstall_is_found_by_the_release_that_made_it() {
        let claims = vec![
            kept_claim("jellyfin-data", "jellyfin", "jellyfin-data"),
            kept_claim("media-sonarr-config", "media-sonarr", "media-sonarr-config"),
            kept_claim("other-data", "other", "other-data"),
            files_claim("movies", "Movies", NEW_FOLDER_SIZE),
        ];
        let found = kept_data_of(&claims, &[], "jellyfin", &[]);
        assert_eq!(
            found,
            vec![KeptData {
                folder: "jellyfin-data".into(),
                claim: "jellyfin-data".into(),
                volume: owner_volume_name("jellyfin-data"),
            }]
        );
        let pack = kept_data_of(&claims, &[], "media", &["sonarr".to_string()]);
        assert_eq!(pack.len(), 1);
        assert_eq!(pack[0].claim, "media-sonarr-config");
    }

    #[test]
    fn data_an_installed_app_still_uses_is_never_handed_to_a_second_app() {
        let claims = vec![kept_claim("jellyfin-data", "jellyfin", "jellyfin-data")];
        let in_use = vec![state_pv("jellyfin-data", "yolab-jellyfin", "Bound")];
        assert!(kept_data_of(&claims, &in_use, "jellyfin", &[]).is_empty());
        let released = vec![state_pv("jellyfin-data", "yolab-jellyfin", "Released")];
        assert_eq!(kept_data_of(&claims, &released, "jellyfin", &[]).len(), 1);
    }

    #[test]
    fn removing_a_data_folder_lets_its_original_volume_delete_the_files() {
        let volumes = vec![
            state_pv("jellyfin-data", "yolab-jellyfin", "Released"),
            state_pv("other", "yolab-x", "Released"),
        ];
        assert_eq!(origins_of(&volumes, "jellyfin-data"), vec!["pvc-123"]);
        assert_eq!(
            delete_when_released("pvc-123")["spec"]["persistentVolumeReclaimPolicy"],
            "Delete"
        );
    }

    #[test]
    fn an_app_reinstalled_on_its_kept_data_binds_to_the_same_files() {
        let source = source_of(&provisioned("/volumes/csi/csi-vol-1/abc")).unwrap();
        let volume = reuse_volume(
            "yolab-jellyfin",
            "jellyfin-data",
            "jellyfin-data",
            "cephfs.csi.ceph.com",
            &source,
        );
        assert_eq!(volume["metadata"]["name"], "yolab-jellyfin.jellyfin-data");
        assert_eq!(
            volume["spec"]["claimRef"],
            json!({ "namespace": "yolab-jellyfin", "name": "jellyfin-data" })
        );
        assert_eq!(volume["spec"]["storageClassName"], "yolab-cephfs");
        assert_eq!(
            volume["spec"]["csi"]["volumeAttributes"]["rootPath"],
            "/volumes/csi/csi-vol-1/abc"
        );
        assert_eq!(volume["metadata"]["labels"][LABEL_STATE], "jellyfin-data");
        assert_eq!(
            volume["metadata"]["labels"][LABEL_STATE_NAMESPACE],
            "yolab-jellyfin"
        );
        assert!(
            volume["metadata"]["labels"][LABEL_STATE_ORIGIN].is_null(),
            "not an origin, so its folder is never deleted out from under the app"
        );
        assert_eq!(state_folder_of(&volume), Some("jellyfin-data"));
    }

    #[test]
    fn a_folder_restored_under_its_own_name_is_a_plain_files_folder_of_the_backed_up_size() {
        let claim = files_claim("movies", "Movies", "500Gi");
        assert_eq!(claim["metadata"]["name"], "folder-movies");
        assert_eq!(claim["metadata"]["labels"][LABEL_KIND], KIND_FILES);
        assert_eq!(claim["metadata"]["annotations"][ANN_TITLE], "Movies");
        assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "500Gi");
        assert_eq!(files_claim("movies", "Movies", "500Gi"), claim);
    }

    mod against_kubernetes {
        use super::*;
        use crate::k8s::testing::{
            accept_patches, api_server, list as listed, patched, serve, status,
        };
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};

        async fn folder_ready(server: &wiremock::MockServer, folder: &str) {
            let mut claim = files_claim(folder, folder, NEW_FOLDER_SIZE);
            claim["spec"]["volumeName"] = json!("pvc-owner");
            serve(
                server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-{folder}"),
                200,
                claim,
            )
            .await;
            let mut owner = owner_volume(json!("/volumes/csi/csi-vol-9/xyz"));
            owner["apiVersion"] = json!("v1");
            owner["kind"] = json!("PersistentVolume");
            owner["metadata"] = json!({ "name": "pvc-owner" });
            serve(server, "/api/v1/persistentvolumes/pvc-owner", 200, owner).await;
        }

        #[tokio::test]
        async fn attaching_mounts_each_chosen_folder_into_the_app() {
            let (server, client) = api_server().await;
            folder_ready(&server, "movies").await;
            accept_patches(&server).await;
            let folders: BTreeSet<String> = ["movies".to_string()].into_iter().collect();
            attach(&client, "yolab-jellyfin", &folders).await.unwrap();
            let applied = patched(&server).await;
            assert_eq!(applied.len(), 1);
            assert_eq!(
                applied[0]["metadata"]["name"],
                "yolab-jellyfin.folder-movies"
            );
            assert_eq!(
                applied[0]["spec"]["csi"]["volumeAttributes"]["rootPath"],
                "/volumes/csi/csi-vol-9/xyz"
            );
        }

        #[tokio::test]
        async fn a_missing_folder_stops_the_install_with_a_reason_a_person_can_act_on() {
            let (server, client) = api_server().await;
            serve(
                &server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-gone"),
                404,
                status(404, "NotFound"),
            )
            .await;
            let folders: BTreeSet<String> = ["gone".to_string()].into_iter().collect();
            let e = attach(&client, "yolab-jellyfin", &folders)
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("does not exist"), "{e:#}");
            assert!(patched(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_folder_still_being_made_is_not_mounted_half_ready() {
            let (server, client) = api_server().await;
            serve(
                &server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-new"),
                200,
                files_claim("new", "New", NEW_FOLDER_SIZE),
            )
            .await;
            let folders: BTreeSet<String> = ["new".to_string()].into_iter().collect();
            let e = attach(&client, "yolab-x", &folders).await.unwrap_err();
            assert!(format!("{e:#}").contains("still being created"), "{e:#}");
        }

        #[tokio::test]
        async fn a_folder_name_that_is_not_plain_never_reaches_kubernetes() {
            let (server, client) = api_server().await;
            let folders: BTreeSet<String> = ["../etc".to_string()].into_iter().collect();
            assert!(attach(&client, "yolab-x", &folders).await.is_err());
            assert!(server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty());
        }

        async fn serve_volumes(server: &wiremock::MockServer, items: Vec<Value>) {
            serve(
                server,
                "/api/v1/persistentvolumes",
                200,
                listed("PersistentVolume", items),
            )
            .await;
        }

        async fn deleted(server: &wiremock::MockServer) -> Vec<String> {
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|r| r.method.as_str() == "DELETE")
                .map(|r| r.url.path().to_string())
                .collect()
        }

        async fn accept_deletes(server: &wiremock::MockServer) {
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status(200, "Success")))
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn a_folder_the_app_no_longer_uses_is_unmounted_and_others_are_left_alone() {
            let (server, client) = api_server().await;
            serve_volumes(
                &server,
                vec![
                    app_pv("yolab-sonarr", "movies"),
                    app_pv("yolab-sonarr", "old"),
                    app_pv("yolab-radarr", "old"),
                ],
            )
            .await;
            accept_deletes(&server).await;
            let keep: BTreeSet<String> = ["movies".to_string()].into_iter().collect();
            release_unused(&client, "yolab-sonarr", &keep)
                .await
                .unwrap();
            assert_eq!(
                deleted(&server).await,
                vec!["/api/v1/persistentvolumes/yolab-sonarr.folder-old"]
            );
        }

        #[tokio::test]
        async fn removing_an_app_unmounts_every_folder_it_had() {
            let (server, client) = api_server().await;
            serve_volumes(
                &server,
                vec![
                    app_pv("yolab-sonarr", "movies"),
                    app_pv("yolab-sonarr", "tv"),
                ],
            )
            .await;
            accept_deletes(&server).await;
            release_all(&client, "yolab-sonarr").await.unwrap();
            assert_eq!(deleted(&server).await.len(), 2);
        }

        #[tokio::test]
        async fn removing_an_app_deletes_only_the_spaces_it_was_last_to_use() {
            let (server, client) = api_server().await;
            serve_volumes(
                &server,
                vec![
                    app_pv("yolab-sonarr", "movies"),
                    app_pv("yolab-jellyfin", "movies"),
                    app_pv("yolab-sonarr", "tv"),
                ],
            )
            .await;
            serve(
                &server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"),
                200,
                listed(
                    "PersistentVolumeClaim",
                    vec![
                        files_claim("movies", "Movies", NEW_FOLDER_SIZE),
                        files_claim("tv", "TV", NEW_FOLDER_SIZE),
                    ],
                ),
            )
            .await;
            accept_deletes(&server).await;
            let doomed = doomed_spaces(&client, "yolab-sonarr").await.unwrap();
            let_go(&client, "yolab-sonarr", &doomed).await.unwrap();
            let deleted = deleted(&server).await;
            assert!(
                deleted.contains(&"/api/v1/persistentvolumes/yolab-sonarr.folder-tv".to_string())
            );
            assert!(deleted
                .contains(&"/api/v1/persistentvolumes/yolab-sonarr.folder-movies".to_string()));
            assert!(deleted.contains(&format!(
                "/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-tv"
            )));
            assert!(!deleted.contains(&format!(
                "/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-movies"
            )));
        }

        #[tokio::test]
        async fn a_folder_in_use_is_kept_and_names_who_uses_it() {
            let (server, client) = api_server().await;
            serve_volumes(&server, vec![app_pv("yolab-jellyfin", "movies")]).await;
            accept_deletes(&server).await;
            match remove(&client, "movies").await.unwrap() {
                Removal::InUse(apps) => assert_eq!(apps, vec!["jellyfin"]),
                Removal::Removed => panic!("a folder an app mounts was deleted"),
            }
            assert!(deleted(&server).await.is_empty());
        }

        #[tokio::test]
        async fn an_unused_folder_is_deleted_with_its_files() {
            let (server, client) = api_server().await;
            serve_volumes(&server, vec![]).await;
            accept_deletes(&server).await;
            assert!(matches!(
                remove(&client, "movies").await.unwrap(),
                Removal::Removed
            ));
            assert_eq!(
                deleted(&server).await,
                vec![format!(
                    "/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-movies"
                )]
            );
        }

        async fn installed_jellyfin(server: &wiremock::MockServer, volume: Value) {
            let mut claim = app_claim("jellyfin-data", "Bound", "pvc-123");
            claim["apiVersion"] = json!("v1");
            claim["kind"] = json!("PersistentVolumeClaim");
            serve(
                server,
                "/api/v1/namespaces/yolab-jellyfin/persistentvolumeclaims",
                200,
                listed("PersistentVolumeClaim", vec![claim]),
            )
            .await;
            serve(
                server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"),
                200,
                listed("PersistentVolumeClaim", vec![]),
            )
            .await;
            let mut volume = volume;
            volume["apiVersion"] = json!("v1");
            volume["kind"] = json!("PersistentVolume");
            serve(server, "/api/v1/persistentvolumes/pvc-123", 200, volume).await;
            for path in [
                "/api/v1/persistentvolumes/yolab-folders.state-jellyfin-data".to_string(),
                format!(
                    "/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/folder-jellyfin-data"
                ),
            ] {
                serve(server, &path, 404, status(404, "NotFound")).await;
            }
            accept_patches(server).await;
        }

        fn jellyfin() -> AppVolumes<'static> {
            AppVolumes {
                namespace: "yolab-jellyfin",
                app_id: "jellyfin",
                release: "jellyfin",
                members: &[],
            }
        }

        #[tokio::test]
        async fn adopting_keeps_the_files_before_it_records_them_as_a_folder() {
            let (server, client) = api_server().await;
            installed_jellyfin(&server, provisioned("/volumes/csi/csi-vol-1/abc")).await;
            let adopted = adopt(&client, &jellyfin()).await.unwrap();
            assert_eq!(adopted, vec!["jellyfin-data"]);
            let names: Vec<String> = patched(&server)
                .await
                .iter()
                .map(|p| {
                    format!(
                        "{}/{}",
                        p["kind"].as_str().unwrap_or(""),
                        p["metadata"]["name"].as_str().unwrap_or("")
                    )
                })
                .collect();
            let retain = names
                .iter()
                .position(|n| n == "PersistentVolume/pvc-123")
                .unwrap();
            let owner = names
                .iter()
                .position(|n| n == "PersistentVolume/yolab-folders.state-jellyfin-data")
                .unwrap();
            let folder = names
                .iter()
                .position(|n| n == "PersistentVolumeClaim/folder-jellyfin-data")
                .unwrap();
            assert!(retain < owner && owner < folder, "{names:?}");
        }

        #[tokio::test]
        async fn a_volume_already_adopted_is_never_adopted_twice() {
            let (server, client) = api_server().await;
            let mut volume = provisioned("/volumes/csi/csi-vol-1/abc");
            volume["metadata"]["labels"] = json!({ LABEL_STATE: "jellyfin-data" });
            installed_jellyfin(&server, volume).await;
            assert!(adopt(&client, &jellyfin()).await.unwrap().is_empty());
            assert!(!patched(&server)
                .await
                .iter()
                .any(|p| p["kind"] == "PersistentVolume" && p["metadata"]["name"] == "pvc-123"));
        }

        #[tokio::test]
        async fn a_half_finished_adoption_gets_its_folder_on_the_next_pass() {
            let (server, client) = api_server().await;
            let mut volume = provisioned("/volumes/csi/csi-vol-1/abc");
            volume["metadata"]["labels"] = kept_for("pvc-123", "jellyfin-data", "yolab-jellyfin")
                ["metadata"]["labels"]
                .clone();
            installed_jellyfin(&server, volume).await;
            assert!(adopt(&client, &jellyfin()).await.unwrap().is_empty());
            assert!(patched(&server)
                .await
                .iter()
                .any(|p| p["metadata"]["name"] == "folder-jellyfin-data"));
        }

        #[tokio::test]
        async fn removing_an_unused_data_folder_lets_its_files_go() {
            let (server, client) = api_server().await;
            let mut owner = provisioned("/x");
            owner["metadata"]["name"] = json!("yolab-folders.state-jellyfin-data");
            serve_volumes(
                &server,
                vec![
                    state_pv("jellyfin-data", "yolab-jellyfin", "Released"),
                    owner,
                ],
            )
            .await;
            accept_deletes(&server).await;
            accept_patches(&server).await;
            assert!(matches!(
                remove(&client, "jellyfin-data").await.unwrap(),
                Removal::Removed
            ));
            assert_eq!(deleted(&server).await.len(), 2);
            let flipped = patched(&server).await;
            assert_eq!(flipped.len(), 1);
            assert_eq!(flipped[0]["metadata"]["name"], "pvc-123");
            assert_eq!(
                flipped[0]["spec"]["persistentVolumeReclaimPolicy"],
                "Delete"
            );
        }

        #[tokio::test]
        async fn a_data_folder_is_kept_while_its_app_is_installed() {
            let (server, client) = api_server().await;
            serve_volumes(
                &server,
                vec![state_pv("jellyfin-data", "yolab-jellyfin", "Bound")],
            )
            .await;
            accept_deletes(&server).await;
            assert!(matches!(
                remove(&client, "jellyfin-data").await.unwrap(),
                Removal::InUse(_)
            ));
            assert!(deleted(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_reinstall_is_pointed_back_at_the_data_its_folder_kept() {
            let (server, client) = api_server().await;
            let owner = StateOwner {
                app: "jellyfin".into(),
                release: "jellyfin".into(),
                claim: "jellyfin-data".into(),
            };
            let mut claim = state_owner_claim("jellyfin-data", "jellyfin-data", &owner, "50Gi");
            claim["spec"]["volumeName"] = json!("yolab-folders.state-jellyfin-data");
            serve(
                &server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"),
                200,
                listed("PersistentVolumeClaim", vec![claim]),
            )
            .await;
            let mut volume = provisioned("/volumes/csi/csi-vol-1/abc");
            volume["metadata"]["name"] = json!("yolab-folders.state-jellyfin-data");
            volume["spec"]["csi"]["driver"] = json!("cephfs.csi.ceph.com");
            serve(
                &server,
                "/api/v1/persistentvolumes/yolab-folders.state-jellyfin-data",
                200,
                volume,
            )
            .await;
            serve_volumes(&server, vec![]).await;
            accept_patches(&server).await;

            let reused = reuse_data_folders(&client, "yolab-jellyfin", "jellyfin", &[])
                .await
                .unwrap();
            assert_eq!(reused, vec!["jellyfin-data"]);
            let applied = patched(&server).await;
            assert_eq!(applied.len(), 1);
            assert_eq!(
                applied[0]["metadata"]["name"],
                "yolab-jellyfin.jellyfin-data"
            );
            assert_eq!(
                applied[0]["spec"]["claimRef"],
                json!({ "namespace": "yolab-jellyfin", "name": "jellyfin-data" })
            );
            assert_eq!(
                applied[0]["spec"]["csi"]["volumeAttributes"]["rootPath"],
                "/volumes/csi/csi-vol-1/abc"
            );
        }

        #[tokio::test]
        async fn a_first_install_of_an_app_with_no_kept_data_reuses_nothing() {
            let (server, client) = api_server().await;
            serve(
                &server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"),
                200,
                listed("PersistentVolumeClaim", vec![]),
            )
            .await;
            accept_patches(&server).await;
            serve_volumes(&server, vec![]).await;
            assert!(
                reuse_data_folders(&client, "yolab-jellyfin", "jellyfin", &[])
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(patched(&server).await.is_empty());
        }

        #[tokio::test]
        async fn an_unreadable_mount_list_never_counts_as_unused() {
            let (server, client) = api_server().await;
            serve(
                &server,
                "/api/v1/persistentvolumes",
                503,
                status(503, "ServiceUnavailable"),
            )
            .await;
            accept_deletes(&server).await;
            assert!(remove(&client, "movies").await.is_err());
            assert!(deleted(&server).await.is_empty());
        }
    }
}
