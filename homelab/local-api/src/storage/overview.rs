use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use axum::{extract::State, http::StatusCode, Json};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};

use crate::cephfs::{DATA_POOL, FS_NAME, META_POOL, SUBVOLUME_GROUP};
use crate::host::{Host, RealHost};
use crate::AppState;

const KUBE_TIMEOUT: Duration = Duration::from_secs(10);
const PARALLEL_LOOKUPS: usize = 6;

#[derive(Debug, Serialize, PartialEq)]
pub struct Overview {
    pub health: String,
    pub space: Option<Space>,
    pub raw: Raw,
    pub osds: Vec<Osd>,
    pub pools: Vec<Pool>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Space {
    pub free_bytes: u64,
    pub apps_bytes: u64,
    pub images_bytes: u64,
    pub other_bytes: u64,
    pub copies: u32,
    pub fullest_disk_percent: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Raw {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub data_bytes: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Osd {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub class: String,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub utilization: f64,
    pub var: f64,
    pub pgs: u64,
    pub up: bool,
    pub weight: f64,
    pub reweight: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Pool {
    pub id: u64,
    pub name: String,
    pub copies: u32,
    pub min_copies: u32,
    pub stored_bytes: u64,
    pub used_bytes: u64,
    pub max_avail_bytes: u64,
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn parse_raw(status: &Value) -> Raw {
    let pg = &status["pgmap"];
    Raw {
        total_bytes: u(&pg["bytes_total"]),
        used_bytes: u(&pg["bytes_used"]),
        avail_bytes: u(&pg["bytes_avail"]),
        data_bytes: u(&pg["data_bytes"]),
    }
}

fn parse_osds(tree: &Value) -> Vec<Osd> {
    let nodes = tree["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let host_of: HashMap<i64, &str> = nodes
        .iter()
        .filter(|n| n["type"] == "host")
        .flat_map(|h| {
            let name = h["name"].as_str().unwrap_or("");
            h["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |c| c.as_i64().map(|id| (id, name)))
        })
        .collect();
    let mut osds: Vec<Osd> = nodes
        .iter()
        .filter(|n| n["type"] == "osd")
        .map(|n| {
            let id = n["id"].as_i64().unwrap_or(-1);
            Osd {
                id,
                name: n["name"].as_str().unwrap_or("").to_string(),
                host: host_of.get(&id).copied().unwrap_or("").to_string(),
                class: n["device_class"]
                    .as_str()
                    .or_else(|| n["class"].as_str())
                    .unwrap_or("")
                    .to_string(),
                size_bytes: u(&n["kb"]) * 1024,
                used_bytes: u(&n["kb_used"]) * 1024,
                avail_bytes: u(&n["kb_avail"]) * 1024,
                utilization: n["utilization"].as_f64().unwrap_or(0.0),
                var: n["var"].as_f64().unwrap_or(0.0),
                pgs: u(&n["pgs"]),
                up: n["status"] == "up",
                weight: n["crush_weight"].as_f64().unwrap_or(0.0),
                reweight: n["reweight"].as_f64().unwrap_or(0.0),
            }
        })
        .collect();
    osds.sort_by(|a, b| a.host.cmp(&b.host).then(a.id.cmp(&b.id)));
    osds
}

fn parse_pools(df: &Value, detail: &Value) -> Vec<Pool> {
    let stats: HashMap<u64, &Value> = df["pools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["id"].as_u64().map(|id| (id, &p["stats"])))
        .collect();
    detail
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            let id = d["pool_id"]
                .as_u64()
                .or_else(|| d["pool"].as_u64())
                .unwrap_or(0);
            let s = stats.get(&id).copied().unwrap_or(&Value::Null);
            Pool {
                id,
                name: d["pool_name"].as_str().unwrap_or("").to_string(),
                copies: u(&d["size"]) as u32,
                min_copies: u(&d["min_size"]) as u32,
                stored_bytes: u(&s["stored"]),
                used_bytes: u(&s["bytes_used"]),
                max_avail_bytes: u(&s["max_avail"]),
            }
        })
        .collect()
}

fn space_of(pools: &[Pool], osds: &[Osd], images_pool: &str) -> Option<Space> {
    let data = pools.iter().find(|p| p.name == DATA_POOL)?;
    let stored = |pred: &dyn Fn(&Pool) -> bool| -> u64 {
        pools.iter().filter(|p| pred(p)).map(|p| p.stored_bytes).sum()
    };
    let is_app = |p: &Pool| p.name == DATA_POOL || p.name == META_POOL;
    let is_images = |p: &Pool| p.name == images_pool;
    Some(Space {
        free_bytes: data.max_avail_bytes,
        apps_bytes: stored(&is_app),
        images_bytes: stored(&is_images),
        other_bytes: stored(&|p| !is_app(p) && !is_images(p)),
        copies: data.copies,
        fullest_disk_percent: osds
            .iter()
            .filter(|o| o.up && o.weight > 0.0)
            .map(|o| o.utilization)
            .fold(0.0, f64::max),
    })
}

pub(crate) fn assemble(
    status: &Value,
    df: &Value,
    tree: &Value,
    detail: &Value,
    images_pool: &str,
) -> Overview {
    let osds = parse_osds(tree);
    let pools = parse_pools(df, detail);
    Overview {
        health: status["health"]["status"].as_str().unwrap_or("").to_string(),
        space: space_of(&pools, &osds, images_pool),
        raw: parse_raw(status),
        osds,
        pools,
    }
}

pub(crate) async fn read<H: Host>(host: &H, images_pool: &str) -> anyhow::Result<Overview> {
    let (status, df, tree, detail) = tokio::try_join!(
        host.ceph_json(&["status"]),
        host.ceph_json(&["df"]),
        host.ceph_json(&["osd", "df", "tree"]),
        host.ceph_json(&["osd", "pool", "ls", "detail"]),
    )?;
    Ok(assemble(&status, &df, &tree, &detail, images_pool))
}

fn failed(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": format!("Storage is not answering: {e}") })),
    )
}

pub async fn handler() -> (StatusCode, Json<Value>) {
    let images_pool = super::StorageEnv::from_env().images_pool;
    match read(&RealHost, &images_pool).await {
        Ok(o) => (StatusCode::OK, Json(json!(o))),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Checks {
    pub ok_to_stop: Vec<i64>,
    pub safe_to_destroy: Vec<i64>,
}

pub(crate) async fn checks<H: Host>(host: &H) -> anyhow::Result<Checks> {
    let ids: Vec<i64> = host
        .ceph_json(&["osd", "ls"])
        .await?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect();
    let verdicts: Vec<(i64, bool, bool)> = stream::iter(ids)
        .map(|id| async move {
            let name = format!("osd.{id}");
            let (stop, destroy) = tokio::join!(
                host.ceph(&["osd", "ok-to-stop", &name]),
                crate::ceph::destructive::safe_to_destroy(host, id),
            );
            (id, stop.is_ok(), matches!(destroy, Ok(Some(_))))
        })
        .buffer_unordered(PARALLEL_LOOKUPS)
        .collect()
        .await;
    let mut out = Checks {
        ok_to_stop: verdicts.iter().filter(|v| v.1).map(|v| v.0).collect(),
        safe_to_destroy: verdicts.iter().filter(|v| v.2).map(|v| v.0).collect(),
    };
    out.ok_to_stop.sort_unstable();
    out.safe_to_destroy.sort_unstable();
    Ok(out)
}

pub async fn checks_handler() -> (StatusCode, Json<Value>) {
    match checks(&RealHost).await {
        Ok(c) => (StatusCode::OK, Json(json!(c))),
        Err(e) => failed(e),
    }
}

pub async fn status_text_handler() -> (StatusCode, Json<Value>) {
    match RealHost.ceph(&["status"]).await {
        Ok(text) => (StatusCode::OK, Json(json!({ "text": text }))),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppVolume {
    pub namespace: String,
    pub subvolume: String,
}

pub(crate) fn app_volumes(pvs: &[Value]) -> Vec<AppVolume> {
    pvs.iter()
        .filter(|pv| {
            pv["spec"]["csi"]["driver"]
                .as_str()
                .is_some_and(|d| d.contains("cephfs"))
        })
        .filter_map(|pv| {
            Some(AppVolume {
                namespace: pv["spec"]["claimRef"]["namespace"].as_str()?.to_string(),
                subvolume: pv["spec"]["csi"]["volumeAttributes"]["subvolumeName"]
                    .as_str()?
                    .to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AppUsage {
    pub namespace: String,
    pub instance: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Usage {
    pub apps: Vec<AppUsage>,
    pub unreadable: u32,
}

pub(crate) fn tally(sizes: Vec<(String, Option<u64>)>) -> Usage {
    let mut by_ns: BTreeMap<String, u64> = BTreeMap::new();
    let mut unreadable = 0;
    for (ns, size) in sizes {
        match size {
            Some(b) => *by_ns.entry(ns).or_default() += b,
            None => unreadable += 1,
        }
    }
    let mut apps: Vec<AppUsage> = by_ns
        .into_iter()
        .map(|(namespace, bytes)| AppUsage {
            instance: namespace.strip_prefix("yolab-").map(str::to_string),
            namespace,
            bytes,
        })
        .collect();
    apps.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.namespace.cmp(&b.namespace)));
    Usage { apps, unreadable }
}

pub(crate) async fn usage<H: Host>(host: &H, volumes: Vec<AppVolume>) -> Usage {
    let sizes = stream::iter(volumes)
        .map(|v| async move {
            let size = host
                .ceph_json(&[
                    "fs",
                    "subvolume",
                    "info",
                    FS_NAME,
                    &v.subvolume,
                    "--group_name",
                    SUBVOLUME_GROUP,
                ])
                .await
                .ok()
                .and_then(|info| info["bytes_used"].as_u64());
            (v.namespace, size)
        })
        .buffer_unordered(PARALLEL_LOOKUPS)
        .collect()
        .await;
    tally(sizes)
}

pub async fn usage_handler(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let listed = async {
        let client = state.kube.client().await?;
        crate::k8s::list(&client, "v1", "PersistentVolume", None, &Default::default()).await
    };
    let pvs = match tokio::time::timeout(KUBE_TIMEOUT, listed).await {
        Ok(Ok(pvs)) => pvs,
        Ok(Err(e)) => return failed(e),
        Err(_) => return failed("the cluster did not list volumes in time"),
    };
    let u = usage(&RealHost, app_volumes(&pvs)).await;
    (StatusCode::OK, Json(json!(u)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn status() -> Value {
        json!({
            "health": {"status": "HEALTH_OK"},
            "pgmap": {
                "bytes_total": 4000, "bytes_used": 900,
                "bytes_avail": 3100, "data_bytes": 420
            }
        })
    }

    fn df() -> Value {
        json!({"pools": [
            {"id": 1, "name": ".mgr", "stats": {"stored": 5, "bytes_used": 10, "max_avail": 1200}},
            {"id": 2, "name": "yolab-fs-metadata", "stats": {"stored": 20, "bytes_used": 40, "max_avail": 1200}},
            {"id": 3, "name": "yolab-fs-data0", "stats": {"stored": 300, "bytes_used": 600, "max_avail": 1200}},
            {"id": 4, "name": "images", "stats": {"stored": 95, "bytes_used": 190, "max_avail": 1200}}
        ]})
    }

    fn detail() -> Value {
        json!([
            {"pool_id": 1, "pool_name": ".mgr", "size": 2, "min_size": 1},
            {"pool_id": 2, "pool_name": "yolab-fs-metadata", "size": 2, "min_size": 1},
            {"pool_id": 3, "pool_name": "yolab-fs-data0", "size": 2, "min_size": 1},
            {"pool_id": 4, "pool_name": "images", "size": 2, "min_size": 1}
        ])
    }

    fn tree() -> Value {
        json!({"nodes": [
            {"type": "host", "name": "node2", "children": [1]},
            {"type": "host", "name": "node1", "children": [0, 2]},
            {"type": "osd", "id": 0, "name": "osd.0", "device_class": "ssd",
             "kb": 2, "kb_used": 1, "kb_avail": 1, "utilization": 50.0,
             "var": 1.1, "pgs": 30, "status": "up", "crush_weight": 1.0, "reweight": 1.0},
            {"type": "osd", "id": 1, "name": "osd.1", "device_class": "hdd",
             "kb": 4, "kb_used": 1, "kb_avail": 3, "utilization": 25.0,
             "var": 0.6, "pgs": 20, "status": "up", "crush_weight": 2.0, "reweight": 1.0},
            {"type": "osd", "id": 2, "name": "osd.2",
             "kb": 4, "kb_used": 4, "kb_avail": 0, "utilization": 99.0,
             "var": 2.0, "pgs": 0, "status": "down", "crush_weight": 0.0, "reweight": 0.0}
        ]})
    }

    fn overview() -> Overview {
        assemble(&status(), &df(), &tree(), &detail(), "images")
    }

    #[test]
    fn the_raw_totals_are_exactly_what_ceph_status_reports() {
        let raw = overview().raw;
        assert_eq!(
            (raw.total_bytes, raw.used_bytes, raw.avail_bytes, raw.data_bytes),
            (4000, 900, 3100, 420)
        );
    }

    #[test]
    fn free_space_is_cephs_own_max_avail_for_the_app_data_pool() {
        assert_eq!(overview().space.unwrap().free_bytes, 1200);
    }

    #[test]
    fn app_data_counts_both_filesystem_pools_once_each() {
        assert_eq!(overview().space.unwrap().apps_bytes, 320);
    }

    #[test]
    fn container_images_are_reported_apart_from_app_data() {
        let s = overview().space.unwrap();
        assert_eq!((s.images_bytes, s.other_bytes), (95, 5));
    }

    #[test]
    fn the_configured_images_pool_name_is_honoured() {
        let o = assemble(&status(), &df(), &tree(), &detail(), "imgs");
        let s = o.space.unwrap();
        assert_eq!((s.images_bytes, s.other_bytes), (0, 100));
    }

    #[test]
    fn copies_come_from_the_app_data_pool() {
        assert_eq!(overview().space.unwrap().copies, 2);
    }

    #[test]
    fn the_fullest_disk_ignores_disks_that_are_down_or_out() {
        assert_eq!(overview().space.unwrap().fullest_disk_percent, 50.0);
    }

    #[test]
    fn there_is_no_space_figure_before_the_app_pool_exists() {
        let no_pools = json!({"pools": []});
        let o = assemble(&status(), &no_pools, &tree(), &json!([]), "images");
        assert_eq!(o.space, None);
    }

    #[test]
    fn osds_are_in_kib_and_converted_to_bytes() {
        let osd0 = overview().osds.into_iter().find(|o| o.id == 0).unwrap();
        assert_eq!(
            (osd0.size_bytes, osd0.used_bytes, osd0.avail_bytes),
            (2048, 1024, 1024)
        );
    }

    #[test]
    fn an_out_disk_reports_cephs_own_free_figure_not_its_size() {
        let osd2 = overview().osds.into_iter().find(|o| o.id == 2).unwrap();
        assert_eq!(osd2.avail_bytes, 0);
        assert!(!osd2.up);
    }

    #[test]
    fn osds_are_ordered_by_host_then_id_and_carry_their_host() {
        let order: Vec<(String, i64)> = overview()
            .osds
            .into_iter()
            .map(|o| (o.host, o.id))
            .collect();
        assert_eq!(
            order,
            vec![("node1".into(), 0), ("node1".into(), 2), ("node2".into(), 1)]
        );
    }

    #[test]
    fn pools_join_their_usage_by_id() {
        let data = overview()
            .pools
            .into_iter()
            .find(|p| p.name == "yolab-fs-data0")
            .unwrap();
        assert_eq!(
            (data.stored_bytes, data.used_bytes, data.copies),
            (300, 600, 2)
        );
    }

    #[test]
    fn health_is_read_through() {
        assert_eq!(overview().health, "HEALTH_OK");
    }

    fn answering() -> FakeHost {
        FakeHost::new()
            .ok("ceph status", &status().to_string())
            .ok("ceph df", &df().to_string())
            .ok("ceph osd df tree", &tree().to_string())
            .ok("ceph osd pool ls detail", &detail().to_string())
    }

    #[tokio::test]
    async fn read_asks_ceph_four_questions_and_nothing_per_disk() {
        let host = answering();
        read(&host, "images").await.unwrap();
        assert_eq!(host.calls().len(), 4);
        assert!(!host.ran("ok-to-stop") && !host.ran("safe-to-destroy"));
    }

    #[tokio::test]
    async fn a_failed_question_is_an_error_not_a_page_of_zeros() {
        let host = answering().fail("ceph df", "timed out");
        assert!(read(&host, "images").await.is_err());
    }

    #[tokio::test]
    async fn checks_report_each_disks_verdicts() {
        let host = FakeHost::new()
            .ok("ceph osd ls", "[0, 1]")
            .ok("ceph osd ok-to-stop osd.0", "")
            .fail("ceph osd ok-to-stop osd.1", "would make pgs inactive")
            .ok("ceph osd safe-to-destroy osd.0", r#"{"safe_to_destroy": []}"#)
            .ok("ceph osd safe-to-destroy osd.1", r#"{"safe_to_destroy": [1]}"#);
        let c = checks(&host).await.unwrap();
        assert_eq!(c.ok_to_stop, vec![0]);
        assert_eq!(c.safe_to_destroy, vec![1]);
    }

    #[tokio::test]
    async fn checks_fail_when_ceph_cannot_list_the_disks() {
        let host = FakeHost::new().fail("ceph osd ls", "unreachable");
        assert!(checks(&host).await.is_err());
    }

    fn pv(driver: &str, ns: &str, sub: &str) -> Value {
        json!({"spec": {
            "csi": {"driver": driver, "volumeAttributes": {"subvolumeName": sub}},
            "claimRef": {"namespace": ns}
        }})
    }

    #[test]
    fn only_cephfs_volumes_with_a_claim_are_counted() {
        let pvs = vec![
            pv("rook-ceph.cephfs.csi.ceph.com", "yolab-immich", "csi-vol-a"),
            pv("rook-ceph.rbd.csi.ceph.com", "yolab-immich", "csi-vol-b"),
            json!({"spec": {"csi": {"driver": "cephfs.csi.ceph.com",
                "volumeAttributes": {"subvolumeName": "csi-vol-c"}}}}),
        ];
        assert_eq!(
            app_volumes(&pvs),
            vec![AppVolume {
                namespace: "yolab-immich".into(),
                subvolume: "csi-vol-a".into()
            }]
        );
    }

    #[test]
    fn usage_sums_an_apps_volumes_and_sorts_largest_first() {
        let u = tally(vec![
            ("yolab-gitea".into(), Some(10)),
            ("yolab-immich".into(), Some(100)),
            ("yolab-gitea".into(), Some(5)),
        ]);
        let rows: Vec<(&str, Option<&str>, u64)> = u
            .apps
            .iter()
            .map(|a| (a.namespace.as_str(), a.instance.as_deref(), a.bytes))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("yolab-immich", Some("immich"), 100),
                ("yolab-gitea", Some("gitea"), 15)
            ]
        );
    }

    #[test]
    fn a_namespace_that_is_not_an_app_has_no_instance() {
        let u = tally(vec![("kube-system".into(), Some(1))]);
        assert_eq!(u.apps[0].instance, None);
    }

    #[test]
    fn an_unreadable_volume_is_counted_not_guessed() {
        let u = tally(vec![
            ("yolab-a".into(), None),
            ("yolab-a".into(), Some(3)),
        ]);
        assert_eq!((u.apps[0].bytes, u.unreadable), (3, 1));
    }

    #[tokio::test]
    async fn usage_reads_each_subvolume_from_the_app_group() {
        let host = FakeHost::new()
            .ok(
                "ceph fs subvolume info yolab-fs csi-vol-a --group_name csi",
                r#"{"bytes_used": 42}"#,
            )
            .fail("ceph fs subvolume info yolab-fs csi-vol-b", "ENOENT");
        let u = usage(
            &host,
            vec![
                AppVolume {
                    namespace: "yolab-x".into(),
                    subvolume: "csi-vol-a".into(),
                },
                AppVolume {
                    namespace: "yolab-x".into(),
                    subvolume: "csi-vol-b".into(),
                },
            ],
        )
        .await;
        assert_eq!((u.apps[0].bytes, u.unreadable), (42, 1));
    }
}
