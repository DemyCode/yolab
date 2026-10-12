use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
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
const KIND_FILES: &str = "files";
const LABEL_STATE: &str = "yolab.io/state-folder";
const LABEL_STATE_NAMESPACE: &str = "yolab.io/state-namespace";
const LABEL_STATE_ORIGIN: &str = "yolab.io/state-origin";
const ANN_TITLE: &str = "yolab.io/folder-title";
const ANN_LINKED_AT: &str = "yolab.io/linked-at";
const ANN_UNLINKED_SINCE: &str = "yolab.io/unlinked-since";
const FORMAT: &str = "folder";
const DRIVER: &str = "rook-ceph.cephfs.csi.ceph.com";
const CSI_NAMESPACE: &str = "rook-ceph";
const MAX_NAME: usize = 40;
const SPACE_ID_BYTES: usize = 6;
pub(crate) const NEW_FOLDER_SIZE: &str = "1Ti";
pub(crate) const GRACE_SECS: i64 = 3600;

#[derive(Serialize, Debug, PartialEq)]
pub struct Folder {
    pub name: String,
    pub title: String,
    pub size: String,
    pub ready: bool,
    pub used_by: Vec<String>,
    #[serde(skip)]
    pub claim: String,
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

pub(crate) fn is_claim(claim: &str) -> bool {
    !claim.is_empty()
        && claim.len() <= 63
        && !claim.starts_with('-')
        && !claim.ends_with('-')
        && claim
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(crate) fn free_name(wanted: &str, taken: &BTreeSet<String>) -> String {
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

pub(crate) fn new_space_claim() -> String {
    format!(
        "space-{}",
        crate::routers::backup_common::random_hex(SPACE_ID_BYTES)
    )
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

pub(crate) fn space_claim_manifest(claim: &str, folder: &str, title: &str, storage: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim,
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

pub(crate) fn state_folder_of(volume: &Value) -> Option<&str> {
    volume["metadata"]["labels"][LABEL_STATE]
        .as_str()
        .filter(|f| !f.is_empty())
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

pub(crate) fn renamed(
    app: &AppSchema,
    config: &Map<String, Value>,
    renames: &BTreeMap<String, String>,
) -> Map<String, Value> {
    let settings = app.with_defaults(config);
    let mut out = config.clone();
    for field in folder_fields(&app.config()) {
        let to = settings
            .get(&field)
            .and_then(Value::as_str)
            .map(str::trim)
            .and_then(|value| renames.get(value));
        if let Some(to) = to {
            out.insert(field, Value::String(to.clone()));
        }
    }
    out
}

pub(crate) fn linked_patch(claim: &str, now: DateTime<Utc>) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim,
            "namespace": NAMESPACE,
            "annotations": { ANN_LINKED_AT: now.to_rfc3339(), ANN_UNLINKED_SINCE: null },
        },
    })
}

fn unlinked_patch(claim: &str, since: Option<DateTime<Utc>>) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim,
            "namespace": NAMESPACE,
            "annotations": { ANN_UNLINKED_SINCE: since.map(|t| t.to_rfc3339()) },
        },
    })
}

pub(crate) async fn attach(
    client: &Client,
    namespace: &str,
    folders: &BTreeSet<String>,
) -> anyhow::Result<()> {
    for folder in folders {
        anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
        let claim = space_claim(client, folder).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "the folder {folder} does not exist — pick another one in the app's settings"
            )
        })?;
        let claim_name = claim["metadata"]["name"].as_str().unwrap_or_default();
        crate::k8s::merge_patch(client, &linked_patch(claim_name, Utc::now())).await?;
        let source = source(client, folder, &claim).await?;
        crate::k8s::apply(client, &app_volume(folder, namespace, &source)).await?;
    }
    Ok(())
}

pub(crate) async fn space_claim(client: &Client, folder: &str) -> anyhow::Result<Option<Value>> {
    let mut found = crate::k8s::list(
        client,
        "v1",
        "PersistentVolumeClaim",
        Some(NAMESPACE),
        &ListParams::default().labels(&format!("{LABEL_FOLDER}={folder}")),
    )
    .await?;
    found.retain(|c| c["metadata"]["deletionTimestamp"].is_null());
    found.sort_by(|a, b| {
        a["metadata"]["name"]
            .as_str()
            .cmp(&b["metadata"]["name"].as_str())
    });
    Ok(found.into_iter().next())
}

