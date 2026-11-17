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
