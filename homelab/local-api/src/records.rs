use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, ObjectMeta, PostParams};
use kube::Client;
use serde::{de::DeserializeOwned, Serialize};

const CAS_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Store {
    pub name: &'static str,
    pub namespace: &'static str,
    pub key: &'static str,
}

#[derive(Debug)]
pub enum RecordError {
    Cluster(kube::Error),
    Corrupt { store: String, detail: String },
    Contended { store: String },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Cluster(e) => write!(f, "{e}"),
            RecordError::Corrupt { store, detail } => {
                write!(f, "{store}: stored records are unreadable: {detail}")
            }
            RecordError::Contended { store } => write!(
                f,
                "{store}: gave up after {CAS_ATTEMPTS} conflicting concurrent updates"
            ),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecordError::Cluster(e) => Some(e),
            _ => None,
        }
    }
}

impl From<kube::Error> for RecordError {
    fn from(e: kube::Error) -> Self {
        RecordError::Cluster(e)
    }
}

struct Loaded {
    raw: Option<String>,
    resource_version: Option<String>,
    exists: bool,
}

impl Store {
    fn label(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    fn api(&self, client: &Client) -> Api<ConfigMap> {
        Api::namespaced(client.clone(), self.namespace)
    }

    async fn load(&self, client: &Client) -> Result<Loaded, kube::Error> {
        let Some(cm) = self.api(client).get_opt(self.name).await? else {
            return Ok(Loaded {
                raw: None,
                resource_version: None,
                exists: false,
            });
        };
        Ok(Loaded {
            raw: cm.data.and_then(|mut d| d.remove(self.key)),
            resource_version: cm.metadata.resource_version,
            exists: true,
        })
    }

    pub async fn read<T: DeserializeOwned + Default>(
        &self,
        client: &Client,
    ) -> Result<T, RecordError> {
        let loaded = self.load(client).await?;
        match loaded.raw {
            None => Ok(T::default()),
            Some(raw) => serde_json::from_str(&raw).map_err(|e| RecordError::Corrupt {
                store: self.label(),
                detail: e.to_string(),
            }),
        }
    }

    pub async fn update<T, R>(
        &self,
        client: &Client,
        mut f: impl FnMut(&mut T) -> R,
    ) -> Result<R, RecordError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        for _ in 0..CAS_ATTEMPTS {
            let loaded = self.load(client).await?;
            let mut corrupt: Option<String> = None;
            let mut value: T = match &loaded.raw {
                None => T::default(),
                Some(raw) => match serde_json::from_str(raw) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(
                            "{}: stored records are unreadable ({e}) — keeping them under \
                             `{}.corrupt` and starting a fresh list",
                            self.label(),
                            self.key
                        );
                        corrupt = Some(raw.clone());
                        T::default()
                    }
                },
            };
            let encode = |v: &T| {
                serde_json::to_string(v).map_err(|e| RecordError::Corrupt {
                    store: self.label(),
                    detail: e.to_string(),
                })
            };
            let before = encode(&value)?;
            let result = f(&mut value);
            let body = encode(&value)?;
            if corrupt.is_none() && before == body {
                return Ok(result);
            }
            if loaded.exists && loaded.resource_version.is_none() {
                return Err(RecordError::Corrupt {
                    store: self.label(),
                    detail: "the stored object has no resourceVersion".into(),
                });
            }
            let written = self.object(&body, corrupt, loaded.resource_version);
            let sent = if loaded.exists {
                self.api(client)
                    .replace(self.name, &PostParams::default(), &written)
                    .await
            } else {
                self.api(client)
                    .create(&PostParams::default(), &written)
                    .await
            };
            match sent {
                Ok(_) => return Ok(result),
                Err(e) if crate::k8s::is_conflict(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RecordError::Contended {
            store: self.label(),
        })
    }