async fn source(client: &Client, folder: &str, claim: &Value) -> anyhow::Result<Source> {
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

fn link_of(pv: &Value) -> Option<(&str, &str)> {
    let labels = &pv["metadata"]["labels"];
    if let (Some(folder), Some(ns)) = (
        labels[LABEL_FOLDER].as_str(),
        labels[LABEL_APP_NAMESPACE].as_str(),
    ) {
        return Some((folder, ns));
    }
    if labels[LABEL_STATE_ORIGIN].as_str() != Some("true")
        && pv["status"]["phase"].as_str() == Some("Bound")
    {
        if let (Some(folder), Some(ns)) = (
            labels[LABEL_STATE].as_str(),
            labels[LABEL_STATE_NAMESPACE].as_str(),
        ) {
            return Some((folder, ns));
        }
    }
    None
}

fn instance_of(namespace: &str) -> String {
    namespace
        .strip_prefix("yolab-")
        .unwrap_or(namespace)
        .to_string()
}

fn users(volumes: &[Value], folder: &str) -> Vec<String> {
    let mut users: Vec<String> = volumes
        .iter()
        .filter_map(link_of)
        .filter(|(f, _)| *f == folder)
        .map(|(_, ns)| instance_of(ns))
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
        claim: claim["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        name,
    })
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub title: String,
    pub used_by: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct OnRemoval {
    pub deleted: Vec<String>,
    pub kept: Vec<Kept>,
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
        } else {
            plan.kept.push(Kept {
                title: folder.title,
                used_by: others,
            });
        }
    }
    plan
}

pub(crate) async fn on_removal_of(client: &Client, namespace: &str) -> anyhow::Result<OnRemoval> {
    let (claims, volumes) = tokio::try_join!(space_claims(client), folder_volumes(client))?;
    Ok(on_removal(&claims, &volumes, namespace))
}

fn time_of(value: &Value) -> Option<Option<DateTime<Utc>>> {
    match value.as_str() {
        None => Some(None),
        Some(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|t| Some(t.with_timezone(&Utc))),
    }
}

fn within_grace(value: &Value, now: DateTime<Utc>) -> bool {
    match time_of(value) {
        Some(Some(t)) => (now - t).num_seconds() < GRACE_SECS,
        Some(None) => false,
        None => true,
    }
}

fn recently_linked(claim: &Value, now: DateTime<Utc>) -> bool {
    within_grace(&claim["metadata"]["annotations"][ANN_LINKED_AT], now)
        || within_grace(&claim["metadata"]["creationTimestamp"], now)
}

fn is_static(pv: &Value) -> bool {
    pv["spec"]["persistentVolumeReclaimPolicy"].as_str() == Some("Retain")
        && pv["spec"]["csi"]["volumeAttributes"]["staticVolume"].as_str() == Some("true")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sweep {
    Forget(String),
    Linked(String),
    Unlinked(String),
    Delete {
        claim: String,
        folder: String,
        resource_version: String,
    },
    LetGo(String),
}

pub(crate) fn sweep(
    claims: &[Value],
    volumes: &[Value],
    namespaces: &BTreeSet<String>,
    now: DateTime<Utc>,
) -> Vec<Sweep> {
    let mut linked: BTreeSet<&str> = BTreeSet::new();
    let mut steps = Vec::new();
    for pv in volumes {
        let name = pv["metadata"]["name"].as_str().unwrap_or_default();
        let labels = &pv["metadata"]["labels"];
        match link_of(pv) {
            Some((_, ns)) if !namespaces.contains(ns) && is_static(pv) => {
                steps.push(Sweep::Forget(name.to_string()));
            }
            Some((folder, _)) => {
                linked.insert(folder);
            }
            None => {
                let leftover_reuse = labels[LABEL_STATE].is_string()
                    && labels[LABEL_STATE_ORIGIN].as_str() != Some("true")
                    && labels[LABEL_STATE_NAMESPACE]
                        .as_str()
                        .is_some_and(|ns| !namespaces.contains(ns));
                if leftover_reuse && is_static(pv) {
                    steps.push(Sweep::Forget(name.to_string()));
                }
            }
        }
    }
    let spaces: BTreeSet<&str> = claims
        .iter()
        .filter_map(|c| c["metadata"]["labels"][LABEL_FOLDER].as_str())
        .collect();
    for pv in volumes {
        let name = pv["metadata"]["name"].as_str().unwrap_or_default();
        let labels = &pv["metadata"]["labels"];
        let owner_of = labels[LABEL_FOLDER]
            .as_str()
            .filter(|_| labels[LABEL_APP_NAMESPACE].is_null());
        if let Some(folder) = owner_of {
            if !spaces.contains(folder)
                && is_static(pv)
                && pv["status"]["phase"].as_str() != Some("Bound")
            {
                steps.push(Sweep::Forget(name.to_string()));
            }
        }
        if labels[LABEL_STATE_ORIGIN].as_str() == Some("true") {
            if let Some(folder) = labels[LABEL_STATE].as_str() {
                if !spaces.contains(folder) && !linked.contains(folder) {
                    steps.push(Sweep::LetGo(name.to_string()));
                }
            }
        }
    }
    for claim in claims {
        let Some(folder) = claim["metadata"]["labels"][LABEL_FOLDER].as_str() else {
            continue;
        };
        let name = claim["metadata"]["name"].as_str().unwrap_or_default();
        let since = &claim["metadata"]["annotations"][ANN_UNLINKED_SINCE];
        if linked.contains(folder) {
            if !since.is_null() {
                steps.push(Sweep::Linked(name.to_string()));
            }
            continue;
        }
        match time_of(since) {
            Some(Some(t)) => {
                if (now - t).num_seconds() >= GRACE_SECS && !recently_linked(claim, now) {
                    if let Some(rv) = claim["metadata"]["resourceVersion"].as_str() {
                        steps.push(Sweep::Delete {
                            claim: name.to_string(),
                            folder: folder.to_string(),
                            resource_version: rv.to_string(),
                        });
                    }
                }
            }
            _ => steps.push(Sweep::Unlinked(name.to_string())),
        }
    }
    steps
}

fn claim_reference(claim: &str) -> Value {
    crate::k8s::reference("v1", "PersistentVolumeClaim", NAMESPACE, claim)
}

pub(crate) fn delete_when_released(volume: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolume",
        "metadata": { "name": volume },
        "spec": { "persistentVolumeReclaimPolicy": "Delete" },
    })
}

