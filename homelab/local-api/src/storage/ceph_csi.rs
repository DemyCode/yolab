use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use kube::Client;
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};

use super::csi_secrets::ROOK_ONLY_LEFTOVERS;

pub const MANIFESTS_ENV: &str = "YOLAB_CEPH_CSI_MANIFESTS";

const NS: &str = "rook-ceph";
pub const WITHOUT_ROOK_LABEL: &str = "yolab.io/without-rook";
const ROOK_RELEASE: &str = "rook-ceph";
const ROOK_OPERATOR: &str = "rook-ceph-operator";
const ROOK_MANIFESTS: &[&str] = &[
    "var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml",
    "var/lib/rancher/k3s/server/manifests/rook-ceph-external.yaml",
];
const CSI_WORKLOADS: &[(&str, &str)] = &[
    ("DaemonSet", "csi-cephfsplugin"),
    ("Deployment", "csi-cephfsplugin-provisioner"),
    ("DaemonSet", "csi-rbdplugin"),
    ("Deployment", "csi-rbdplugin-provisioner"),
];
pub const CEPHCSI_SPEAKING_AES256K: (u32, u32, u32) = (3, 17, 1);

fn ships_rook(root: &Path) -> bool {
    ROOK_MANIFESTS.iter().any(|m| root.join(m).exists())
}

fn node_label_patch(node: &str, without_rook: bool) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "name": node,
            "labels": { WITHOUT_ROOK_LABEL: if without_rook { json!("true") } else { Value::Null } },
        },
    })
}

pub async fn mark_node(client: &Client, root: &Path, node: &str) -> Result<Tick> {
    let without = !ships_rook(root);
    crate::k8s::merge_patch(client, &node_label_patch(node, without)).await?;
    Ok(if without {
        Tick::Done
    } else {
        Tick::Idle("k3s on this machine still ships the Rook manifests".into())
    })
}

fn still_shipping_rook(nodes: &[Value]) -> Option<String> {
    if nodes.is_empty() {
        return Some("no machine".into());
    }
    nodes
        .iter()
        .find(|n| n["metadata"]["labels"][WITHOUT_ROOK_LABEL].as_str() != Some("true"))
        .map(|n| n["metadata"]["name"].as_str().unwrap_or("a machine").to_string())
}

fn ours(object: &Value) -> bool {
    object["metadata"]["labels"]["app.kubernetes.io/managed-by"].as_str() == Some("yolab")
}

fn released_by_rook(object: &Value) -> bool {
    object["metadata"]["annotations"]["meta.helm.sh/release-name"].as_str() == Some(ROOK_RELEASE)
}

