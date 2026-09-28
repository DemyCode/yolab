use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Outcome;
use crate::host::Host;
use crate::routers::backup_common::Backend;
use crate::storage::settings;
use crate::store::Store;
use crate::AppState;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoragePolicy {
    pub size: u32,
    pub failure_domain: String,
}

pub fn min_size_for(size: u32) -> u32 {
    size.saturating_sub(1).max(1)
}

pub enum PolicyState {
    NotChosen,
    Chosen(StoragePolicy),
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Topology {
    pub nodes: u32,
    pub osds: u32,
    pub osd_hosts: u32,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Target {
    pub size: u32,
    pub min_size: u32,
    pub failure_domain: String,
    pub mon: u32,
    pub mgr: u32,
}

pub fn compute_target(policy: &StoragePolicy, topo: &Topology) -> Target {
    let (mon, mgr) = (topo.nodes.max(1), topo.nodes.max(1));

    Target {
        size: policy.size,
        min_size: min_size_for(policy.size),
        failure_domain: policy.failure_domain.clone(),
        mon,
        mgr,
    }
}

async fn read_mirror_policy<H: crate::host::Host>(host: &H) -> Option<Option<StoragePolicy>> {
    match settings::get_json::<_, StoragePolicy>(host, settings::STORAGE_POLICY).await {
        Ok(found) => Some(found),
        Err(e) => {
            tracing::warn!("storage policy is unreadable right now ({e})");
            None
        }
    }
}

struct SyncedPolicy {
    stored: Option<StoragePolicy>,
    seeded: bool,
    changed: bool,
}

fn sync_policy(store: &mut Store, mirror: Option<Option<&StoragePolicy>>) -> SyncedPolicy {
    let mut changed = false;
    if let Some(found) = mirror {
        if let Some(p) = found {
            match store.import_storage_policy(p) {
                Ok(written) => changed |= written,
                Err(e) => {
                    tracing::warn!("the storage policy could not be taken into the store ({e})")
                }
            }
        }
        match store.mark_policy_seeded() {
            Ok(written) => changed |= written,
            Err(e) => tracing::warn!("could not record the storage policy as seeded ({e})"),
        }
    }
    let stored = store.storage_policy().unwrap_or_else(|e| {
        tracing::error!("the stored storage policy is unreadable ({e})");
        None
    });
    SyncedPolicy {
        stored,
        seeded: store.policy_seeded(),
        changed,
    }
}

fn merge_policy(
    mirror: Option<Option<StoragePolicy>>,
    stored: Option<StoragePolicy>,
    seeded: bool,
) -> Option<PolicyState> {
    if let Some(p) = stored {
        return Some(PolicyState::Chosen(p));
    }
    match mirror {
        Some(Some(p)) => Some(PolicyState::Chosen(p)),
        Some(None) => Some(PolicyState::NotChosen),
        None if seeded => Some(PolicyState::NotChosen),
        None => None,
    }
}

pub(crate) async fn read_policy_from<H: Host>(host: &H) -> Option<PolicyState> {
    let mirror = read_mirror_policy(host).await;
    let synced = {
        let mut store = crate::store::locked();
        let synced = sync_policy(&mut store, mirror.as_ref().map(|f| f.as_ref()));
        if synced.changed {
            if let Err(e) = store.persist(&crate::store::default_path()) {
                tracing::warn!("the desired-state store could not be saved ({e})");
            }
        }
        synced
    };
    merge_policy(mirror, synced.stored, synced.seeded)
}

async fn write_policy<H: crate::host::Host>(host: &H, p: &StoragePolicy) -> anyhow::Result<()> {
    write_policy_to_cluster(host, p).await?;
    record_policy_choice(p);
    crate::runtime::wake("topology");
    crate::runtime::wake("disks");
    Ok(())
}

async fn write_policy_to_cluster<H: crate::host::Host>(
    host: &H,
    p: &StoragePolicy,
) -> anyhow::Result<()> {
    settings::set_json(host, settings::STORAGE_POLICY, p).await?;
    Ok(())
}

fn record_policy_choice(p: &StoragePolicy) {
    let mut store = crate::store::locked();
    if let Err(e) = store.set_storage_policy(p) {
        tracing::warn!(
            "the storage policy was saved to the cluster but not recorded as a choice ({e}) — the \
             next read will take it from the cluster settings"
        );
        return;
    }
    if let Err(e) = store.persist(&crate::store::default_path()) {
        tracing::warn!("the chosen storage policy could not be saved to disk ({e})");
    }
}

pub(crate) async fn observe<H: Host>(b: &Backend<H>) -> Option<Topology> {
    let nodes = crate::k8s::list(&b.kube, "v1", "Node", None, &Default::default())
        .await
        .ok()?
        .len() as u32;
    let osds = b.host.ceph_json(&["osd", "stat"]).await.ok()?["num_up_osds"].as_u64()? as u32;

    let tree = b.host.ceph_json(&["osd", "tree"]).await.ok()?;
    let osd_hosts = tree["nodes"]
        .as_array()?
        .iter()
        .filter(|n| {
            n["type"].as_str() == Some("host")
                && n["children"].as_array().is_some_and(|c| !c.is_empty())
        })
        .count() as u32;

    Some(Topology {
        nodes,
        osds,
        osd_hosts,
    })
}

async fn cluster_health<H: Host>(host: &H) -> Option<String> {
    host.ceph_json(&["health"]).await.ok()?["status"]
        .as_str()
        .map(str::to_string)
}

pub struct TopologyController;

impl crate::runtime::Controller for TopologyController {
    fn name(&self) -> &'static str {
        "topology"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[
            crate::runtime::Requirement::KubeApi,
            crate::runtime::Requirement::Ceph,
        ]
    }
    fn pauses_during(&self) -> &'static [crate::runtime::Activity] {
        &[crate::runtime::Activity::Restore]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        tick(&Backend::real().await?).await;
        Ok(crate::runtime::Tick::Done)
    }
}