async fn namespaces(client: &Client) -> anyhow::Result<BTreeSet<String>> {
    Ok(
        crate::k8s::list(client, "v1", "Namespace", None, &ListParams::default())
            .await?
            .iter()
            .filter_map(|ns| ns["metadata"]["name"].as_str().map(str::to_string))
            .collect(),
    )
}

pub(crate) async fn sweep_unlinked(client: &Client) -> anyhow::Result<Vec<String>> {
    let (claims, volumes, namespaces) = tokio::try_join!(
        space_claims(client),
        folder_volumes(client),
        namespaces(client)
    )?;
    let mut deleted = Vec::new();
    for step in sweep(&claims, &volumes, &namespaces, Utc::now()) {
        match step {
            Sweep::Forget(volume) => {
                crate::k8s::delete_if_present(
                    client,
                    &crate::k8s::cluster_reference("v1", "PersistentVolume", &volume),
                )
                .await?
            }
            Sweep::Linked(claim) => {
                crate::k8s::merge_patch(client, &unlinked_patch(&claim, None)).await?
            }
            Sweep::Unlinked(claim) => {
                crate::k8s::merge_patch(client, &unlinked_patch(&claim, Some(Utc::now()))).await?
            }
            Sweep::Delete {
                claim,
                folder,
                resource_version,
            } => match crate::k8s::delete_unchanged(
                client,
                &claim_reference(&claim),
                &resource_version,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!("the space {folder} had no app for an hour — deleted it");
                    deleted.push(folder);
                }
                Err(e) if crate::k8s::refused_with(&e, 409) => {
                    tracing::info!("the space {folder} changed while being deleted — kept it");
                }
                Err(e) => return Err(e),
            },
            Sweep::LetGo(volume) => {
                crate::k8s::merge_patch(client, &delete_when_released(&volume)).await?
            }
        }
    }
    Ok(deleted)
}

