use serde_json::Value;

use crate::k8s::{cluster_reference, reference};
use crate::runtime::{Ctx, Resource, Scope, Tick, KUBE_API};

pub const NAME: &str = "volsync-removal";

const API: &str = "volsync.backube/v1alpha1";
const KINDS: [&str; 2] = ["ReplicationSource", "ReplicationDestination"];
const CRDS: [&str; 2] = [
    "replicationsources.volsync.backube",
    "replicationdestinations.volsync.backube",
];
const NAMESPACE: &str = "volsync-system";
const CACHE_PREFIXES: [&str; 2] = ["volsync-src-", "volsync-dst-"];

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Found {
    pub replications: Vec<Value>,
    pub caches: Vec<Value>,
    pub chart: bool,
    pub crds: Vec<&'static str>,
    pub namespace: bool,
}

pub(crate) fn is_cache(pvc: &Value) -> bool {
    pvc["metadata"]["name"]
        .as_str()
        .is_some_and(|n| CACHE_PREFIXES.iter().any(|p| n.starts_with(p)))
}

fn target(object: &Value) -> Value {
    reference(
        object["apiVersion"].as_str().unwrap_or_default(),
        object["kind"].as_str().unwrap_or_default(),
        object["metadata"]["namespace"].as_str().unwrap_or_default(),
        object["metadata"]["name"].as_str().unwrap_or_default(),
    )
}

pub(crate) fn chart() -> Value {
    reference("helm.cattle.io/v1", "HelmChart", "kube-system", "volsync")
}

pub(crate) fn next_step(found: &Found) -> Vec<Value> {
    if !found.replications.is_empty() || !found.caches.is_empty() {
        return found
            .replications
            .iter()
            .chain(&found.caches)
            .map(target)
            .collect();
    }
    if found.chart {
        return vec![chart()];
    }
    let mut last: Vec<Value> = found
        .crds
        .iter()
        .map(|c| cluster_reference("apiextensions.k8s.io/v1", "CustomResourceDefinition", c))
        .collect();
    if found.namespace {
        last.push(cluster_reference("v1", "Namespace", NAMESPACE));
    }
    last
}

async fn listed(client: &kube::Client, api: &str, kind: &str) -> anyhow::Result<Vec<Value>> {
    match crate::k8s::list(client, api, kind, None, &Default::default()).await {
        Ok(found) => Ok(found),
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

async fn survey(client: &kube::Client) -> anyhow::Result<Found> {
    let mut found = Found::default();
    for kind in KINDS {
        found.replications.extend(listed(client, API, kind).await?);
    }
    found.caches = crate::k8s::list(
        client,
        "v1",
        "PersistentVolumeClaim",
        None,
        &Default::default(),
    )
    .await?
    .into_iter()
    .filter(is_cache)
    .collect();
    found.chart = crate::k8s::exists(client, &chart()).await?;
    for crd in CRDS {
        let r = cluster_reference("apiextensions.k8s.io/v1", "CustomResourceDefinition", crd);
        if crate::k8s::exists(client, &r).await? {
            found.crds.push(crd);
        }
    }
    found.namespace =
        crate::k8s::exists(client, &cluster_reference("v1", "Namespace", NAMESPACE)).await?;
    Ok(found)
}

pub struct VolsyncRemovalController;

impl Resource for VolsyncRemovalController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[KUBE_API]
    }
    async fn converge(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        let client = ctx.tools.kube().await?;
        let step = next_step(&survey(&client).await?);
        if step.is_empty() {
            return Ok(Tick::Idle("VolSync is not installed".into()));
        }
        for object in &step {
            crate::k8s::delete_if_present(&client, object).await?;
            tracing::info!(
                "volsync removal: deleted {} {}/{}",
                object["kind"].as_str().unwrap_or_default(),
                object["metadata"]["namespace"].as_str().unwrap_or("-"),
                object["metadata"]["name"].as_str().unwrap_or_default(),
            );
        }
        Ok(Tick::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn replication(ns: &str, name: &str) -> Value {
        json!({"apiVersion": API, "kind": "ReplicationSource",
               "metadata": {"namespace": ns, "name": name}})
    }

    fn pvc(ns: &str, name: &str) -> Value {
        json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
               "metadata": {"namespace": ns, "name": name}})
    }

    fn names(step: &[Value]) -> Vec<String> {
        step.iter()
            .map(|o| {
                format!(
                    "{}/{}",
                    o["kind"].as_str().unwrap(),
                    o["metadata"]["name"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn everything() -> Found {
        Found {
            replications: vec![replication("yolab-memos-44vc", "volsync-memos-44vc-data")],
            caches: vec![pvc(
                "yolab-memos-44vc",
                "volsync-src-volsync-memos-44vc-data-cache",
            )],
            chart: true,
            crds: CRDS.to_vec(),
            namespace: true,
        }
    }

    #[test]
    fn the_backup_schedules_and_their_caches_go_first_while_the_operator_still_runs() {
        assert_eq!(
            names(&next_step(&everything())),
            [
                "ReplicationSource/volsync-memos-44vc-data",
                "PersistentVolumeClaim/volsync-src-volsync-memos-44vc-data-cache"
            ]
        );
    }

    #[test]
    fn the_operator_is_uninstalled_once_nothing_of_it_is_left_to_clean() {
        let found = Found {
            replications: vec![],
            caches: vec![],
            ..everything()
        };
        assert_eq!(names(&next_step(&found)), ["HelmChart/volsync"]);
    }

    #[test]
    fn its_types_and_namespace_go_last() {
        let found = Found {
            replications: vec![],
            caches: vec![],
            chart: false,
            ..everything()
        };
        assert_eq!(
            names(&next_step(&found)),
            [
                "CustomResourceDefinition/replicationsources.volsync.backube",
                "CustomResourceDefinition/replicationdestinations.volsync.backube",
                "Namespace/volsync-system"
            ]
        );
    }

    #[test]
    fn a_cluster_without_volsync_has_nothing_to_do() {
        assert!(next_step(&Found::default()).is_empty());
    }

    #[test]
    fn only_volsync_caches_are_taken_for_caches() {
        assert!(is_cache(&pvc(
            "yolab-a",
            "volsync-src-volsync-a-data-cache"
        )));
        assert!(is_cache(&pvc(
            "yolab-a",
            "volsync-dst-volsync-a-data-cache"
        )));
        assert!(!is_cache(&pvc("yolab-a", "a-data")));
        assert!(!is_cache(&pvc("yolab-volsync", "volsync-notes")));
    }

    #[test]
    fn the_helm_chart_k3s_keeps_reinstalling_is_the_one_removed() {
        let c = chart();
        assert_eq!(c["apiVersion"], "helm.cattle.io/v1");
        assert_eq!(c["metadata"]["namespace"], "kube-system");
        assert_eq!(c["metadata"]["name"], "volsync");
    }
}