async fn tick<H: Host>(b: &Backend<H>) {
    let Some(topo) = observe(b).await else {
        tracing::debug!("topology: cluster shape unknown this tick — not touching replication");
        return;
    };
    if topo.osds == 0 {
        return;
    }
    match cluster_health(&b.host).await.as_deref() {
        Some("HEALTH_ERR") | None => return,
        _ => {}
    }

    let policy = match read_policy_from(&b.host).await {
        None => {
            tracing::debug!("topology: storage policy unreadable this tick — changing nothing");
            return;
        }
        Some(PolicyState::NotChosen) => {
            return;
        }
        Some(PolicyState::Chosen(p)) => p,
    };
    let target = compute_target(&policy, &topo);

    apply_mon_mgr(&b.host, &target).await;
    apply_pools(&b.host, &target).await;
}

async fn apply_mon_mgr<H: Host>(host: &H, target: &Target) {
    let Ok(dump) = host.ceph_json(&["mon", "dump"]).await else {
        return;
    };
    let cur_mon = dump["mons"].as_array().map(|a| a.len()).unwrap_or(0) as u32;
    if cur_mon == target.mon {
        return;
    }

    if cur_mon < target.mon {
        tracing::info!(
            "topology: {cur_mon} mon(s) across {} node(s) — a machine has joined Kubernetes but \
             its Ceph has not yet; its yolab-ceph-bootstrap retry timer will pick it up",
            target.mon
        );
    } else {
        tracing::info!(
            "topology: {cur_mon} mon(s) for {} node(s) — a machine has left. Removal is \
             operator-driven (`ceph mon remove <name>`): an offline node must never be \
             auto-evicted from the quorum.",
            target.mon
        );
    }
}

fn apply_pools_selects(pool: &str) -> bool {
    !pool.is_empty() && !pool.starts_with(".nfs") && !pool.starts_with(".rgw")
}

async fn pool_size<H: Host>(host: &H, pool: &str) -> Option<u32> {
    host.ceph(&["osd", "pool", "get", pool, "size", "-f", "json"])
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["size"].as_u64())
        .map(|x| x as u32)
}