    fn object(
        &self,
        body: &str,
        corrupt: Option<String>,
        resource_version: Option<String>,
    ) -> ConfigMap {
        let mut data = std::collections::BTreeMap::new();
        data.insert(self.key.to_string(), body.to_string());
        if let Some(c) = corrupt {
            data.insert(format!("{}.corrupt", self.key), c);
        }
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(self.name.to_string()),
                namespace: Some(self.namespace.to_string()),
                labels: Some(
                    [(
                        "app.kubernetes.io/managed-by".to_string(),
                        "yolab".to_string(),
                    )]
                    .into(),
                ),
                resource_version,
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub fn path_of(name: &str) -> String {
        format!("/api/v1/namespaces/kube-system/configmaps/{name}")
    }

    pub fn stored(name: &str, sets: &Value, rv: &str) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": name, "namespace": "kube-system", "resourceVersion": rv },
            "data": { "sets": sets.to_string() },
        })
    }

    pub async fn records(server: &MockServer, name: &str, sets: Value) {
        Mock::given(method("GET"))
            .and(path(path_of(name)))
            .respond_with(ResponseTemplate::new(200).set_body_json(stored(name, &sets, "1")))
            .mount(server)
            .await;
    }

    pub async fn no_records(server: &MockServer, name: &str) {
        Mock::given(method("GET"))
            .and(path(path_of(name)))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(crate::k8s::testing::status(404, "NotFound")),
            )
            .mount(server)
            .await;
    }

    pub async fn written(server: &MockServer, name: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| matches!(r.method.as_str(), "PUT" | "POST"))
            .filter(|r| r.url.path().contains("/configmaps"))
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .filter(|v| v["metadata"]["name"] == name)
            .map(|v| {
                let sets = v["data"]["sets"].as_str().unwrap_or("null");
                serde_json::from_str(sets).unwrap_or(Value::Null)
            })
            .collect()
    }

    #[derive(Clone)]
    pub struct Kept {
        object: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
    }

    impl Kept {
        pub fn data(&self, key: &str) -> Value {
            let held = self.object.lock().unwrap();
            held.as_ref()
                .and_then(|o| o["data"][key].as_str())
                .map(|raw| serde_json::from_str(raw).unwrap())
                .unwrap_or(Value::Null)
        }
    }

    impl wiremock::Respond for Kept {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let mut held = self.object.lock().unwrap();
            if request.method.as_str() == "GET" {
                return match held.as_ref() {
                    Some(o) => ResponseTemplate::new(200).set_body_json(o),
                    None => ResponseTemplate::new(404)
                        .set_body_json(crate::k8s::testing::status(404, "NotFound")),
                };
            }
            let mut written: Value =
                serde_json::from_slice(&request.body).expect("a ConfigMap was written");
            let version = held
                .as_ref()
                .and_then(|o| o["metadata"]["resourceVersion"].as_str())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            written["metadata"]["resourceVersion"] = Value::String((version + 1).to_string());
            *held = Some(written.clone());
            ResponseTemplate::new(200).set_body_json(written)
        }
    }

    pub async fn kept(server: &MockServer, name: &str) -> Kept {
        let kept = Kept {
            object: Default::default(),
        };
        Mock::given(path(path_of(name)))
            .respond_with(kept.clone())
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/namespaces/kube-system/configmaps"))
            .and(wiremock::matchers::body_partial_json(
                json!({ "metadata": { "name": name } }),
            ))
            .respond_with(kept.clone())
            .mount(server)
            .await;
        kept
    }

    pub async fn accept_writes(server: &MockServer) {
        Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "x" }
            })))
            .mount(server)
            .await;
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "x" }
            })))
            .mount(server)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{path_of, stored};
    use super::*;
    use crate::k8s::testing::{api_server, status};
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const STORE: Store = Store {
        name: "yolab-test",
        namespace: "kube-system",
        key: "sets",
    };

    fn cm(sets: &str, rv: &str) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "yolab-test", "namespace": "kube-system", "resourceVersion": rv },
            "data": { "sets": sets },
        })
    }

    async fn get_answers(server: &MockServer, code: u16, body: Value, times: Option<u64>) {
        let mock = Mock::given(method("GET"))
            .and(path(path_of("yolab-test")))
            .respond_with(ResponseTemplate::new(code).set_body_json(body));
        match times {
            Some(n) => mock.up_to_n_times(n).mount(server).await,
            None => mock.mount(server).await,
        }
    }

    fn ok_write() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(cm("[]", "99"))
    }

    fn conflict() -> ResponseTemplate {
        ResponseTemplate::new(409).set_body_json(status(409, "Conflict"))
    }

    async fn writes(server: &MockServer, verb: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == verb)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_failed_read_is_an_error_never_an_empty_history() {
        let (server, client) = api_server().await;
        get_answers(&server, 503, status(503, "ServiceUnavailable"), None).await;
        let r: Result<Vec<String>, _> = STORE.read(&client).await;
        assert!(r.is_err());

        let wrote = STORE.update(&client, Vec::<String>::clear).await;
        assert!(wrote.is_err());
        assert!(writes(&server, "PUT").await.is_empty());
        assert!(writes(&server, "POST").await.is_empty());
    }

    #[tokio::test]
    async fn a_missing_configmap_is_empty_and_is_created() {
        let (server, client) = api_server().await;
        get_answers(&server, 404, status(404, "NotFound"), None).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/namespaces/kube-system/configmaps"))
            .and(body_partial_json(json!({ "data": { "sets": "[\"a\"]" } })))
            .respond_with(ok_write())
            .expect(1)
            .mount(&server)
            .await;
        let r: Vec<String> = STORE.read(&client).await.unwrap();
        assert!(r.is_empty());
        STORE
            .update(&client, |v: &mut Vec<String>| v.push("a".into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_update_is_a_compare_and_swap_that_retries_from_a_fresh_read() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm(r#"["a"]"#, "1"), Some(1)).await;
        get_answers(&server, 200, cm(r#"["a","b"]"#, "2"), None).await;
        Mock::given(method("PUT"))
            .and(path(path_of("yolab-test")))
            .respond_with(conflict())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(path_of("yolab-test")))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        let mut seen = Vec::new();
        STORE
            .update(&client, |v: &mut Vec<String>| {
                seen.push(v.clone());
                v.push("c".into());
            })
            .await
            .unwrap();
        assert_eq!(
            seen.last().unwrap(),
            &vec!["a".to_string(), "b".to_string()]
        );
        let replaces = writes(&server, "PUT").await;
        assert_eq!(replaces.len(), 2);
        assert_eq!(replaces[0]["metadata"]["resourceVersion"], "1");
        assert_eq!(replaces[1]["metadata"]["resourceVersion"], "2");
        assert_eq!(replaces[1]["data"]["sets"], r#"["a","b","c"]"#);
    }

    #[tokio::test]
    async fn unreadable_content_is_kept_aside_rather_than_discarded() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("not json {", "7"), None).await;
        Mock::given(method("PUT"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        let r: Result<Vec<String>, _> = STORE.read(&client).await;
        assert!(matches!(r, Err(RecordError::Corrupt { .. })));
        STORE
            .update(&client, |v: &mut Vec<String>| v.push("fresh".into()))
            .await
            .unwrap();
        let write = &writes(&server, "PUT").await[0];
        assert_eq!(write["data"]["sets.corrupt"], "not json {");
        assert_eq!(write["data"]["sets"], r#"["fresh"]"#);
    }

    #[tokio::test]
    async fn an_update_that_changes_nothing_writes_nothing() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm(r#"["a"]"#, "1"), None).await;
        let n = STORE
            .update(&client, |v: &mut Vec<String>| v.len())
            .await
            .unwrap();
        assert_eq!(n, 1, "the closure's result is still returned");
        assert!(writes(&server, "PUT").await.is_empty());

        let (absent, client) = api_server().await;
        get_answers(&absent, 404, status(404, "NotFound"), None).await;
        STORE
            .update(&client, |_: &mut Vec<String>| ())
            .await
            .unwrap();
        assert!(
            writes(&absent, "POST").await.is_empty(),
            "an empty record is not worth creating"
        );
    }

    #[tokio::test]
    async fn losing_the_race_to_create_retries_as_a_replace_of_the_winner() {
        let (server, client) = api_server().await;
        get_answers(&server, 404, status(404, "NotFound"), Some(1)).await;
        get_answers(&server, 200, cm(r#"["theirs"]"#, "4"), None).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(409).set_body_json(status(409, "AlreadyExists")))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        STORE
            .update(&client, |v: &mut Vec<String>| v.push("ours".into()))
            .await
            .unwrap();
        let replace = &writes(&server, "PUT").await[0];
        assert_eq!(replace["data"]["sets"], r#"["theirs","ours"]"#);
        assert_eq!(replace["metadata"]["resourceVersion"], "4");
    }

    #[test]
    fn errors_name_the_store_they_are_about() {
        let corrupt = RecordError::Corrupt {
            store: "kube-system/x".into(),
            detail: "eof".into(),
        };
        assert!(corrupt.to_string().contains("kube-system/x"));
        let contended = RecordError::Contended {
            store: "kube-system/x".into(),
        };
        assert!(contended.to_string().contains("gave up"));
        assert!(std::error::Error::source(&contended).is_none());
        let cluster = RecordError::from(kube::Error::LinesCodecMaxLineLengthExceeded);
        assert!(std::error::Error::source(&cluster).is_some());
        assert_eq!(
            cluster.to_string(),
            kube::Error::LinesCodecMaxLineLengthExceeded.to_string()
        );
    }

    #[derive(Default, serde::Deserialize)]
    struct Unwritable;

    impl serde::Serialize for Unwritable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("this value has no JSON form"))
        }
    }

    #[tokio::test]
    async fn records_that_cannot_be_written_as_json_are_refused_before_any_write() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("null", "5"), Some(1)).await;
        get_answers(&server, 404, status(404, "NotFound"), None).await;
        let err = STORE
            .update(&client, std::mem::take::<Unwritable>)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("no JSON form"), "{err}");

        let unkeyable = STORE
            .update(
                &client,
                |m: &mut std::collections::BTreeMap<(u8, u8), u8>| {
                    m.insert((1, 2), 3);
                },
            )
            .await;
        assert!(unkeyable.is_err());
        assert!(writes(&server, "POST").await.is_empty());
        assert!(writes(&server, "PUT").await.is_empty());
    }

    #[tokio::test]
    async fn a_write_the_cluster_refuses_outright_is_an_error_not_a_retry() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "5"), None).await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(403).set_body_json(status(403, "Forbidden")))
            .expect(1)
            .mount(&server)
            .await;
        let r = STORE
            .update(&client, |v: &mut Vec<String>| v.push("a".into()))
            .await;
        assert!(matches!(r, Err(RecordError::Cluster(_))));
    }

    #[tokio::test]
    async fn an_object_without_a_resource_version_is_never_blindly_replaced() {
        let (server, client) = api_server().await;
        get_answers(
            &server,
            200,
            json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": { "name": "yolab-test", "namespace": "kube-system" },
                "data": { "sets": "[]" }
            }),
            None,
        )
        .await;
        let r = STORE
            .update(&client, |v: &mut Vec<String>| v.push("x".into()))
            .await;
        assert!(r.is_err());
        assert!(writes(&server, "PUT").await.is_empty());
    }

    #[tokio::test]
    async fn endless_contention_gives_up_instead_of_overwriting() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "1"), None).await;
        Mock::given(method("PUT"))
            .respond_with(conflict())
            .mount(&server)
            .await;
        let r = STORE
            .update(&client, |v: &mut Vec<String>| v.push("x".into()))
            .await;
        assert!(matches!(r, Err(RecordError::Contended { .. })));
        assert_eq!(writes(&server, "PUT").await.len(), CAS_ATTEMPTS);
    }

    #[derive(Default, serde::Deserialize)]
    struct Doc(Vec<String>);

    impl serde::Serialize for Doc {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if self.0.iter().any(|e| e == "unwritable") {
                return Err(serde::ser::Error::custom("this value has no JSON form"));
            }
            serde::Serialize::serialize(&self.0, s)
        }
    }

    fn push(d: &mut Doc) {
        d.0.push("x".into());
    }

    async fn change(client: &kube::Client, f: &mut dyn FnMut(&mut Doc)) -> Result<(), RecordError> {
        STORE.update(client, f).await
    }

    #[tokio::test]
    async fn one_update_shape_reads_creates_replaces_and_keeps_corrupt_content() {
        let (server, client) = api_server().await;
        get_answers(&server, 503, status(503, "ServiceUnavailable"), None).await;
        let r = change(&client, &mut push).await;
        assert!(matches!(r, Err(RecordError::Cluster(_))));

        let (server, client) = api_server().await;
        get_answers(&server, 404, status(404, "NotFound"), None).await;
        Mock::given(method("POST"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        change(&client, &mut push).await.unwrap();
        change(&client, &mut |_: &mut Doc| {}).await.unwrap();
        let created = writes(&server, "POST").await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["data"]["sets"], r#"["x"]"#);

        let (server, client) = api_server().await;
        get_answers(&server, 200, cm(r#"["a"]"#, "1"), None).await;
        Mock::given(method("PUT"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        change(&client, &mut push).await.unwrap();
        assert_eq!(
            writes(&server, "PUT").await[0]["data"]["sets"],
            r#"["a","x"]"#
        );

        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("not json {", "2"), None).await;
        Mock::given(method("PUT"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        change(&client, &mut push).await.unwrap();
        let write = &writes(&server, "PUT").await[0];
        assert_eq!(write["data"]["sets.corrupt"], "not json {");
        assert_eq!(write["data"]["sets"], r#"["x"]"#);
    }

    #[tokio::test]
    async fn one_update_shape_refuses_unwritable_records_and_unversioned_objects() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm(r#"["unwritable"]"#, "1"), None).await;
        let before = change(&client, &mut push).await;
        assert!(matches!(before, Err(RecordError::Corrupt { .. })));

        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "1"), None).await;
        let after = change(&client, &mut |d: &mut Doc| d.0.push("unwritable".into())).await;
        assert!(matches!(after, Err(RecordError::Corrupt { .. })));
        assert!(writes(&server, "PUT").await.is_empty());

        let (server, client) = api_server().await;
        get_answers(
            &server,
            200,
            json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": { "name": "yolab-test", "namespace": "kube-system" },
                "data": { "sets": "[]" }
            }),
            None,
        )
        .await;
        let unversioned = change(&client, &mut push).await;
        assert!(matches!(unversioned, Err(RecordError::Corrupt { .. })));
        assert!(writes(&server, "PUT").await.is_empty());
    }

    #[tokio::test]
    async fn one_update_shape_retries_conflicts_and_stops_on_refusal_or_endless_contention() {
        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "1"), None).await;
        Mock::given(method("PUT"))
            .respond_with(conflict())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ok_write())
            .mount(&server)
            .await;
        change(&client, &mut push).await.unwrap();
        assert_eq!(writes(&server, "PUT").await.len(), 2);

        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "1"), None).await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(403).set_body_json(status(403, "Forbidden")))
            .mount(&server)
            .await;
        let refused = change(&client, &mut push).await;
        assert!(matches!(refused, Err(RecordError::Cluster(_))));

        let (server, client) = api_server().await;
        get_answers(&server, 200, cm("[]", "1"), None).await;
        Mock::given(method("PUT"))
            .respond_with(conflict())
            .mount(&server)
            .await;
        let contended = change(&client, &mut push).await;
        assert!(matches!(contended, Err(RecordError::Contended { .. })));
        assert_eq!(writes(&server, "PUT").await.len(), CAS_ATTEMPTS);
    }

    #[test]
    fn the_test_fixture_is_what_the_store_reads() {
        let v = stored("yolab-test", &json!(["a"]), "3");
        let cm: ConfigMap = serde_json::from_value(v).unwrap();
        assert_eq!(cm.data.unwrap()["sets"], r#"["a"]"#);
    }
}
