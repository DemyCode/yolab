use super::*;

#[derive(Serialize)]
pub struct DomainResponse {
    pub domain: String,
}

pub(super) const ANN_DISPLAY_NAME: &str = "yolab.io/display-name";
pub(super) const ANN_ICON: &str = "yolab.io/icon";
pub(super) const ANN_CATEGORY: &str = "yolab.io/category";
pub(super) const ANN_GITHUB: &str = "yolab.io/github";
pub(super) const ANN_TAGLINE: &str = "yolab.io/tagline";
pub(super) const ANN_COLLECTIONS: &str = "yolab.io/collections";
pub(super) const ANN_DISABLED: &str = "yolab.io/disabled";

#[derive(Deserialize, Default)]
pub(super) struct ChartYaml {
    pub(super) name: String,
    #[serde(default)]
    pub(super) description: String,
    #[serde(default)]
    pub(super) home: String,
    #[serde(default)]
    pub(super) version: String,
    #[serde(default, rename = "type")]
    pub(super) type_: String,
    #[serde(default)]
    pub(super) annotations: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub(super) dependencies: Vec<ChartDependency>,
}

#[derive(Deserialize)]
pub(super) struct ChartDependency {
    pub(super) name: String,
}

pub const LIBRARIES: [&str; 2] = ["yolab-common", "yolab-stack"];
pub(super) const LOGIN_DEPENDENCIES: [&str; 1] = ["authelia"];

pub(super) struct ChartMeta {
    pub(super) chart: ChartYaml,
    pub(super) app: crate::appschema::AppSchema,
}

impl ChartMeta {
    pub(super) fn ann(&self, key: &str) -> &str {
        self.chart
            .annotations
            .get(key)
            .map(String::as_str)
            .unwrap_or("")
    }
    pub(super) fn in_store(&self) -> bool {
        self.ann(ANN_DISABLED).trim().is_empty()
    }
    pub(super) fn members(&self) -> Vec<String> {
        self.chart
            .dependencies
            .iter()
            .map(|d| d.name.clone())
            .filter(|name| !LIBRARIES.contains(&name.as_str()))
            .filter(|name| !LOGIN_DEPENDENCIES.contains(&name.as_str()))
            .collect()
    }
    pub(super) fn display_name(&self) -> String {
        let n = self.ann(ANN_DISPLAY_NAME);
        if n.is_empty() {
            self.chart.name.clone()
        } else {
            n.to_string()
        }
    }
}