async fn apply_pools<H: Host>(host: &H, target: &Target) {
    let rule = if target.failure_domain == "osd" {
        "replicated_osd"
    } else {
        "replicated_rule"
    };

    if target.failure_domain == "osd" {
        let have = host
            .ceph(&["osd", "crush", "rule", "ls"])
            .await
            .unwrap_or_default();
        if !have.lines().any(|l| l.trim() == rule) {
            host.ceph(&[
                "osd",
                "crush",
                "rule",
                "create-replicated",
                rule,
                "default",
                "osd",
            ])
            .await
            .warn_on_err(format!("topology: create crush rule {rule}"));
        }
    }

    let pools = host.ceph(&["osd", "pool", "ls"]).await.unwrap_or_default();
    for pool in pools
        .lines()
        .map(|l| l.trim())
        .filter(|p| apply_pools_selects(p))
    {
        let Some(cur) = pool_size(host, pool).await else {
            tracing::debug!("topology: size of {pool} unknown this tick — leaving it");
            continue;
        };
        let want = target.size;
        let min = min_size_for(want);

        host.ceph(&["osd", "pool", "set", pool, "crush_rule", rule])
            .await
            .warn_on_err(format!("topology: set crush_rule on {pool}"));
        if want != cur {
            let ws = want.to_string();
            let res = if want == 1 {
                host.ceph(&[
                    "osd",
                    "pool",
                    "set",
                    pool,
                    "size",
                    &ws,
                    "--yes-i-really-mean-it",
                ])
                .await
            } else {
                host.ceph(&["osd", "pool", "set", pool, "size", &ws]).await
            };
            if res.is_ok() {
                tracing::info!(
                    "topology: pool {pool} size {cur}→{want} (fd={})",
                    target.failure_domain
                );
            }
        }
        let ms = min.to_string();
        host.ceph(&["osd", "pool", "set", pool, "min_size", &ms])
            .await
            .warn_on_err(format!("topology: set min_size on {pool}"));
    }
}

pub async fn get_policy(State(state): State<AppState>) -> crate::error::Result<Json<Value>> {
    let b = state.backend().await?;
    let chosen = match read_policy_from(&b.host).await {
        Some(PolicyState::Chosen(p)) => Some(p),
        _ => None,
    };
    let topo = observe(&b).await;
    let target = match (&chosen, &topo) {
        (Some(p), Some(t)) => Some(compute_target(p, t)),
        _ => None,
    };
    Ok(Json(serde_json::json!({
        "policy": chosen,
        "topology": topo,
        "target": target,
    })))
}

#[derive(Deserialize)]
pub struct SetPolicyReq {
    pub size: Option<u32>,
    pub failure_domain: Option<String>,
}

