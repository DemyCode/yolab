use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
use kube::Client;

pub const NS: &str = "rook-ceph";

pub async fn restart_local_plugin(client: &Client) -> anyhow::Result<()> {
    let on_this_node = ListParams::default()
        .labels("app=csi-cephfsplugin")
        .fields(&format!("spec.nodeName={}", crate::system::hostname()));
    Api::<Pod>::namespaced(client.clone(), NS)
        .delete_collection(&DeleteParams::default(), &on_this_node)
        .await?;
    Ok(())
}

pub async fn plugin_daemonset_exists(client: &Client) -> anyhow::Result<bool> {
    crate::k8s::exists(
        client,
        &crate::k8s::reference("apps/v1", "DaemonSet", NS, "csi-cephfsplugin"),
    )
    .await
}

#[cfg(test)]
pub(crate) mod testing {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub const DAEMONSET: &str = "/apis/apps/v1/namespaces/rook-ceph/daemonsets/csi-cephfsplugin";

    pub async fn plugin_pods_deleted(server: &MockServer, code: u16) {
        let body = if code == 200 {
            json!({ "apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": [] })
        } else {
            crate::k8s::testing::status(code, "InternalError")
        };
        Mock::given(method("DELETE"))
            .and(path("/api/v1/namespaces/rook-ceph/pods"))
            .and(query_param("labelSelector", "app=csi-cephfsplugin"))
            .respond_with(ResponseTemplate::new(code).set_body_json(body))
            .mount(server)
            .await;
    }

    pub async fn deleted_plugin_pods(server: &MockServer) -> bool {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|r| {
                r.method.as_str() == "DELETE" && r.url.path() == "/api/v1/namespaces/rook-ceph/pods"
            })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::k8s::testing::{api_server, serve, status};
    use serde_json::json;

    #[tokio::test]
    async fn only_the_local_plugin_pod_is_deleted_and_a_failure_is_reported() {
        let (server, client) = api_server().await;
        plugin_pods_deleted(&server, 200).await;
        restart_local_plugin(&client).await.unwrap();
        let request = &server.received_requests().await.unwrap()[0];
        let query = request.url.query().unwrap_or_default().to_string();
        assert!(query.contains("fieldSelector=spec.nodeName"), "{query}");
        assert!(
            query.contains("labelSelector=app%3Dcsi-cephfsplugin"),
            "{query}"
        );

        let (failing, client) = api_server().await;
        plugin_pods_deleted(&failing, 500).await;
        assert!(restart_local_plugin(&client).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_daemonset_is_distinguished_from_an_unreachable_api() {
        let (absent, client) = api_server().await;
        serve(&absent, DAEMONSET, 404, status(404, "NotFound")).await;
        assert!(!plugin_daemonset_exists(&client).await.unwrap());

        let (down, client) = api_server().await;
        serve(&down, DAEMONSET, 503, status(503, "ServiceUnavailable")).await;
        assert!(plugin_daemonset_exists(&client).await.is_err());

        let (present, client) = api_server().await;
        serve(
            &present,
            DAEMONSET,
            200,
            json!({ "metadata": { "name": "csi-cephfsplugin" } }),
        )
        .await;
        assert!(plugin_daemonset_exists(&client).await.unwrap());
    }
}