async fn list_or_empty(
    client: &Client,
    api_version: &str,
    kind: &str,
    namespace: Option<&str>,
) -> Result<Vec<Value>> {
    match crate::k8s::list(client, api_version, kind, namespace, &Default::default()).await {
        Ok(items) => Ok(items),
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

async fn retire_rook(client: &Client) -> Result<Option<String>> {
    let chart = crate::k8s::reference("helm.cattle.io/v1", "HelmChart", "kube-system", ROOK_RELEASE);
    if crate::k8s::exists(client, &chart).await? {
        crate::k8s::delete_if_present(client, &chart).await?;
        return Ok(Some("uninstalling the Rook operator chart".into()));
    }

    let operator = crate::k8s::reference("apps/v1", "Deployment", NS, ROOK_OPERATOR);
    if crate::k8s::exists(client, &operator).await? {
        crate::k8s::delete_if_present(client, &operator).await?;
        return Ok(Some("removing the Rook operator".into()));
    }

    let mut rooks_csi = false;
    for (kind, name) in CSI_WORKLOADS {
        let workload = crate::k8s::reference("apps/v1", kind, NS, name);
        if let Some(found) = crate::k8s::get(client, &workload).await? {
            if !ours(&found) {
                crate::k8s::delete_if_present(client, &workload).await?;
                rooks_csi = true;
            }
        }
    }
    if rooks_csi {
        return Ok(Some("waiting for Rook's CSI pods to go".into()));
    }

    let clusters = list_or_empty(client, "ceph.rook.io/v1", "CephCluster", Some(NS)).await?;
    for cluster in &clusters {
        let name = cluster["metadata"]["name"].as_str().unwrap_or_default();
        let unpinned = json!({
            "apiVersion": "ceph.rook.io/v1",
            "kind": "CephCluster",
            "metadata": { "name": name, "namespace": NS, "finalizers": null },
        });
        crate::k8s::merge_patch(client, &unpinned).await?;
        crate::k8s::delete_if_present(client, &unpinned).await?;
    }
    if !clusters.is_empty() {
        return Ok(Some("removing Rook's CephCluster record".into()));
    }

    let crds = list_or_empty(client, "apiextensions.k8s.io/v1", "CustomResourceDefinition", None)
        .await?
        .into_iter()
        .filter(released_by_rook)
        .collect::<Vec<_>>();
    for crd in &crds {
        let name = crd["metadata"]["name"].as_str().unwrap_or_default();
        crate::k8s::delete_if_present(
            client,
            &crate::k8s::cluster_reference("apiextensions.k8s.io/v1", "CustomResourceDefinition", name),
        )
        .await?;
    }
    if !crds.is_empty() {
        return Ok(Some("removing Rook's resource definitions".into()));
    }

    for (kind, name) in ROOK_ONLY_LEFTOVERS {
        crate::k8s::delete_if_present(client, &crate::k8s::reference("v1", kind, NS, name))
            .await
            .warn_on_err(format!("ceph-csi: remove Rook's leftover {kind} {name}"));
    }
    Ok(None)
}

pub async fn converge(client: &Client) -> Result<Tick> {
    let nodes = crate::k8s::nodes(client).await?;
    if let Some(lagging) = still_shipping_rook(&nodes) {
        return Ok(Tick::NotYet(format!(
            "{lagging} still ships the Rook manifests; the CSI driver changes hands once every machine runs this version"
        )));
    }
    if let Some(step) = retire_rook(client).await? {
        return Ok(Tick::NotYet(step));
    }
    let path = std::env::var(MANIFESTS_ENV)
        .map_err(|_| anyhow::anyhow!("{MANIFESTS_ENV} is not set; the NixOS module names the CSI manifests"))?;
    let manifests = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read the CSI manifests at {path}: {e}"))?;
    crate::k8s::apply_documents(client, &manifests).await?;
    Ok(Tick::Done)
}

pub fn cephcsi_version(image: &str) -> Option<(u32, u32, u32)> {
    let without_digest = image.split('@').next()?;
    let (_, tag) = without_digest.rsplit_once(':')?;
    let mut parts = tag.trim_start_matches('v').splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

fn runs_cephcsi_speaking_aes256k(workload: &Value) -> bool {
    let containers = workload["spec"]["template"]["spec"]["containers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let plugins: Vec<&Value> = containers
        .iter()
        .filter(|c| c["name"] == "csi-cephfsplugin")
        .collect();
    !plugins.is_empty()
        && plugins.iter().all(|c| {
            c["image"]
                .as_str()
                .and_then(cephcsi_version)
                .is_some_and(|v| v >= CEPHCSI_SPEAKING_AES256K)
        })
}

fn observed(workload: &Value) -> bool {
    let generation = workload["metadata"]["generation"].as_i64().unwrap_or(0);
    workload["status"]["observedGeneration"].as_i64().unwrap_or(-1) >= generation
}

fn daemonset_rolled_out(ds: &Value) -> bool {
    let s = &ds["status"];
    let desired = s["desiredNumberScheduled"].as_i64().unwrap_or(0);
    observed(ds)
        && desired > 0
        && s["updatedNumberScheduled"].as_i64() == Some(desired)
        && s["numberReady"].as_i64() == Some(desired)
}

fn deployment_rolled_out(deploy: &Value) -> bool {
    let s = &deploy["status"];
    let want = deploy["spec"]["replicas"].as_i64().unwrap_or(1);
    observed(deploy)
        && want > 0
        && s["updatedReplicas"].as_i64() == Some(want)
        && s["readyReplicas"].as_i64() == Some(want)
}

pub fn csi_speaks_aes256k(ds: &Value, deploy: &Value) -> bool {
    [ds, deploy]
        .iter()
        .all(|w| ours(w) && runs_cephcsi_speaking_aes256k(w))
        && daemonset_rolled_out(ds)
        && deployment_rolled_out(deploy)
}

pub async fn speaks_aes256k(client: &Client) -> Result<bool> {
    let operator = crate::k8s::reference("apps/v1", "Deployment", NS, ROOK_OPERATOR);
    if crate::k8s::exists(client, &operator).await? {
        return Ok(false);
    }
    let ds = crate::k8s::get(
        client,
        &crate::k8s::reference("apps/v1", "DaemonSet", NS, "csi-cephfsplugin"),
    )
    .await?;
    let deploy = crate::k8s::get(
        client,
        &crate::k8s::reference("apps/v1", "Deployment", NS, "csi-cephfsplugin-provisioner"),
    )
    .await?;
    Ok(match (ds, deploy) {
        (Some(ds), Some(deploy)) => csi_speaks_aes256k(&ds, &deploy),
        _ => false,
    })
}

pub struct RookManifestsGoneController;

impl Controller for RookManifestsGoneController {
    fn name(&self) -> &'static str {
        "rook-manifests-gone"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let client = crate::k8s::client().await?;
        mark_node(&client, Path::new("/"), &ctx.node).await
    }
}

pub struct CephCsiController;

impl Controller for CephCsiController {
    fn name(&self) -> &'static str {
        "ceph-csi"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let client = crate::k8s::client().await?;
        converge(&client).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[test]
    fn cephcsi_versions_are_read_from_tag_and_digest_pinned_images() {
        assert_eq!(
            cephcsi_version("quay.io/cephcsi/cephcsi:v3.17.1@sha256:abc"),
            Some((3, 17, 1))
        );
        assert_eq!(cephcsi_version("quay.io/cephcsi/cephcsi:v3.13.1"), Some((3, 13, 1)));
        assert_eq!(cephcsi_version("quay.io/cephcsi/cephcsi:v3.18.0-rc1"), Some((3, 18, 0)));
        assert_eq!(cephcsi_version("quay.io/cephcsi/cephcsi"), None);
        assert_eq!(cephcsi_version("localhost:5000/cephcsi"), None);
    }

    fn workload(ours_label: bool, image: &str) -> Value {
        let labels = if ours_label {
            json!({"app.kubernetes.io/managed-by": "yolab"})
        } else {
            json!({})
        };
        json!({
            "metadata": {"generation": 3, "labels": labels},
            "spec": {"replicas": 1, "template": {"spec": {"containers": [
                {"name": "driver-registrar", "image": "registry.k8s.io/x:v1"},
                {"name": "csi-cephfsplugin", "image": image},
            ]}}},
            "status": {
                "observedGeneration": 3,
                "desiredNumberScheduled": 2, "updatedNumberScheduled": 2, "numberReady": 2,
                "updatedReplicas": 1, "readyReplicas": 1,
            },
        })
    }

    const NEW: &str = "quay.io/cephcsi/cephcsi:v3.17.1@sha256:abc";
    const OLD: &str = "quay.io/cephcsi/cephcsi:v3.13.1";

    #[test]
    fn csi_speaks_aes256k_only_when_our_new_driver_has_fully_rolled_out() {
        assert!(csi_speaks_aes256k(&workload(true, NEW), &workload(true, NEW)));
        assert!(!csi_speaks_aes256k(&workload(true, OLD), &workload(true, NEW)));
        assert!(!csi_speaks_aes256k(&workload(false, NEW), &workload(true, NEW)));

        let mut rolling = workload(true, NEW);
        rolling["status"]["updatedNumberScheduled"] = json!(1);
        assert!(!csi_speaks_aes256k(&rolling, &workload(true, NEW)));

        let mut stale = workload(true, NEW);
        stale["status"]["observedGeneration"] = json!(2);
        assert!(!csi_speaks_aes256k(&workload(true, NEW), &stale));

        let mut unready = workload(true, NEW);
        unready["status"]["readyReplicas"] = json!(0);
        assert!(!csi_speaks_aes256k(&workload(true, NEW), &unready));
    }

    #[test]
    fn a_machine_that_still_ships_rook_holds_the_cutover() {
        let node = |name: &str, label: Option<&str>| {
            let labels = match label {
                Some(v) => json!({ WITHOUT_ROOK_LABEL: v }),
                None => json!({}),
            };
            json!({"metadata": {"name": name, "labels": labels}})
        };
        assert_eq!(
            still_shipping_rook(&[node("n1", Some("true")), node("n2", None)]),
            Some("n2".into())
        );
        assert_eq!(
            still_shipping_rook(&[node("n1", Some("true")), node("n2", Some("true"))]),
            None
        );
        assert!(still_shipping_rook(&[]).is_some());
    }

    #[tokio::test]
    async fn a_machine_whose_k3s_still_has_a_rook_manifest_is_not_marked() {
        let root = tempfile::tempdir().unwrap();
        let manifests = root.path().join("var/lib/rancher/k3s/server/manifests");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(manifests.join("rook-ceph-operator.yaml"), "x").unwrap();
        let (server, client) = api_server().await;
        accept_patches(&server).await;
        let tick = mark_node(&client, root.path(), "n1").await.unwrap();
        assert!(matches!(tick, Tick::Idle(_)));
        let sent = patched(&server).await;
        assert!(sent[0]["metadata"]["labels"][WITHOUT_ROOK_LABEL].is_null());
    }

    #[tokio::test]
    async fn a_machine_without_rook_manifests_is_marked() {
        let root = tempfile::tempdir().unwrap();
        let (server, client) = api_server().await;
        accept_patches(&server).await;
        let tick = mark_node(&client, root.path(), "n1").await.unwrap();
        assert_eq!(tick, Tick::Done);
        assert_eq!(patched(&server).await[0]["metadata"]["labels"][WITHOUT_ROOK_LABEL], "true");
    }

    async fn all_nodes_marked(server: &wiremock::MockServer) {
        serve(
            server,
            "/api/v1/nodes",
            200,
            json!({"kind": "NodeList", "apiVersion": "v1", "metadata": {}, "items": [
                {"metadata": {"name": "n1", "labels": { WITHOUT_ROOK_LABEL: "true" }}},
            ]}),
        )
        .await;
    }

    async fn absent(server: &wiremock::MockServer, p: &str) {
        serve(server, p, 404, status(404, "NotFound")).await;
    }

    const CHART: &str = "/apis/helm.cattle.io/v1/namespaces/kube-system/helmcharts/rook-ceph";
    const OPERATOR: &str = "/apis/apps/v1/namespaces/rook-ceph/deployments/rook-ceph-operator";

    #[tokio::test]
    async fn nothing_changes_hands_while_any_machine_still_ships_rook() {
        let (server, client) = api_server().await;
        serve(
            &server,
            "/api/v1/nodes",
            200,
            json!({"kind": "NodeList", "apiVersion": "v1", "metadata": {}, "items": [
                {"metadata": {"name": "n1", "labels": { WITHOUT_ROOK_LABEL: "true" }}},
                {"metadata": {"name": "n2", "labels": {}}},
            ]}),
        )
        .await;
        let tick = converge(&client).await.unwrap();
        assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("n2")));
        let mutated = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method.as_str() != "GET");
        assert!(!mutated);
    }

    #[tokio::test]
    async fn the_rook_chart_is_uninstalled_first_and_nothing_of_ours_is_applied_yet() {
        let (server, client) = api_server().await;
        all_nodes_marked(&server).await;
        serve(&server, CHART, 200, json!({"metadata": {"name": "rook-ceph"}})).await;
        Mock::given(method("DELETE"))
            .and(path(CHART))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let tick = converge(&client).await.unwrap();
        assert!(matches!(tick, Tick::NotYet(_)));
        assert!(patched(&server).await.is_empty());
    }

    #[tokio::test]
    async fn rooks_own_csi_pods_are_removed_before_ours_take_the_driver_socket() {
        let (server, client) = api_server().await;
        all_nodes_marked(&server).await;
        absent(&server, CHART).await;
        absent(&server, OPERATOR).await;
        let ds = "/apis/apps/v1/namespaces/rook-ceph/daemonsets/csi-cephfsplugin";
        serve(&server, ds, 200, json!({"metadata": {"name": "csi-cephfsplugin", "labels": {}}})).await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/deployments/csi-cephfsplugin-provisioner").await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/daemonsets/csi-rbdplugin").await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/deployments/csi-rbdplugin-provisioner").await;
        Mock::given(method("DELETE"))
            .and(path(ds))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let tick = converge(&client).await.unwrap();
        assert!(matches!(tick, Tick::NotYet(ref why) if why.contains("CSI")));
        assert!(patched(&server).await.is_empty());
    }

    #[tokio::test]
    async fn our_own_csi_pods_are_never_deleted_as_if_they_were_rooks() {
        let (server, client) = api_server().await;
        all_nodes_marked(&server).await;
        absent(&server, CHART).await;
        absent(&server, OPERATOR).await;
        let ds = "/apis/apps/v1/namespaces/rook-ceph/daemonsets/csi-cephfsplugin";
        serve(
            &server,
            ds,
            200,
            json!({"metadata": {"name": "csi-cephfsplugin", "labels": {"app.kubernetes.io/managed-by": "yolab"}}}),
        )
        .await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/deployments/csi-cephfsplugin-provisioner").await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/daemonsets/csi-rbdplugin").await;
        absent(&server, "/apis/apps/v1/namespaces/rook-ceph/deployments/csi-rbdplugin-provisioner").await;
        let _ = converge(&client).await;
        let deleted_ds = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method.as_str() == "DELETE" && r.url.path() == ds);
        assert!(!deleted_ds);
    }
}