pub async fn set_policy(
    State(_s): State<AppState>,
    Json(req): Json<SetPolicyReq>,
) -> (StatusCode, Json<Value>) {
    let (Some(size), Some(failure_domain)) = (req.size, req.failure_domain.clone()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "size and failure_domain are both required"})),
        );
    };

    if size < 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "keep at least one copy"})),
        );
    }
    if failure_domain != "osd" && failure_domain != "host" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "failure_domain must be osd or host"})),
        );
    }
    let p = StoragePolicy {
        size,
        failure_domain,
    };
    match write_policy(&crate::host::RealHost, &p).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "policy": p})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(size: u32, fd: &str) -> StoragePolicy {
        StoragePolicy {
            size,
            failure_domain: fd.into(),
        }
    }

    fn topo(nodes: u32, osds: u32, osd_hosts: u32) -> Topology {
        Topology {
            nodes,
            osds,
            osd_hosts,
        }
    }

    #[test]
    fn the_chosen_size_is_what_comes_out() {
        for size in [1u32, 2, 3, 5, 9] {
            let t = compute_target(&policy(size, "host"), &topo(3, 6, 3));
            assert_eq!(t.size, size);
        }
    }

    #[test]
    fn the_chosen_failure_domain_is_what_comes_out() {
        for fd in ["osd", "host"] {
            let t = compute_target(&policy(2, fd), &topo(2, 4, 2));
            assert_eq!(t.failure_domain, fd);
        }
    }

    #[test]
    fn more_copies_than_disks_is_kept_not_clamped() {
        let t = compute_target(&policy(3, "osd"), &topo(1, 1, 1));
        assert_eq!(t.size, 3, "the owner's number survives a small cluster");
    }

    #[test]
    fn more_copies_than_machines_is_kept_not_clamped() {
        let t = compute_target(&policy(3, "host"), &topo(1, 4, 1));
        assert_eq!(t.size, 3);
    }

    #[test]
    fn losing_disks_cannot_change_how_many_copies_are_kept() {
        let p = policy(3, "host");
        let healthy = compute_target(&p, &topo(3, 3, 3));
        let one_gone = compute_target(&p, &topo(3, 2, 2));
        let two_gone = compute_target(&p, &topo(3, 1, 1));
        let all_gone = compute_target(&p, &topo(0, 0, 0));

        assert_eq!(healthy.size, 3);
        assert_eq!(
            one_gone.size, 3,
            "a disconnected disk is not a smaller cluster"
        );
        assert_eq!(two_gone.size, 3);
        assert_eq!(
            all_gone.size, 3,
            "even an unreadable cluster keeps the promise"
        );
    }

    #[test]
    fn a_single_copy_still_accepts_writes_because_refusing_would_mean_no_storage_at_all() {
        assert_eq!(min_size_for(1), 1);
        let t = compute_target(&policy(1, "osd"), &topo(1, 1, 1));
        assert_eq!(t.min_size, 1);
    }

    #[test]
    fn replicated_pools_stop_accepting_writes_before_the_last_copy_is_left() {
        for size in [2u32, 3, 7] {
            assert_eq!(
                min_size_for(size),
                size - 1,
                "size={size}: a pool that keeps taking writes down to one surviving \
                 copy loses acknowledged data the moment that copy dies, which is \
                 exactly what the Storage page promises it will not do"
            );
        }
    }

    #[test]
    fn min_size_never_exceeds_size() {
        for size in [1u32, 2, 3, 7] {
            for fd in ["osd", "host"] {
                let t = compute_target(&policy(size, fd), &topo(3, 6, 3));
                assert!(t.min_size <= t.size, "size={size} fd={fd}");
                assert!(t.min_size >= 1, "size={size} fd={fd}");
            }
        }
    }

    #[test]
    fn one_mon_and_mgr_per_machine() {
        assert_eq!(compute_target(&policy(2, "host"), &topo(3, 3, 3)).mon, 3);
        assert_eq!(compute_target(&policy(2, "host"), &topo(3, 3, 3)).mgr, 3);
    }

    #[test]
    fn an_unreadable_cluster_still_reports_at_least_one_mon() {
        let t = compute_target(&policy(1, "osd"), &topo(0, 0, 0));
        assert_eq!((t.mon, t.mgr), (1, 1));
    }

    #[test]
    fn every_data_pool_including_images_follows_the_chosen_size() {
        for p in ["images", "yolab-fs-data0", "yolab-fs-metadata"] {
            assert!(apply_pools_selects(p), "{p} must follow the chosen size");
        }
    }

    #[test]
    fn cephs_own_pools_are_left_alone() {
        for p in [".rgw.root", ".nfs"] {
            assert!(!apply_pools_selects(p), "{p} must not be resized");
        }
    }

    #[test]
    fn the_current_page_body_parses() {
        let req: SetPolicyReq =
            serde_json::from_str(r#"{"size":2,"failure_domain":"osd"}"#).unwrap();
        assert_eq!(req.size, Some(2));
        assert_eq!(req.failure_domain.as_deref(), Some("osd"));
    }

    #[tokio::test]
    async fn a_policy_is_chosen_not_chosen_or_unreadable_and_never_defaulted() {
        use crate::host::fake::FakeHost;

        let chosen = FakeHost::new().ok(
            "ceph config-key get yolab/storage-policy",
            r#"{"size":2,"failure_domain":"host"}"#,
        );
        match merge_policy(read_mirror_policy(&chosen).await, None, false) {
            Some(PolicyState::Chosen(p)) => assert_eq!(p, policy(2, "host")),
            _ => panic!("expected a chosen policy"),
        }

        let fresh = FakeHost::new().fail(
            "ceph config-key get yolab/storage-policy",
            "Error ENOENT: key 'yolab/storage-policy' doesn't exist",
        );
        assert!(matches!(
            merge_policy(read_mirror_policy(&fresh).await, None, false),
            Some(PolicyState::NotChosen)
        ));

        let down = FakeHost::new().fail(
            "ceph config-key get yolab/storage-policy",
            "error connecting to the cluster",
        );
        assert!(merge_policy(read_mirror_policy(&down).await, None, false).is_none());

        let junk = FakeHost::new().ok(
            "ceph config-key get yolab/storage-policy",
            r#"{"size":"two"}"#,
        );
        assert!(
            merge_policy(read_mirror_policy(&junk).await, None, false).is_none(),
            "unreadable is not 'not chosen'"
        );
    }

    #[tokio::test]
    async fn a_chosen_policy_is_stored_in_ceph() {
        use crate::host::fake::FakeHost;
        let host = FakeHost::new().ok("ceph config-key set yolab/storage-policy", "");
        write_policy_to_cluster(&host, &policy(3, "osd"))
            .await
            .unwrap();
        assert!(host
            .ran(r#"ceph config-key set yolab/storage-policy {"size":3,"failure_domain":"osd"}"#));
    }

    #[test]
    fn a_policy_in_the_store_is_used_even_when_the_cluster_cannot_be_read() {
        match merge_policy(None, Some(policy(3, "host")), true) {
            Some(PolicyState::Chosen(p)) => assert_eq!(p, policy(3, "host")),
            _ => panic!("a stored choice is still a choice when ceph is down"),
        }
    }

    #[test]
    fn an_unreadable_cluster_with_a_seeded_store_means_nobody_has_chosen_yet() {
        assert!(matches!(
            merge_policy(None, None, true),
            Some(PolicyState::NotChosen)
        ));
    }

    #[test]
    fn an_unreadable_cluster_with_an_unseeded_store_stays_unknown() {
        assert!(
            merge_policy(None, None, false).is_none(),
            "never report 'not chosen' from a store that has never seen the cluster"
        );
    }

    #[test]
    fn the_store_wins_over_a_stale_cluster_copy() {
        match merge_policy(Some(Some(policy(2, "osd"))), Some(policy(3, "host")), true) {
            Some(PolicyState::Chosen(p)) => assert_eq!(p, policy(3, "host")),
            _ => panic!("expected the stored policy"),
        }
    }

    #[test]
    fn taking_the_cluster_policy_in_records_it_and_marks_the_store_seeded() {
        let mut store = Store::new("node1");
        let cluster = policy(2, "host");

        let synced = sync_policy(&mut store, Some(Some(&cluster)));

        assert_eq!(synced.stored, Some(cluster));
        assert!(synced.seeded);
        assert!(synced.changed);
    }

    #[test]
    fn a_cluster_with_no_policy_still_marks_the_store_seeded() {
        let mut store = Store::new("node1");
        let synced = sync_policy(&mut store, Some(None));
        assert_eq!(synced.stored, None);
        assert!(
            synced.seeded,
            "having read the cluster and found nothing is still having read it"
        );
    }

    #[test]
    fn an_unreadable_cluster_does_not_mark_the_store_seeded() {
        let mut store = Store::new("node1");
        let synced = sync_policy(&mut store, None);
        assert!(!synced.seeded);
        assert!(!synced.changed);
    }

    #[test]
    fn taking_the_cluster_policy_in_does_not_undo_one_chosen_since() {
        let mut store = Store::new("node1");
        store.mark_policy_seeded().unwrap();
        store.set_storage_policy(&policy(3, "host")).unwrap();

        let synced = sync_policy(&mut store, Some(Some(&policy(2, "osd"))));

        assert_eq!(synced.stored, Some(policy(3, "host")));
        assert!(!synced.changed, "nothing to write means nothing to save");
    }

    mod against_the_cluster {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, list};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        fn target(size: u32, fd: &str) -> Target {
            compute_target(
                &policy(size, fd),
                &Topology {
                    nodes: 2,
                    osds: 2,
                    osd_hosts: 2,
                },
            )
        }

        #[tokio::test]
        async fn a_pool_is_resized_to_the_chosen_copies_and_its_floor() {
            let host = FakeHost::new()
                .ok("ceph osd pool ls", "images\n.nfs\n")
                .ok("ceph osd pool get images size", r#"{"size": 1}"#)
                .ok("ceph osd pool set", "");
            apply_pools(&host, &target(2, "host")).await;
            assert!(host.ran("ceph osd pool set images size 2"));
            assert!(host.ran("ceph osd pool set images min_size 1"));
            assert!(host.ran("ceph osd pool set images crush_rule replicated_rule"));
            assert!(!host.ran(".nfs"), "{:?}", host.calls());
        }

        #[tokio::test]
        async fn one_copy_needs_ceph_to_be_told_it_is_meant() {
            let host = FakeHost::new()
                .ok("ceph osd pool ls", "images\n")
                .ok("ceph osd pool get images size", r#"{"size": 2}"#)
                .ok("ceph osd crush rule ls", "replicated_rule\n")
                .ok("ceph osd crush rule create-replicated", "")
                .ok("ceph osd pool set", "");
            apply_pools(&host, &target(1, "osd")).await;
            assert!(host.ran("ceph osd crush rule create-replicated replicated_osd default osd"));
            assert!(host.ran("ceph osd pool set images size 1 --yes-i-really-mean-it"));
        }

        #[tokio::test]
        async fn a_pool_of_unknown_size_is_left_alone() {
            let host = FakeHost::new()
                .ok("ceph osd pool ls", "images\n")
                .fail("ceph osd pool get images size", "timed out");
            apply_pools(&host, &target(3, "host")).await;
            assert!(!host.ran("ceph osd pool set"));
        }

        #[tokio::test]
        async fn the_shape_counts_nodes_osds_and_hosts_that_hold_osds() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/nodes"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Node",
                    vec![
                        serde_json::json!({ "metadata": { "name": "node1" } }),
                        serde_json::json!({ "metadata": { "name": "node2" } }),
                    ],
                )))
                .mount(&server)
                .await;
            let host = FakeHost::new()
                .ok("ceph osd stat", r#"{"num_up_osds": 3}"#)
                .ok(
                    "ceph osd tree",
                    r#"{"nodes": [
                        {"type": "root", "children": [-2, -3]},
                        {"type": "host", "children": [0, 1]},
                        {"type": "host", "children": []}
                    ]}"#,
                );
            let topo = observe(&Backend { kube, host }).await.unwrap();
            assert_eq!(
                topo,
                Topology {
                    nodes: 2,
                    osds: 3,
                    osd_hosts: 1
                }
            );
        }

        #[tokio::test]
        async fn nothing_is_changed_while_the_cluster_is_in_error() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/nodes"))
                .respond_with(ResponseTemplate::new(200).set_body_json(list(
                    "Node",
                    vec![serde_json::json!({ "metadata": { "name": "node1" } })],
                )))
                .mount(&server)
                .await;
            let host = FakeHost::new()
                .ok("ceph osd stat", r#"{"num_up_osds": 1}"#)
                .ok("ceph osd tree", r#"{"nodes": []}"#)
                .ok("ceph health", r#"{"status": "HEALTH_ERR"}"#);
            let b = Backend { kube, host };
            tick(&b).await;
            assert!(!b.host.ran("ceph osd pool"), "{:?}", b.host.calls());
        }
    }
}
