use std::time::Duration;

use kube::{Client, Config};
use tokio::sync::OnceCell;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const FIELD_MANAGER: &str = "yolab";

pub async fn client() -> anyhow::Result<Client> {
    static CLIENT: OnceCell<Client> = OnceCell::const_new();
    let client = CLIENT
        .get_or_try_init(|| async {
            let mut config = Config::infer()
                .await
                .map_err(|e| anyhow::anyhow!("no way to reach Kubernetes yet: {e}"))?;
            config.read_timeout = Some(REQUEST_TIMEOUT);
            config.write_timeout = Some(REQUEST_TIMEOUT);
            Client::try_from(config).map_err(anyhow::Error::from)
        })
        .await?;
    Ok(client.clone())
}

#[derive(Clone)]
pub struct Kube(Option<Client>);

impl Kube {
    pub fn from_environment() -> Self {
        Self(None)
    }

    #[cfg(test)]
    pub fn with(client: Client) -> Self {
        Self(Some(client))
    }

    pub async fn client(&self) -> anyhow::Result<Client> {
        match &self.0 {
            Some(client) => Ok(client.clone()),
            None => client().await,
        }
    }
}

pub fn is_not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(status) if status.code == 404)
}

pub fn is_conflict(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(status) if status.code == 409)
}

pub fn refused_with(e: &anyhow::Error, code: u16) -> bool {
    matches!(e.downcast_ref::<kube::Error>(), Some(kube::Error::Api(status)) if status.code == code)
}

fn dynamic_api(
    client: &Client,
    manifest: &serde_json::Value,
) -> anyhow::Result<(kube::Api<kube::api::DynamicObject>, String)> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind, TypeMeta};
    let types: TypeMeta = serde_json::from_value(serde_json::json!({
        "apiVersion": manifest["apiVersion"],
        "kind": manifest["kind"],
    }))
    .map_err(|e| anyhow::anyhow!("a manifest without apiVersion and kind: {e}"))?;
    let gvk = GroupVersionKind::try_from(&types)
        .map_err(|e| anyhow::anyhow!("an unusable apiVersion {:?}: {e}", types.api_version))?;
    let resource = ApiResource::from_gvk(&gvk);
    let name = manifest["metadata"]["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow::anyhow!("a {} manifest without a name", types.kind))?
        .to_string();
    let api = match manifest["metadata"]["namespace"].as_str() {
        Some(ns) => kube::Api::<DynamicObject>::namespaced_with(client.clone(), ns, &resource),
        None => kube::Api::<DynamicObject>::all_with(client.clone(), &resource),
    };
    Ok((api, name))
}

pub async fn get(
    client: &Client,
    reference: &serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let (api, name) = dynamic_api(client, reference)?;
    match api.get_opt(&name).await? {
        Some(found) => Ok(Some(stamped(serde_json::to_value(found)?, reference))),
        None => Ok(None),
    }
}

fn stamped(mut object: serde_json::Value, like: &serde_json::Value) -> serde_json::Value {
    if let Some(fields) = object.as_object_mut() {
        fields.insert("apiVersion".into(), like["apiVersion"].clone());
        fields.insert("kind".into(), like["kind"].clone());
    }
    object
}

pub async fn list(
    client: &Client,
    api_version: &str,
    kind: &str,
    namespace: Option<&str>,
    params: &kube::api::ListParams,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let like = serde_json::json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": { "name": "-", "namespace": namespace },
    });
    let (api, _) = dynamic_api(client, &like)?;
    let listed = api.list(params).await?;
    listed
        .items
        .into_iter()
        .map(|item| Ok(stamped(serde_json::to_value(item)?, &like)))
        .collect()
}
pub async fn apply(client: &Client, manifest: &serde_json::Value) -> anyhow::Result<()> {
    use kube::api::{Patch, PatchParams};
    let (api, name) = dynamic_api(client, manifest)?;
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(manifest),
    )
    .await?;
    Ok(())
}

pub async fn create(client: &Client, manifest: &serde_json::Value) -> anyhow::Result<()> {
    let (api, _) = dynamic_api(client, manifest)?;
    let object: kube::api::DynamicObject = serde_json::from_value(manifest.clone())?;
    api.create(&Default::default(), &object).await?;
    Ok(())
}

pub async fn replace(client: &Client, manifest: &serde_json::Value) -> anyhow::Result<()> {
    let (api, name) = dynamic_api(client, manifest)?;
    let object: kube::api::DynamicObject = serde_json::from_value(manifest.clone())?;
    api.replace(&name, &Default::default(), &object).await?;
    Ok(())
}

