use super::*;

#[derive(Serialize)]
pub struct OutputsResponse {
    pub outputs: Vec<crate::outputs::ShownOutput>,
}

pub(crate) struct KnownOutputs {
    pub specs: Vec<crate::appschema::OutputSpec>,
    pub remembered: crate::outputs::Remembered,
}

pub(crate) async fn known_outputs(
    client: &Client,
    catalog_dir: &std::path::Path,
    ns: &str,
    rescan_first: bool,
) -> anyhow::Result<KnownOutputs> {
    let ns_v = namespace(client, ns)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{ns} does not exist"))?;
    let ann = ns_v["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let id = ann.get(ANN_APP_ID).and_then(|v| v.as_str()).unwrap_or("");
    let app = installed_schema(client, ns, id, catalog_dir).await;

    let remembered = if rescan_first {
        crate::outputs::rescan(client, ns, &app).await?
    } else {
        crate::outputs::read_remembered(client, ns)
            .await?
            .unwrap_or_default()
    };

    Ok(KnownOutputs {
        specs: app.outputs(),
        remembered,
    })
}

pub(super) async fn outputs_of(
    state: &AppState,
    instance_name: &str,
    rescan_first: bool,
) -> Result<Json<OutputsResponse>> {
    let ns = format!("yolab-{instance_name}");
    let client = state.kube.client().await?;
    let known = known_outputs(&client, &state.config.catalog_dir(), &ns, rescan_first).await?;
    Ok(Json(OutputsResponse {
        outputs: crate::outputs::shown(&known.specs, &known.remembered),
    }))
}

pub async fn app_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<OutputsResponse>> {
    outputs_of(&state, &instance_name, false).await
}

pub async fn scan_outputs(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<OutputsResponse>> {
    outputs_of(&state, &instance_name, true).await
}

#[cfg(test)]
mod tests {
    use crate::k8s::testing::{api_server, serve, status};
    use crate::testkit::TestApi;
    use serde_json::json;

    async fn a_service_with_nothing_remembered() -> (wiremock::MockServer, TestApi) {
        let (server, kube) = api_server().await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-notes",
            200,
            json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "yolab-notes" } }),
        )
        .await;
        for missing in [
            "/api/v1/namespaces/yolab-notes/configmaps/yolab-schema",
            "/api/v1/namespaces/yolab-notes/secrets/yolab-outputs",
        ] {
            serve(&server, missing, 404, status(404, "NotFound")).await;
        }
        (server, TestApi::with_kube(kube).login().await)
    }

    #[tokio::test]
    async fn a_service_without_outputs_shows_none_read_or_rescanned() {
        let (_server, api) = a_service_with_nothing_remembered().await;
        let res = api.get("/api/apps/notes/outputs").await;
        assert_eq!(res.json(), json!({ "outputs": [] }), "{}", res.body);
        let res = api.post("/api/apps/notes/scan-outputs", "").await;
        assert_eq!(res.json(), json!({ "outputs": [] }), "{}", res.body);
    }

    #[tokio::test]
    async fn outputs_that_cannot_be_read_are_an_error_not_an_empty_list() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-notes",
            200,
            json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "yolab-notes" } }),
        )
        .await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-notes/configmaps/yolab-schema",
            404,
            status(404, "NotFound"),
        )
        .await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-notes/secrets/yolab-outputs",
            500,
            status(500, "InternalError"),
        )
        .await;
        let api = TestApi::with_kube(kube).login().await;
        assert!(!api.get("/api/apps/notes/outputs").await.status.is_success());
        assert!(!api
            .post("/api/apps/notes/scan-outputs", "")
            .await
            .status
            .is_success());
    }

    #[tokio::test]
    async fn the_outputs_of_a_service_that_is_not_there_are_an_error() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-gone",
            404,
            status(404, "NotFound"),
        )
        .await;
        let api = TestApi::with_kube(kube).login().await;
        let res = api.get("/api/apps/gone/outputs").await;
        assert_eq!(
            res.json()["error"],
            "yolab-gone does not exist",
            "{}",
            res.body
        );
        let res = TestApi::without_cluster()
            .login()
            .await
            .post("/api/apps/gone/scan-outputs", "")
            .await;
        assert!(!res.status.is_success());
    }
}