pub(crate) async fn wait_until_ready(client: &Client, folder: &str) -> anyhow::Result<()> {
    crate::poll::until(
        std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(5),
        move || async move {
            match space_claim(client, folder).await {
                Ok(Some(claim)) if claim["status"]["phase"].as_str() == Some("Bound") => {
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

async fn space_claims(client: &Client) -> anyhow::Result<Vec<Value>> {
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
    let (claims, volumes) = tokio::try_join!(space_claims(client), folder_volumes(client))?;
    let mut folders: Vec<Folder> = claims
        .iter()
        .filter(|c| c["metadata"]["deletionTimestamp"].is_null())
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

pub(crate) async fn create_named(
    client: &Client,
    claim: &str,
    folder: &str,
    title: &str,
    storage: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    anyhow::ensure!(is_claim(claim), "{claim:?} is not a space id");
    ensure_namespace(client).await?;
    crate::k8s::create(client, &space_claim_manifest(claim, folder, title, storage)).await?;
    Ok(())
}

pub(crate) async fn create(client: &Client, title: &str) -> anyhow::Result<String> {
    let title = title.trim();
    let wanted = name_from_title(title).ok_or_else(|| {
        anyhow::anyhow!("a folder needs a name with at least one letter or number")
    })?;
    ensure_namespace(client).await?;
    let taken: BTreeSet<String> = space_claims(client)
        .await?
        .iter()
        .filter_map(|c| c["metadata"]["labels"][LABEL_FOLDER].as_str())
        .map(str::to_string)
        .collect();
    let name = free_name(&wanted, &taken);
    crate::k8s::create(
        client,
        &space_claim_manifest(&new_space_claim(), &name, title, NEW_FOLDER_SIZE),
    )
    .await?;
    Ok(name)
}

pub(crate) enum Removal {
    Removed,
    InUse(Vec<String>),
}

pub(crate) async fn remove(client: &Client, folder: &str) -> anyhow::Result<Removal> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    let claim = space_claim(client, folder)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the folder {folder} does not exist"))?;
    let volumes = folder_volumes(client).await?;
    let in_use = users(&volumes, folder);
    if !in_use.is_empty() {
        return Ok(Removal::InUse(in_use));
    }
    anyhow::ensure!(
        !recently_linked(&claim, Utc::now()),
        "an app was just set up to use the folder {folder} — try again in an hour"
    );
    let name = claim["metadata"]["name"].as_str().unwrap_or_default();
    let version = claim["metadata"]["resourceVersion"]
        .as_str()
        .unwrap_or_default();
    match crate::k8s::delete_unchanged(client, &claim_reference(name), version).await {
        Ok(()) => Ok(Removal::Removed),
        Err(e) if crate::k8s::refused_with(&e, 409) => {
            anyhow::bail!("the folder {folder} changed while being deleted — try again")
        }
        Err(e) => Err(e),
    }
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

    fn space(claim: &str, folder: &str) -> Value {
        let mut c = space_claim_manifest(claim, folder, folder, NEW_FOLDER_SIZE);
        c["metadata"]["resourceVersion"] = json!("7");
        c["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
        c
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn all(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
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
        let taken = all(&["movies", "movies-2"]);
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
    fn a_new_space_gets_an_identity_of_its_own_that_no_name_can_reuse() {
        let a = new_space_claim();
        let b = new_space_claim();
        assert_ne!(a, b);
        assert!(a.starts_with("space-"), "{a}");
        assert!(is_claim(&a));
        assert!(
            format!("volsync-dst-restore-{a}").len() <= 63,
            "volsync names jobs after it"
        );
        assert!(!is_claim("../x"));
        assert!(!is_claim("Space-1"));
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
    fn the_folder_lives_on_the_shared_filesystem_under_its_own_identity() {
        let claim = space_claim_manifest("space-0a1b2c", "movies-tv", "Movies & TV", "500Gi");
        assert_eq!(claim["metadata"]["name"], "space-0a1b2c");
        assert_eq!(claim["metadata"]["namespace"], NAMESPACE);
        assert_eq!(claim["metadata"]["labels"][LABEL_FOLDER], "movies-tv");
        assert_eq!(claim["metadata"]["annotations"][ANN_TITLE], "Movies & TV");
        assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "500Gi");
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

    fn media_app() -> AppSchema {
        AppSchema::new(json!({
            "properties": { "config": { "type": "object", "properties": {
                "media": { "type": "string", "format": "folder", "default": "" },
                "photos": { "type": "string", "format": "folder", "default": "photos" },
                "music": { "type": "string", "format": "folder" },
                "subdomain": { "type": "string" },
            }}}
        }))
    }

    #[test]
    fn an_empty_folder_field_means_the_app_keeps_its_files_to_itself() {
        let mut config = Map::new();
        config.insert("media".into(), json!("movies-tv"));
        config.insert("music".into(), json!("  "));
        assert_eq!(
            wanted(&media_app(), &config).into_iter().collect::<Vec<_>>(),
            vec!["movies-tv", "photos"]
        );
        assert!(wanted(&AppSchema::new(Value::Null), &config).is_empty());
    }

    #[test]
    fn a_space_restored_under_a_new_name_is_renamed_only_in_folder_settings() {
        let mut config = Map::new();
        config.insert("media".into(), json!("movies"));
        config.insert("subdomain".into(), json!("movies"));
        let renames: BTreeMap<String, String> = [
            ("movies".to_string(), "movies-2".to_string()),
            ("photos".to_string(), "photos-2".to_string()),
        ]
        .into();
        let out = renamed(&media_app(), &config, &renames);
        assert_eq!(out["media"], "movies-2");
        assert_eq!(out["subdomain"], "movies");
        assert_eq!(out["photos"], "photos-2", "a default folder is renamed too");
        assert!(!out.contains_key("music"));
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
    fn a_folder_shows_its_title_size_identity_and_whether_it_is_ready() {
        let mut claim = space_claim_manifest("space-1", "movies-tv", "Movies & TV", "500Gi");
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
                claim: "space-1".into(),
            }
        );
        claim["status"] = json!({ "phase": "Pending" });
        assert!(!folder_of(&claim, &[]).unwrap().ready);
    }

    #[test]
    fn removing_an_app_says_which_spaces_go_and_which_stay_for_others() {
        let claims = vec![space("space-1", "movies"), space("space-2", "tv")];
        let volumes = vec![
            app_pv("yolab-sonarr", "movies"),
            app_pv("yolab-jellyfin", "movies"),
            app_pv("yolab-sonarr", "tv"),
        ];
        assert_eq!(
            on_removal(&claims, &volumes, "yolab-sonarr"),
            OnRemoval {
                deleted: vec!["tv".into()],
                kept: vec![Kept {
                    title: "movies".into(),
                    used_by: vec!["jellyfin".into()],
                }],
            }
        );
        assert_eq!(
            on_removal(&claims, &volumes, "yolab-other"),
            OnRemoval::default()
        );
    }

    fn deletes(steps: &[Sweep]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|s| match s {
                Sweep::Delete { folder, .. } => Some(folder.as_str()),
                _ => None,
            })
            .collect()
    }

    fn unlinked_since(mut claim: Value, since: &str) -> Value {
        claim["metadata"]["annotations"][ANN_UNLINKED_SINCE] = json!(since);
        claim
    }

    const NOW: &str = "2026-10-12T12:00:00Z";

    #[test]
    fn a_space_an_app_still_uses_is_never_deleted_whoever_made_it() {
        let claims = vec![unlinked_since(
            space("space-1", "movies"),
            "2026-10-01T00:00:00Z",
        )];
        let volumes = vec![app_pv("yolab-jellyfin", "movies")];
        let steps = sweep(&claims, &volumes, &all(&["yolab-jellyfin"]), t(NOW));
        assert_eq!(steps, vec![Sweep::Linked("space-1".into())]);
    }

    #[test]
    fn a_space_no_app_uses_is_first_marked_then_deleted_an_hour_later() {
        let fresh = vec![space("space-1", "movies")];
        assert_eq!(
            sweep(&fresh, &[], &all(&[]), t(NOW)),
            vec![Sweep::Unlinked("space-1".into())]
        );
        let marked = vec![unlinked_since(
            space("space-1", "movies"),
            "2026-10-12T11:30:00Z",
        )];
        assert!(sweep(&marked, &[], &all(&[]), t(NOW)).is_empty());
        let long_ago = vec![unlinked_since(
            space("space-1", "movies"),
            "2026-10-12T10:59:00Z",
        )];
        assert_eq!(
            sweep(&long_ago, &[], &all(&[]), t(NOW)),
            vec![Sweep::Delete {
                claim: "space-1".into(),
                folder: "movies".into(),
                resource_version: "7".into(),
            }]
        );
    }

    #[test]
    fn a_space_an_install_just_picked_is_not_deleted_before_its_mount_exists() {
        let mut claim = unlinked_since(space("space-1", "movies"), "2026-10-12T09:00:00Z");
        claim["metadata"]["annotations"][ANN_LINKED_AT] = json!("2026-10-12T11:59:00Z");
        assert!(deletes(&sweep(&[claim], &[], &all(&[]), t(NOW))).is_empty());
        let mut garbled = unlinked_since(space("space-1", "movies"), "2026-10-12T09:00:00Z");
        garbled["metadata"]["annotations"][ANN_LINKED_AT] = json!("not a time");
        assert!(deletes(&sweep(&[garbled], &[], &all(&[]), t(NOW))).is_empty());
    }

    #[test]
    fn a_space_created_a_moment_ago_waits_for_the_install_that_asked_for_it() {
        let mut claim = unlinked_since(space("space-1", "movies"), "2026-10-12T09:00:00Z");
        claim["metadata"]["creationTimestamp"] = json!("2026-10-12T11:50:00Z");
        assert!(deletes(&sweep(&[claim], &[], &all(&[]), t(NOW))).is_empty());
    }

    #[test]
    fn the_mount_of_an_app_that_is_gone_no_longer_keeps_the_space() {
        let claims = vec![unlinked_since(
            space("space-1", "movies"),
            "2026-10-12T10:00:00Z",
        )];
        let volumes = vec![app_pv("yolab-gone", "movies")];
        let steps = sweep(&claims, &volumes, &all(&["yolab-other"]), t(NOW));
        assert!(steps.contains(&Sweep::Forget("yolab-gone.folder-movies".into())));
        assert_eq!(deletes(&steps), vec!["movies"]);
    }

    #[test]
    fn an_app_being_removed_still_holds_its_spaces_until_its_namespace_is_gone() {
        let claims = vec![unlinked_since(
            space("space-1", "movies"),
            "2026-10-12T10:00:00Z",
        )];
        let volumes = vec![app_pv("yolab-going", "movies")];
        let steps = sweep(&claims, &volumes, &all(&["yolab-going"]), t(NOW));
        assert_eq!(steps, vec![Sweep::Linked("space-1".into())]);
    }

    #[test]
    fn a_marked_space_without_a_version_is_never_deleted_blind() {
        let mut claim = unlinked_since(space("space-1", "movies"), "2026-10-12T09:00:00Z");
        claim["metadata"]["resourceVersion"] = Value::Null;
        assert!(deletes(&sweep(&[claim], &[], &all(&[]), t(NOW))).is_empty());
    }

    fn provisioned(name: &str) -> Value {
        json!({
            "metadata": { "name": name },
            "spec": {
                "capacity": { "storage": "50Gi" },
                "persistentVolumeReclaimPolicy": "Retain",
                "csi": {
                    "driver": "cephfs.csi.ceph.com",
                    "volumeAttributes": { "subvolumePath": "/volumes/csi/csi-vol-1/abc" },
                },
            },
        })
    }

    fn origin(folder: &str, namespace: &str, phase: &str) -> Value {
        let mut pv = provisioned("pvc-123");
        pv["metadata"]["labels"] = json!({
            LABEL_STATE: folder,
            LABEL_STATE_NAMESPACE: namespace,
            LABEL_STATE_ORIGIN: "true",
        });
        pv["status"] = json!({ "phase": phase });
        pv
    }

    fn reused(folder: &str, namespace: &str, phase: &str) -> Value {
        let mut pv = provisioned(&format!("{namespace}.{folder}"));
        pv["metadata"]["labels"] = json!({ LABEL_STATE: folder, LABEL_STATE_NAMESPACE: namespace });
        pv["spec"]["csi"]["volumeAttributes"]["staticVolume"] = json!("true");
        pv["status"] = json!({ "phase": phase });
        pv
    }

    fn old_state_space(folder: &str) -> Value {
        let mut c = unlinked_since(space(&claim_name(folder), folder), "2026-10-12T09:00:00Z");
        c["metadata"]["labels"][LABEL_KIND] = json!("state");
        c["spec"]["volumeName"] = json!(format!("{NAMESPACE}.state-{folder}"));
        c
    }

    #[test]
    fn an_apps_own_volume_is_not_a_link_to_the_space_it_was_once_turned_into() {
        let claims = vec![old_state_space("jellyfin-data")];
        let volumes = vec![origin("jellyfin-data", "yolab-jellyfin", "Bound")];
        let steps = sweep(&claims, &volumes, &all(&["yolab-jellyfin"]), t(NOW));
        assert_eq!(deletes(&steps), vec!["jellyfin-data"]);
    }

    #[test]
    fn once_such_a_space_is_gone_the_apps_volume_is_deleted_with_the_app_again() {
        let mut owner = provisioned(&format!("{NAMESPACE}.state-jellyfin-data"));
        owner["metadata"]["labels"] = json!({ LABEL_FOLDER: "jellyfin-data" });
        owner["spec"]["csi"]["volumeAttributes"]["staticVolume"] = json!("true");
        owner["status"] = json!({ "phase": "Released" });
        let volumes = vec![origin("jellyfin-data", "yolab-jellyfin", "Bound"), owner];
        let steps = sweep(&[], &volumes, &all(&["yolab-jellyfin"]), t(NOW));
        assert!(steps.contains(&Sweep::LetGo("pvc-123".into())));
        assert!(steps.contains(&Sweep::Forget(format!(
            "{NAMESPACE}.state-jellyfin-data"
        ))));
    }

    #[test]
    fn data_an_app_was_reinstalled_onto_stays_while_that_app_is_installed() {
        let claims = vec![old_state_space("jellyfin-data")];
        let volumes = vec![
            origin("jellyfin-data", "yolab-jellyfin", "Released"),
            reused("jellyfin-data", "yolab-jellyfin", "Bound"),
        ];
        let steps = sweep(&claims, &volumes, &all(&["yolab-jellyfin"]), t(NOW));
        assert!(deletes(&steps).is_empty());
        assert!(!steps.iter().any(|s| matches!(s, Sweep::LetGo(_))));
        let no_claim = sweep(&[], &volumes, &all(&["yolab-jellyfin"]), t(NOW));
        assert!(!no_claim.iter().any(|s| matches!(s, Sweep::LetGo(_))));
    }

    #[test]
    fn a_space_another_app_mounts_keeps_the_original_volume_alive() {
        let claims = vec![old_state_space("jellyfin-data")];
        let volumes = vec![
            origin("jellyfin-data", "yolab-jellyfin", "Released"),
            app_pv("yolab-filebrowser", "jellyfin-data"),
        ];
        let steps = sweep(&claims, &volumes, &all(&["yolab-filebrowser"]), t(NOW));
        assert_eq!(steps, vec![Sweep::Linked(claim_name("jellyfin-data"))]);
    }

    #[test]
    fn only_volumes_that_hold_no_data_of_their_own_are_ever_forgotten() {
        let mut dynamic = app_pv("yolab-gone", "movies");
        dynamic["spec"]["persistentVolumeReclaimPolicy"] = json!("Delete");
        assert!(sweep(&[], &[dynamic], &all(&[]), t(NOW)).is_empty());
        let leftover = reused("jellyfin-data", "yolab-gone", "Released");
        assert_eq!(
            sweep(&[], &[leftover], &all(&[]), t(NOW)),
            vec![Sweep::Forget("yolab-gone.jellyfin-data".into())]
        );
    }

    #[test]
    fn letting_go_lets_the_volume_delete_its_files_when_released() {
        assert_eq!(
            delete_when_released("pvc-123")["spec"]["persistentVolumeReclaimPolicy"],
            "Delete"
        );
    }

    #[test]
    fn marks_are_written_as_annotations_and_cleared_with_null() {
        let linked = linked_patch("space-1", t(NOW));
        let annotations = linked["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(annotations[ANN_LINKED_AT], t(NOW).to_rfc3339());
        assert!(annotations[ANN_UNLINKED_SINCE].is_null());
        assert_eq!(linked["metadata"]["namespace"], NAMESPACE);
        let cleared = unlinked_patch("space-1", None);
        assert!(cleared["metadata"]["annotations"]
            .as_object()
            .unwrap()
            .contains_key(ANN_UNLINKED_SINCE));
    }

    mod against_kubernetes {
        use super::*;
        use crate::k8s::testing::{
            accept_patches, api_server, list as listed, patched, serve, status,
        };
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};

        async fn serve_spaces(server: &wiremock::MockServer, claims: Vec<Value>) {
            serve(
                server,
                &format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims"),
                200,
                listed("PersistentVolumeClaim", claims),
            )
            .await;
        }

        async fn folder_ready(server: &wiremock::MockServer, folder: &str) {
            let mut claim = space_claim_manifest("space-9", folder, folder, NEW_FOLDER_SIZE);
            claim["spec"]["volumeName"] = json!("pvc-owner");
            serve_spaces(server, vec![claim]).await;
            let mut owner = owner_volume(json!("/volumes/csi/csi-vol-9/xyz"));
            owner["apiVersion"] = json!("v1");
            owner["kind"] = json!("PersistentVolume");
            owner["metadata"] = json!({ "name": "pvc-owner" });
            serve(server, "/api/v1/persistentvolumes/pvc-owner", 200, owner).await;
        }

        #[tokio::test]
        async fn attaching_marks_the_space_linked_before_it_mounts_it_into_the_app() {
            let (server, client) = api_server().await;
            folder_ready(&server, "movies").await;
            accept_patches(&server).await;
            attach(&client, "yolab-jellyfin", &all(&["movies"]))
                .await
                .unwrap();
            let applied = patched(&server).await;
            assert_eq!(applied.len(), 2);
            assert_eq!(applied[0]["metadata"]["name"], "space-9");
            assert!(applied[0]["metadata"]["annotations"][ANN_LINKED_AT].is_string());
            assert_eq!(
                applied[1]["metadata"]["name"],
                "yolab-jellyfin.folder-movies"
            );
            assert_eq!(
                applied[1]["spec"]["csi"]["volumeAttributes"]["rootPath"],
                "/volumes/csi/csi-vol-9/xyz"
            );
        }

        #[tokio::test]
        async fn a_missing_folder_stops_the_install_with_a_reason_a_person_can_act_on() {
            let (server, client) = api_server().await;
            serve_spaces(&server, vec![]).await;
            let e = attach(&client, "yolab-jellyfin", &all(&["gone"]))
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("does not exist"), "{e:#}");
            assert!(patched(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_folder_still_being_made_is_not_mounted_half_ready() {
            let (server, client) = api_server().await;
            serve_spaces(
                &server,
                vec![space_claim_manifest("space-1", "new", "New", NEW_FOLDER_SIZE)],
            )
            .await;
            accept_patches(&server).await;
            let e = attach(&client, "yolab-x", &all(&["new"])).await.unwrap_err();
            assert!(format!("{e:#}").contains("still being created"), "{e:#}");
        }

        #[tokio::test]
        async fn a_folder_name_that_is_not_plain_never_reaches_kubernetes() {
            let (server, client) = api_server().await;
            assert!(attach(&client, "yolab-x", &all(&["../etc"])).await.is_err());
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
            release_unused(&client, "yolab-sonarr", &all(&["movies"]))
                .await
                .unwrap();
            assert_eq!(
                deleted(&server).await,
                vec!["/api/v1/persistentvolumes/yolab-sonarr.folder-old"]
            );
        }

        #[tokio::test]
        async fn removing_an_app_unmounts_every_folder_it_had_and_deletes_no_space() {
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
            let gone = deleted(&server).await;
            assert_eq!(gone.len(), 2);
            assert!(gone
                .iter()
                .all(|p| p.starts_with("/api/v1/persistentvolumes/")));
        }

        #[tokio::test]
        async fn a_folder_in_use_is_kept_and_names_who_uses_it() {
            let (server, client) = api_server().await;
            serve_spaces(&server, vec![space("space-1", "movies")]).await;
            serve_volumes(&server, vec![app_pv("yolab-jellyfin", "movies")]).await;
            accept_deletes(&server).await;
            match remove(&client, "movies").await.unwrap() {
                Removal::InUse(apps) => assert_eq!(apps, vec!["jellyfin"]),
                Removal::Removed => panic!("a folder an app mounts was deleted"),
            }
            assert!(deleted(&server).await.is_empty());
        }

        #[tokio::test]
        async fn an_unused_folder_is_deleted_only_if_nothing_changed_since_it_was_read() {
            let (server, client) = api_server().await;
            serve_spaces(&server, vec![space("space-1", "movies")]).await;
            serve_volumes(&server, vec![]).await;
            accept_deletes(&server).await;
            assert!(matches!(
                remove(&client, "movies").await.unwrap(),
                Removal::Removed
            ));
            let requests = server.received_requests().await.unwrap_or_default();
            let delete = requests
                .iter()
                .find(|r| r.method.as_str() == "DELETE")
                .unwrap();
            assert_eq!(
                delete.url.path(),
                format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims/space-1")
            );
            let body: Value = serde_json::from_slice(&delete.body).unwrap();
            assert_eq!(body["preconditions"]["resourceVersion"], "7");
        }

        #[tokio::test]
        async fn an_unreadable_mount_list_never_counts_as_unused() {
            let (server, client) = api_server().await;
            serve_spaces(&server, vec![space("space-1", "movies")]).await;
            serve(
                &server,
                "/api/v1/persistentvolumes",
                503,
                status(503, "ServiceUnavailable"),
            )
            .await;
            accept_deletes(&server).await;
            assert!(remove(&client, "movies").await.is_err());
            assert!(sweep_unlinked(&client).await.is_err());
            assert!(deleted(&server).await.is_empty());
        }

        #[tokio::test]
        async fn a_space_that_changed_while_being_deleted_is_kept() {
            let (server, client) = api_server().await;
            serve_spaces(
                &server,
                vec![unlinked_since(
                    space("space-1", "movies"),
                    "2026-01-01T00:00:00Z",
                )],
            )
            .await;
            serve_volumes(&server, vec![]).await;
            serve(
                &server,
                "/api/v1/namespaces",
                200,
                listed("Namespace", vec![]),
            )
            .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(409).set_body_json(status(409, "Conflict")))
                .mount(&server)
                .await;
            assert!(sweep_unlinked(&client).await.unwrap().is_empty());
        }
    }
}