pub async fn secret_data(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<Option<std::collections::HashMap<String, String>>, kube::Error> {
    use k8s_openapi::api::core::v1::Secret;
    let secret = kube::Api::<Secret>::namespaced(client.clone(), namespace)
        .get_opt(name)
        .await?;
    Ok(secret.map(|s| {
        s.data
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, String::from_utf8_lossy(&v.0).into_owned()))
            .collect()
    }))
}

pub fn secret_manifest(
    name: &str,
    namespace: &str,
    data: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> serde_json::Value {
    let string_data: serde_json::Map<String, serde_json::Value> = data
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
        .collect();
    let labels: serde_json::Map<String, serde_json::Value> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
        .collect();
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "Opaque",
        "metadata": { "name": name, "namespace": namespace, "labels": labels },
        "stringData": string_data,
    })
}

pub async fn merge_patch(client: &Client, manifest: &serde_json::Value) -> anyhow::Result<()> {
    use kube::api::{Patch, PatchParams};
    let (api, name) = dynamic_api(client, manifest)?;
    api.patch(&name, &PatchParams::default(), &Patch::Merge(manifest))
        .await?;
    Ok(())
}

pub async fn delete_if_present(
    client: &Client,
    manifest: &serde_json::Value,
) -> anyhow::Result<()> {
    let (api, name) = dynamic_api(client, manifest)?;
    match api.delete(&name, &Default::default()).await {
        Ok(_) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn exists(client: &Client, manifest: &serde_json::Value) -> anyhow::Result<bool> {
    let (api, name) = dynamic_api(client, manifest)?;
    Ok(api.get_opt(&name).await?.is_some())
}

pub fn reference(api_version: &str, kind: &str, namespace: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": { "name": name, "namespace": namespace },
    })
}

pub fn cluster_reference(api_version: &str, kind: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": { "name": name },
    })
}

pub async fn ready(client: &Client) -> bool {
    let Ok(request) = axum::http::Request::get("/readyz").body(Vec::new()) else {
        return false;
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(15), client.request_text(request)).await,
        Ok(Ok(_))
    )
}

pub async fn nodes(client: &Client) -> anyhow::Result<Vec<serde_json::Value>> {
    list(client, "v1", "Node", None, &Default::default()).await
}

pub fn peer_ipv6(nodes: &[serde_json::Value], self_ip: &str) -> Vec<String> {
    nodes
        .iter()
        .filter_map(cluster_ipv6)
        .filter(|a| a != self_ip)
        .collect()
}

fn cluster_ipv6(node: &serde_json::Value) -> Option<String> {
    node["status"]["addresses"]
        .as_array()?
        .iter()
        .find(|a| {
            a["type"] == "InternalIP" && a["address"].as_str().is_some_and(|s| s.contains(':'))
        })
        .and_then(|a| a["address"].as_str())
        .map(String::from)
}

pub fn documents(text: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let parsed: Vec<serde_json::Value> = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(one) => vec![one],
        Err(_) => {
            use serde::Deserialize as _;
            serde_norway::Deserializer::from_str(text)
                .map(serde_json::Value::deserialize)
                .collect::<Result<_, _>>()
                .map_err(|e| anyhow::anyhow!("neither JSON nor YAML: {e}"))?
        }
    };
    let mut objects = Vec::new();
    for doc in parsed {
        if doc.is_null() {
            continue;
        }
        let is_list =
            doc["kind"].as_str().is_some_and(|k| k.ends_with("List")) && doc["items"].is_array();
        if is_list {
            objects.extend(doc["items"].as_array().into_iter().flatten().cloned());
        } else {
            objects.push(doc);
        }
    }
    Ok(objects)
}

pub async fn apply_documents(client: &Client, text: &str) -> anyhow::Result<()> {
    for object in documents(text)? {
        apply(client, &object).await.map_err(|e| {
            anyhow::anyhow!(
                "{} {}: {e}",
                object["kind"].as_str().unwrap_or("object"),
                object["metadata"]["name"].as_str().unwrap_or("?")
            )
        })?;
    }
    Ok(())
}
#[cfg(test)]
pub mod testing {
    use kube::{Client, Config};
    use serde_json::{json, Value};
    use wiremock::MockServer;

    pub async fn api_server() -> (MockServer, Client) {
        let server = MockServer::start().await;
        let config = Config::new(server.uri().parse().expect("the mock server has a URL"));
        let client = Client::try_from(config).expect("a client for the mock API server");
        (server, client)
    }

