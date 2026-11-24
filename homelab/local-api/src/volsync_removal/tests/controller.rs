use super::*;
use crate::k8s::testing::{api_server, list, serve, status};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SOURCES: &str = "/apis/volsync.backube/v1alpha1/replicationsources";
const CLAIMS: &str = "/api/v1/persistentvolumeclaims";

fn ctx(kube: kube::Client) -> Ctx {
    Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools {
            kube: crate::k8s::Kube::with(kube),
            ..crate::tools::Tools::unreachable()
        },
    }
}

async fn otherwise_absent(server: &MockServer) {
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status(200, "Success")))
        .mount(server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
        .mount(server)
        .await;
}

fn deleted(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .map(|r| r.url.path().to_string())
        .collect()
}

#[tokio::test]
async fn the_removal_runs_on_the_leader_every_ten_minutes_once_the_api_answers() {
    let c = VolsyncRemovalController;
    assert_eq!(
        (c.name(), c.scope(), c.interval(), c.depends_on()),
        (
            NAME,
            Scope::Cluster,
            std::time::Duration::from_secs(600),
            &[KUBE_API][..]
        )
    );
    let none = Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools::real(),
    };
    assert!(c.converge(&none).await.is_err());
}

#[tokio::test]
async fn a_cluster_without_volsync_has_nothing_to_remove() {
    let (server, kube) = api_server().await;
    serve(
        &server,
        CLAIMS,
        200,
        list(
            "PersistentVolumeClaim",
            vec![serde_json::json!({ "metadata": { "name": "data" } })],
        ),
    )
    .await;
    otherwise_absent(&server).await;
    assert_eq!(
        VolsyncRemovalController.converge(&ctx(kube)).await.unwrap(),
        Tick::Idle("VolSync is not installed".into())
    );
}

#[tokio::test]
async fn replications_go_first_then_the_chart_while_crds_and_namespace_wait() {
    let (server, kube) = api_server().await;
    serve(
        &server,
        SOURCES,
        200,
        list(
            "ReplicationSource",
            vec![serde_json::json!({
                "apiVersion": API, "kind": "ReplicationSource",
                "metadata": { "namespace": "yolab-notes", "name": "volsync-notes-data" },
            })],
        ),
    )
    .await;
    serve(&server, CLAIMS, 200, list("PersistentVolumeClaim", vec![])).await;
    otherwise_absent(&server).await;
    assert_eq!(
        VolsyncRemovalController.converge(&ctx(kube)).await.unwrap(),
        Tick::Done
    );
    assert_eq!(
        deleted(&server.received_requests().await.unwrap()),
        ["/apis/volsync.backube/v1alpha1/namespaces/yolab-notes/replicationsources/volsync-notes-data"]
    );

    let (server, kube) = api_server().await;
    serve(&server, CLAIMS, 200, list("PersistentVolumeClaim", vec![])).await;
    for found in [
        "/apis/helm.cattle.io/v1/namespaces/kube-system/helmcharts/volsync",
        "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/replicationsources.volsync.backube",
        "/api/v1/namespaces/volsync-system",
    ] {
        serve(&server, found, 200, serde_json::json!({ "metadata": { "name": "x" } })).await;
    }
    otherwise_absent(&server).await;
    assert_eq!(
        VolsyncRemovalController.converge(&ctx(kube)).await.unwrap(),
        Tick::Done
    );
    assert_eq!(
        deleted(&server.received_requests().await.unwrap()),
        ["/apis/helm.cattle.io/v1/namespaces/kube-system/helmcharts/volsync"]
    );
}

#[tokio::test]
async fn a_replication_list_the_api_refuses_stops_the_survey() {
    let (server, kube) = api_server().await;
    Mock::given(path(SOURCES))
        .respond_with(ResponseTemplate::new(500).set_body_json(status(500, "InternalError")))
        .mount(&server)
        .await;
    assert!(VolsyncRemovalController.converge(&ctx(kube)).await.is_err());
}