pub(super) fn read_chart(dir: &std::path::Path) -> Option<ChartMeta> {
    let chart: ChartYaml =
        serde_norway::from_str(&std::fs::read_to_string(dir.join("Chart.yaml")).ok()?).ok()?;
    if chart.type_ == "library" {
        return None;
    }
    let schema = std::fs::read_to_string(dir.join("values.schema.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(Value::Null);
    let app = crate::appschema::AppSchema::new(schema);
    Some(ChartMeta { chart, app })
}

pub(crate) fn app_schema(catalog_dir: &std::path::Path, id: &str) -> crate::appschema::AppSchema {
    let found = (!id.is_empty())
        .then(|| read_chart(&catalog_dir.join(id)))
        .flatten();
    match found {
        Some(meta) => meta.app,
        None => crate::appschema::AppSchema::new(Value::Null),
    }
}

pub(crate) async fn installed_schema(
    client: &Client,
    ns: &str,
    app_id: &str,
    fallback_catalog: &std::path::Path,
) -> crate::appschema::AppSchema {
    match crate::saved_chart::read_schema(client, ns).await {
        Ok(Some(schema)) => crate::appschema::AppSchema::new(schema),
        _ => app_schema(fallback_catalog, app_id),
    }
}

pub(crate) async fn chart_schema(
    client: &Client,
    app_id: &str,
) -> Option<crate::appschema::AppSchema> {
    let (_, dir) = crate::charts::resolve_chart(client, app_id, None).await?;
    read_chart(&dir).map(|meta| meta.app)
}

pub(super) fn resolve_service_name(
    schema: &Value,
    config: &serde_json::Map<String, Value>,
) -> String {
    fn tunnel_field(props: Option<&serde_json::Map<String, Value>>) -> Option<(String, Value)> {
        props?.iter().find_map(|(k, v)| {
            (v["format"].as_str() == Some("tunnel")).then(|| (k.clone(), v.clone()))
        })
    }

    let nested = schema["properties"]["config"]["properties"].as_object();
    let top = schema["properties"].as_object();
    let behind_switch = [
        &schema["properties"]["config"]["dependencies"][YOLAB_SWITCH]["oneOf"],
        &schema["dependencies"][YOLAB_SWITCH]["oneOf"],
    ]
    .into_iter()
    .filter_map(Value::as_array)
    .flatten()
    .find_map(|branch| tunnel_field(branch["properties"].as_object()));

    let Some((field, spec)) = tunnel_field(nested)
        .or(behind_switch)
        .or_else(|| tunnel_field(top))
    else {
        return String::new();
    };

    config
        .get(&field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| spec["default"].as_str().filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_string()
}

pub(super) fn build_values(
    config: &serde_json::Map<String, Value>,
    tunnel_cfg: &toml::Table,
    service_name: &str,
) -> String {
    serde_json::json!({
        "config": config,
        "yolab": {
            "platformApiUrl": tunnel_cfg.get("platform_api_url").and_then(|v| v.as_str()).unwrap_or(""),
            "serviceName": service_name,
        },
    })
    .to_string()
}

pub(super) fn derive_domain(dns_url: &str) -> String {
    let host = dns_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 1 && !parts[0].chars().all(|c| c.is_ascii_digit()) {
        parts[1..].join(".")
    } else {
        host.to_string()
    }
}

pub async fn tunnel_domain(State(state): State<AppState>) -> Result<Json<DomainResponse>> {
    let tunnel = tunnel_config(&state.config)?;
    let dns_url = tunnel.get("dns_url").and_then(|v| v.as_str()).unwrap_or("");
    Ok(Json(DomainResponse {
        domain: derive_domain(dns_url),
    }))
}

pub(super) fn catalog_entry_from(
    repo: String,
    meta: ChartMeta,
    stars: &std::collections::HashMap<String, crate::github::RepoStats>,
) -> CatalogApp {
    let members = meta.members();
    let kind = if members.is_empty() { "app" } else { "pack" };
    let github = meta.ann(ANN_GITHUB).to_string();
    let known = stars.get(&github).filter(|s| !s.archived || s.stars > 0);
    CatalogApp {
        id: meta.chart.name.clone(),
        repo,
        name: meta.display_name(),
        description: meta.chart.description.clone(),
        home: meta.chart.home.clone(),
        icon: meta.ann(ANN_ICON).to_string(),
        category: meta.ann(ANN_CATEGORY).to_string(),
        tagline: meta.ann(ANN_TAGLINE).to_string(),
        collections: meta
            .ann(ANN_COLLECTIONS)
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
        stars: known.map(|s| s.stars),
        pushed_at: known.and_then(|s| s.pushed_at.clone()),
        github,
        chart_version: meta.chart.version.clone(),
        kind: kind.to_string(),
        members,
        schema: meta.app.config(),
    }
}

pub async fn refresh_catalog_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let (refreshed, note) = match crate::charts::fetch_newest(
        &b.host,
        std::path::Path::new(crate::charts::CACHE_DIR),
        &crate::charts::list_repos(&b.kube).await,
        &id,
        None,
    )
    .await
    {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };

    let stars = crate::github::read_all(&b.kube).await.unwrap_or_default();
    let sources = crate::charts::chart_sources(&b.kube).await;
    let entry = sources.iter().find_map(|(repo, dir)| {
        let m = read_chart(&dir.join(&id))?;
        Some(catalog_entry_from(repo.clone(), m, &stars))
    });

    Ok(Json(serde_json::json!({
        "refreshed": refreshed,
        "note": note,
        "app": entry,
    })))
}

pub async fn catalog(State(state): State<AppState>) -> Result<Json<Vec<CatalogApp>>> {
    let mut apps: Vec<CatalogApp> = vec![];
    let mut seen: std::collections::HashSet<String> = Default::default();

    let b = state.backend().await?;
    let client = b.kube;
    for repo in crate::charts::list_repos(&client).await {
        if let Err(e) = crate::charts::sync_repo(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repo,
        )
        .await
        {
            tracing::warn!("catalog: {} not refreshed: {e:#}", repo.name);
        }
    }
    let stars = crate::github::read_all(&client).await.unwrap_or_default();
    let sources = crate::charts::chart_sources(&client).await;
    for (repo, dir) in &sources {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Some(meta) = read_chart(&entry.path()) else {
                continue;
            };
            if !seen.insert(meta.chart.name.clone()) || !meta.in_store() {
                continue;
            }
            apps.push(catalog_entry_from(repo.clone(), meta, &stars));
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(Json(apps))
}

#[derive(Deserialize)]
pub struct AddRepoBody {
    pub name: String,
    pub url: String,
}

pub async fn list_repos(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::charts::ChartRepo>>> {
    Ok(Json(
        crate::charts::list_repos(&state.kube.client().await?).await,
    ))
}

pub async fn add_repo(
    State(state): State<AppState>,
    Json(body): Json<AddRepoBody>,
) -> impl IntoResponse {
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    if let Err(e) = crate::charts::add_repo(&b.kube, &body.name, &body.url).await {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let repos = crate::charts::list_repos(&b.kube).await;
    if let Some(r) = repos.iter().find(|r| r.name == body.name) {
        if let Err(e) =
            crate::charts::sync_repo(&b.host, std::path::Path::new(crate::charts::CACHE_DIR), r)
                .await
        {
            return (
                StatusCode::BAD_GATEWAY,
                format!("added, but sync failed: {e}"),
            )
                .into_response();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

pub async fn remove_repo(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    match crate::charts::remove_repo(&client, &name).await {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn sync_repos(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let b = state.backend().await?;
    let mut results = serde_json::Map::new();
    for repo in crate::charts::list_repos(&b.kube).await {
        let entry = match crate::charts::sync_repo(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repo,
        )
        .await
        {
            Ok(n) => serde_json::json!({ "ok": true, "charts": n }),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        };
        results.insert(repo.name.clone(), entry);
    }
    Ok(Json(Value::Object(results)))
}
