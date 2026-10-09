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
const ANN_TITLE: &str = "yolab.io/folder-title";
const FORMAT: &str = "folder";
const DRIVER: &str = "rook-ceph.cephfs.csi.ceph.com";
const CSI_NAMESPACE: &str = "rook-ceph";
const MAX_NAME: usize = 40;
pub(crate) const MAX_SIZE_GIB: u64 = 1 << 20;

#[derive(Serialize, Debug, PartialEq)]
pub struct Folder {
    pub name: String,
    pub title: String,
    pub size: String,
    pub ready: bool,
    pub used_by: Vec<String>,
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

pub(crate) fn owner_claim(folder: &str, title: &str, size_gib: u64) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": claim_name(folder),
            "namespace": NAMESPACE,
            "labels": { LABEL_FOLDER: folder },
            "annotations": { ANN_TITLE: title },
        },
        "spec": {
            "accessModes": ["ReadWriteMany"],
            "storageClassName": crate::routers::copy::CEPHFS_STORAGE_CLASS,
            "resources": { "requests": { "storage": format!("{size_gib}Gi") } },
        },
    })
}

pub(crate) struct Source {
    root_path: String,
    capacity: String,
}

fn source_of(owner_volume: &Value) -> Option<Source> {
    let csi = &owner_volume["spec"]["csi"];
    let root_path = csi["volumeAttributes"]["subvolumePath"]
        .as_str()
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

async fn attached_volumes(client: &Client) -> anyhow::Result<Vec<Value>> {
    crate::k8s::list(
        client,
        "v1",
        "PersistentVolume",
        None,
        &ListParams::default().labels(LABEL_APP_NAMESPACE),
    )
    .await
}

pub(crate) async fn release_unused(
    client: &Client,
    namespace: &str,
    keep: &BTreeSet<String>,
) -> anyhow::Result<()> {
    for (volume, folder) in attached(&attached_volumes(client).await?, namespace) {
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

fn users(volumes: &[Value], folder: &str) -> Vec<String> {
    let mut users: Vec<String> = volumes
        .iter()
        .filter(|pv| pv["metadata"]["labels"][LABEL_FOLDER].as_str() == Some(folder))
        .filter_map(|pv| pv["metadata"]["labels"][LABEL_APP_NAMESPACE].as_str())
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
        name,
    })
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
    let (claims, volumes) = tokio::try_join!(owner_claims(client), attached_volumes(client))?;
    let mut folders: Vec<Folder> = claims
        .iter()
        .filter_map(|c| folder_of(c, &volumes))
        .collect();
    folders.sort_by_key(|f| f.title.to_lowercase());
    Ok(folders)
}

pub(crate) async fn create(client: &Client, title: &str, size_gib: u64) -> anyhow::Result<String> {
    let title = title.trim();
    let wanted = name_from_title(title).ok_or_else(|| {
        anyhow::anyhow!("a folder needs a name with at least one letter or number")
    })?;
    anyhow::ensure!(
        (1..=MAX_SIZE_GIB).contains(&size_gib),
        "a folder holds between 1 and {MAX_SIZE_GIB} GiB"
    );
    crate::k8s::apply(
        client,
        &json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": NAMESPACE },
        }),
    )
    .await?;
    let taken: BTreeSet<String> = list(client).await?.into_iter().map(|f| f.name).collect();
    let name = free_name(&wanted, &taken);
    crate::k8s::create(client, &owner_claim(&name, title, size_gib)).await?;
    Ok(name)
}

pub(crate) enum Removal {
    Removed,
    InUse(Vec<String>),
}

pub(crate) async fn remove(client: &Client, folder: &str) -> anyhow::Result<Removal> {
    anyhow::ensure!(is_name(folder), "{folder:?} is not a folder name");
    let in_use = users(&attached_volumes(client).await?, folder);
    if !in_use.is_empty() {
        return Ok(Removal::InUse(in_use));
    }
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
        let claim = owner_claim("movies-tv", "Movies & TV", 1024);
        assert_eq!(claim["metadata"]["namespace"], NAMESPACE);
        assert_eq!(claim["metadata"]["labels"][LABEL_FOLDER], "movies-tv");
        assert_eq!(claim["metadata"]["annotations"][ANN_TITLE], "Movies & TV");
        assert_eq!(claim["spec"]["resources"]["requests"]["storage"], "1024Gi");
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
        let mut claim = owner_claim("movies-tv", "Movies & TV", 500);
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
            }
        );
        claim["status"] = json!({ "phase": "Pending" });
        assert!(!folder_of(&claim, &[]).unwrap().ready);
    }

    mod against_kubernetes {
        use super::*;
        use crate::k8s::testing::{
            accept_patches, api_server, list as listed, patched, serve, status,
        };
        use wiremock::matchers::method;
        use wiremock::{Mock, ResponseTemplate};

        async fn folder_ready(server: &wiremock::MockServer, folder: &str) {
            let mut claim = owner_claim(folder, folder, 10);
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
                owner_claim("new", "New", 10),
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