    pub fn unreachable() -> Client {
        let config = Config::new("http://127.0.0.1:9".parse().expect("a URL"));
        Client::try_from(config).expect("a client that reaches nothing")
    }

    pub fn list(kind: &str, items: Vec<Value>) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": format!("{kind}List"),
            "metadata": { "resourceVersion": "1" },
            "items": items,
        })
    }

    pub async fn serve(server: &MockServer, path: &str, code: u16, body: Value) {
        use wiremock::{matchers, Mock, ResponseTemplate};
        Mock::given(matchers::method("GET"))
            .and(matchers::path(path))
            .respond_with(ResponseTemplate::new(code).set_body_json(body))
            .mount(server)
            .await;
    }

    pub async fn serve_logs(server: &MockServer, pod_path: &str, container: &str, text: &str) {
        use wiremock::{matchers, Mock, ResponseTemplate};
        Mock::given(matchers::method("GET"))
            .and(matchers::path(format!("{pod_path}/log")))
            .and(matchers::query_param("container", container))
            .respond_with(ResponseTemplate::new(200).set_body_string(text))
            .mount(server)
            .await;
    }

    pub async fn accept_patches(server: &MockServer) {
        use wiremock::{matchers, Mock, ResponseTemplate};
        Mock::given(matchers::method("PATCH"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "x" }
            })))
            .mount(server)
            .await;
    }

    pub async fn patched(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .filter_map(|r| serde_json::from_slice(&r.body).ok())
            .collect()
    }

    pub async fn asked(server: &MockServer, path_part: &str) -> bool {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|r| r.url.path().contains(path_part))
    }

    pub fn secret_with(key: &str, value: &str) -> Value {
        use base64::Engine as _;
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "s" },
            "data": { key: base64::engine::general_purpose::STANDARD.encode(value) }
        })
    }

    pub fn status(code: u16, reason: &str) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Status",
            "metadata": {},
            "status": "Failure",
            "message": reason,
            "reason": reason,
            "code": code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{Api, ListParams};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn a_client_pointed_at_the_mock_server_lists_what_it_serves() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces"))
            .and(query_param("labelSelector", "yolab.io/managed=true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(testing::list(
                "Namespace",
                vec![json!({ "metadata": { "name": "yolab-gitea-ab12" } })],
            )))
            .expect(1)
            .mount(&server)
            .await;

        let listed = Api::<Namespace>::all(client)
            .list(&ListParams::default().labels("yolab.io/managed=true"))
            .await
            .unwrap();

        assert_eq!(
            listed.items[0].metadata.name.as_deref(),
            Some("yolab-gitea-ab12")
        );
    }

    #[tokio::test]
    async fn a_missing_object_is_told_apart_from_other_failures() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces/gone"))
            .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces/busy"))
            .respond_with(ResponseTemplate::new(409).set_body_json(status(409, "Conflict")))
            .mount(&server)
            .await;

        let api = Api::<Namespace>::all(client);
        let gone = api.get("gone").await.unwrap_err();
        let busy = api.get("busy").await.unwrap_err();

        assert!(is_not_found(&gone) && !is_conflict(&gone));
        assert!(is_conflict(&busy) && !is_not_found(&busy));
    }

    fn replication_source() -> serde_json::Value {
        json!({
            "apiVersion": "volsync.backube/v1alpha1",
            "kind": "ReplicationSource",
            "metadata": { "name": "volsync-data", "namespace": "yolab-gitea-ab12" },
            "spec": { "sourcePVC": "data" }
        })
    }

    #[tokio::test]
    async fn applying_any_kind_is_a_server_side_apply_owned_by_yolab() {
        use wiremock::matchers::header;
        let (server, client) = api_server().await;
        Mock::given(method("PATCH"))
            .and(path(
                "/apis/volsync.backube/v1alpha1/namespaces/yolab-gitea-ab12/replicationsources/volsync-data",
            ))
            .and(query_param("fieldManager", "yolab"))
            .and(query_param("force", "true"))
            .and(header("content-type", "application/apply-patch+yaml"))
            .respond_with(ResponseTemplate::new(200).set_body_json(replication_source()))
            .expect(1)
            .mount(&server)
            .await;

        apply(&client, &replication_source()).await.unwrap();
    }

    #[tokio::test]
    async fn a_core_kind_is_applied_under_the_core_api() {
        let (server, client) = api_server().await;
        let secret = secret_manifest(
            "s",
            "ns",
            &[("k", "v")],
            &[("app.kubernetes.io/managed-by", "yolab")],
        );
        Mock::given(method("PATCH"))
            .and(path("/api/v1/namespaces/ns/secrets/s"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&secret))
            .expect(1)
            .mount(&server)
            .await;

        apply(&client, &secret).await.unwrap();
    }

    #[test]
    fn a_secret_manifest_carries_its_values_as_plain_text_for_the_server_to_encode() {
        let secret = secret_manifest("s", "ns", &[("password", "hunter2")], &[("a", "b")]);
        assert_eq!(secret["stringData"]["password"], "hunter2");
        assert_eq!(secret["metadata"]["labels"]["a"], "b");
        assert!(secret.get("data").is_none());
    }

    #[tokio::test]
    async fn a_manifest_without_a_name_or_kind_is_refused_before_anything_is_sent() {
        let (server, client) = api_server().await;
        let nameless = json!({ "apiVersion": "v1", "kind": "Secret", "metadata": {} });
        let kindless = json!({ "metadata": { "name": "x" } });
        assert!(apply(&client, &nameless).await.is_err());
        assert!(apply(&client, &kindless).await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_secret_is_read_back_decoded() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/namespaces/kube-system/secrets/yolab-backup-config",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": "yolab-backup-config", "namespace": "kube-system" },
                "data": { "restic_password": "aHVudGVyMg==" }
            })))
            .mount(&server)
            .await;

        let data = secret_data(&client, "kube-system", "yolab-backup-config")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data["restic_password"], "hunter2");
    }

    #[tokio::test]
    async fn a_secret_that_does_not_exist_is_none_not_an_error() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces/ns/secrets/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
            .mount(&server)
            .await;
        assert_eq!(secret_data(&client, "ns", "missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_unreachable_api_server_is_an_error_not_a_missing_secret() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
            )
            .mount(&server)
            .await;
        assert!(secret_data(&client, "ns", "s").await.is_err());
    }

    #[tokio::test]
    async fn deleting_something_already_gone_is_fine() {
        let (server, client) = api_server().await;
        Mock::given(method("DELETE"))
            .and(path(
                "/apis/volsync.backube/v1alpha1/namespaces/yolab-gitea-ab12/replicationsources/volsync-data",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
            .expect(1)
            .mount(&server)
            .await;
        delete_if_present(&client, &replication_source())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn existence_is_asked_of_the_right_resource() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path(
                "/apis/volsync.backube/v1alpha1/namespaces/yolab-gitea-ab12/replicationsources/volsync-data",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(replication_source()))
            .mount(&server)
            .await;
        assert!(exists(&client, &replication_source()).await.unwrap());
        let other = reference(
            "volsync.backube/v1alpha1",
            "ReplicationSource",
            "yolab-gitea-ab12",
            "nope",
        );
        Mock::given(method("GET"))
            .and(path(
                "/apis/volsync.backube/v1alpha1/namespaces/yolab-gitea-ab12/replicationsources/nope",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
            .mount(&server)
            .await;
        assert!(!exists(&client, &other).await.unwrap());
    }

    #[tokio::test]
    async fn a_merge_patch_is_sent_as_a_merge_patch() {
        use wiremock::matchers::{body_json, header};
        let (server, client) = api_server().await;
        let patch = json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": { "name": "yolab-a", "annotations": { "x": "y" } }
        });
        Mock::given(method("PATCH"))
            .and(path("/api/v1/namespaces/yolab-a"))
            .and(header("content-type", "application/merge-patch+json"))
            .and(body_json(&patch))
            .respond_with(ResponseTemplate::new(200).set_body_json(&patch))
            .expect(1)
            .mount(&server)
            .await;
        merge_patch(&client, &patch).await.unwrap();
    }

    #[test]
    fn a_list_is_the_objects_in_it() {
        let text = json!({ "apiVersion": "v1", "kind": "List", "items": [
            { "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "a" } },
            { "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "s", "namespace": "a" } }
        ]})
        .to_string();
        let kinds: Vec<String> = documents(&text)
            .unwrap()
            .iter()
            .map(|o| o["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            kinds,
            vec!["Namespace", "Secret"],
            "in the order they were saved"
        );
    }

    #[test]
    fn several_yaml_documents_are_read_one_by_one() {
        let text = "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: a\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\n  namespace: a\n";
        assert_eq!(documents(text).unwrap().len(), 2);
    }

    #[test]
    fn text_that_is_no_manifest_at_all_is_an_error() {
        assert!(documents("{ not: [valid").is_err());
    }

    #[tokio::test]
    async fn applying_a_saved_namespace_applies_every_object_in_order() {
        let (server, client) = api_server().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/namespaces/yolab-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "yolab-a" } }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/namespaces/yolab-a/configmaps/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": { "name": "settings", "namespace": "yolab-a" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let saved = json!({ "apiVersion": "v1", "kind": "List", "items": [
            { "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "yolab-a" } },
            { "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "settings", "namespace": "yolab-a" } }
        ]});

        apply_documents(&client, &saved.to_string()).await.unwrap();

        let order: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(
            order,
            vec![
                "/api/v1/namespaces/yolab-a",
                "/api/v1/namespaces/yolab-a/configmaps/settings"
            ]
        );
    }

    #[tokio::test]
    async fn a_failure_names_the_object_that_could_not_be_applied() {
        let (server, client) = api_server().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(422).set_body_json(status(422, "Invalid")))
            .mount(&server)
            .await;
        let err = apply_documents(
            &client,
            &json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "settings", "namespace": "a" } })
                .to_string(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ConfigMap settings"));
    }

    #[tokio::test]
    async fn listed_objects_carry_the_kind_the_server_leaves_off_list_items() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/apis/storage.k8s.io/v1/storageclasses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(testing::list(
                "StorageClass",
                vec![json!({ "metadata": { "name": "yolab-cephfs" }, "provisioner": "cephfs" })],
            )))
            .mount(&server)
            .await;
        let items = super::list(
            &client,
            "storage.k8s.io/v1",
            "StorageClass",
            None,
            &ListParams::default(),
        )
        .await
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["apiVersion"], "storage.k8s.io/v1");
        assert_eq!(items[0]["kind"], "StorageClass");
        assert_eq!(items[0]["provisioner"], "cephfs");
    }

    #[tokio::test]
    async fn a_namespaced_list_asks_only_that_namespace() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/namespaces/yolab-notes/configmaps"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(testing::list("ConfigMap", vec![])),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(super::list(
            &client,
            "v1",
            "ConfigMap",
            Some("yolab-notes"),
            &ListParams::default()
        )
        .await
        .unwrap()
        .is_empty());
    }

    #[tokio::test]
    async fn a_fetched_object_is_stamped_and_a_missing_one_is_none() {
        let (server, client) = api_server().await;
        Mock::given(method("GET"))
            .and(path("/apis/apps/v1/namespaces/a/deployments/web"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "metadata": { "name": "web", "namespace": "a" } })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/apis/apps/v1/namespaces/a/deployments/gone"))
            .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
            .mount(&server)
            .await;
        let found = get(&client, &reference("apps/v1", "Deployment", "a", "web"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found["kind"], "Deployment");
        assert_eq!(found["apiVersion"], "apps/v1");
        assert!(
            get(&client, &reference("apps/v1", "Deployment", "a", "gone"))
                .await
                .unwrap()
                .is_none()
        );
    }

    fn node(ips: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "status": { "addresses": ips.iter()
                .map(|(t, a)| serde_json::json!({"type": t, "address": a}))
                .collect::<Vec<_>>() }
        })
    }

    #[test]
    fn this_node_is_never_its_own_peer() {
        let nodes = [
            node(&[("InternalIP", "fd00:cafe::5")]),
            node(&[("InternalIP", "fd00:cafe::6")]),
        ];
        assert_eq!(peer_ipv6(&nodes, "fd00:cafe::5"), vec!["fd00:cafe::6"]);
    }

    #[test]
    fn an_ipv4_internal_ip_is_ignored() {
        let nodes = [node(&[("InternalIP", "10.0.0.7")])];
        assert!(peer_ipv6(&nodes, "fd00:cafe::5").is_empty());
    }

    #[test]
    fn the_hostname_entry_is_not_mistaken_for_an_address() {
        let nodes = [node(&[
            ("Hostname", "node2"),
            ("InternalIP", "fd00:cafe::6"),
        ])];
        assert_eq!(peer_ipv6(&nodes, "fd00:cafe::5"), vec!["fd00:cafe::6"]);
    }

    #[test]
    fn a_single_node_cluster_has_no_peers() {
        let nodes = [node(&[("InternalIP", "fd00:cafe::5")])];
        assert!(peer_ipv6(&nodes, "fd00:cafe::5").is_empty());
    }

    #[test]
    fn every_peer_is_listed_once_and_in_order() {
        let nodes = [
            node(&[("InternalIP", "fd00:cafe::5")]),
            node(&[("InternalIP", "fd00:cafe::6")]),
            node(&[("InternalIP", "fd00:cafe::7")]),
        ];
        assert_eq!(
            peer_ipv6(&nodes, "fd00:cafe::5"),
            vec!["fd00:cafe::6", "fd00:cafe::7"]
        );
    }
}
