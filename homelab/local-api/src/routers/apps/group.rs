use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Map;

use crate::group_chart::{GroupRecord, Member, MemberStatus};
use crate::groups::Membership;

const GROUPS_NS: &str = "yolab-groups";
const RECORD_KEY: &str = "group.json";
const LABEL_RECORD: &str = "yolab.io/group-record";
const RENDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

const WAITING: &str = "waiting";
const WORKING: &str = "working";
const DONE: &str = "done";
const FAILED: &str = "failed";

pub(crate) fn is_group(meta: &ChartMeta) -> bool {
    meta.ann(crate::group_chart::KIND_ANNOTATION) == crate::group_chart::KIND_GROUP
}

fn schema_loader(
    sources: &[(String, PathBuf)],
) -> impl Fn(&str, Option<&str>) -> Option<Value> + '_ {
    move |chart: &str, version: Option<&str>| {
        let cache = std::path::Path::new(crate::charts::CACHE_DIR);
        sources.iter().find_map(|(repo, dir)| {
            let found = match version {
                Some(v) => {
                    crate::charts::cached_at_version(cache, repo, chart, v).or_else(|| {
                        let d = dir.join(chart);
                        read_chart(&d).filter(|m| m.chart.version == v).map(|_| d)
                    })?
                }
                None => dir.join(chart),
            };
            let text = std::fs::read_to_string(found.join("values.schema.json")).ok()?;
            serde_json::from_str(&text).ok()
        })
    }
}

pub(crate) fn resolved_schema(
    meta: &ChartMeta,
    sources: &[(String, PathBuf)],
) -> Result<crate::appschema::AppSchema, String> {
    let loader = schema_loader(sources);
    crate::group_chart::resolve(meta.app.document(), &loader).map(crate::appschema::AppSchema::new)
}

fn record_secret(record: &GroupRecord) -> anyhow::Result<Value> {
    let text = serde_json::to_string(record)?;
    Ok(crate::k8s::secret_manifest(
        &record.name,
        GROUPS_NS,
        &[(RECORD_KEY, text.as_str())],
        &[(LABEL_RECORD, "true")],
    ))
}

async fn write_record(client: &Client, record: &GroupRecord) -> anyhow::Result<()> {
    crate::k8s::apply(
        client,
        &serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": GROUPS_NS },
        }),
    )
    .await?;
    crate::k8s::apply(client, &record_secret(record)?).await
}

async fn read_record(client: &Client, name: &str) -> anyhow::Result<Option<GroupRecord>> {
    let Some(data) = crate::k8s::secret_data(client, GROUPS_NS, name).await? else {
        return Ok(None);
    };
    let raw = data
        .get(RECORD_KEY)
        .ok_or_else(|| anyhow::anyhow!("the group {name} has no saved record"))?;
    Ok(Some(serde_json::from_str(raw)?))
}

fn record_of(secret: &Value) -> Option<GroupRecord> {
    use base64::Engine as _;
    let encoded = secret["data"][RECORD_KEY].as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn list_records(client: &Client) -> anyhow::Result<Vec<GroupRecord>> {
    let found = crate::k8s::list(
        client,
        "v1",
        "Secret",
        Some(GROUPS_NS),
        &kube::api::ListParams::default().labels(&format!("{LABEL_RECORD}=true")),
    )
    .await?;
    Ok(found.iter().filter_map(record_of).collect())
}

fn values_file(config: &Map<String, Value>, apps: &BTreeMap<String, String>) -> Value {
    serde_json::json!({ "config": config, "yolab": { "apps": apps } })
}

async fn render<H: crate::host::Host>(
    host: &H,
    name: &str,
    dir: &std::path::Path,
    config: &Map<String, Value>,
    apps: &BTreeMap<String, String>,
) -> Result<Vec<Member>, String> {
    let staged = |e: std::io::Error| format!("could not stage the group's values: {e}");
    let values = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .map_err(staged)?;
    std::fs::write(values.path(), values_file(config, apps).to_string()).map_err(staged)?;
    let out = host
        .run_cmd_bounded(
            "helm",
            &[
                "template",
                name,
                &dir.to_string_lossy(),
                "--values",
                &values.path().to_string_lossy(),
            ],
            RENDER_TIMEOUT,
        )
        .await
        .map_err(|e| format!("could not run helm: {e}"))?;
    if !out.success {
        return Err(format!(
            "the group does not render with these choices: {}",
            out.stderr.trim()
        ));
    }
    crate::group_chart::members(&out.stdout)
}

async fn plan_members<H: crate::host::Host>(
    b: &Backend<H>,
    name: &str,
    dir: &std::path::Path,
    config: &Map<String, Value>,
    known: &BTreeMap<String, String>,
) -> Result<(Vec<Member>, BTreeMap<String, String>), String> {
    let first = render(&b.host, name, dir, config, known).await?;
    let mut namespaces = known.clone();
    for member in &first {
        match member {
            Member::Use { key, namespace, .. } => {
                namespaces.insert(key.clone(), namespace.clone());
            }
            Member::Install { key, .. } if !namespaces.contains_key(key) => {
                let instance = unique_instance_name(&b.kube, key)
                    .await
                    .ok_or_else(|| format!("could not find a free name for {key}"))?;
                namespaces.insert(key.clone(), format!("yolab-{instance}"));
            }
            Member::Install { .. } => {}
        }
    }
    let members = render(&b.host, name, dir, config, &namespaces).await?;
    if let Some(stray) = members.iter().find(|m| !namespaces.contains_key(m.key())) {
        return Err(format!(
            "{} only appears once the group knows where its other apps live — a group must name the same apps on both passes",
            stray.key()
        ));
    }
    let rendered: BTreeSet<&str> = members.iter().map(Member::key).collect();
    namespaces.retain(|k, _| rendered.contains(k.as_str()));
    Ok((members, namespaces))
}

fn check_config(
    app: &crate::appschema::AppSchema,
    config: &Map<String, Value>,
) -> Result<(), String> {
    let validator = jsonschema::validator_for(&app.config())
        .map_err(|e| format!("the group's form is not a valid schema: {e}"))?;
    let instance = Value::Object(config.clone());
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn refuse(code: StatusCode, why: impl Into<String>) -> axum::response::Response {
    (code, why.into()).into_response()
}

fn waiting_for(members: &BTreeMap<String, String>) -> BTreeMap<String, MemberStatus> {
    members
        .keys()
        .map(|k| {
            (
                k.clone(),
                MemberStatus {
                    state: WAITING.into(),
                    message: String::new(),
                },
            )
        })
        .collect()
}

fn busy(record: &GroupRecord) -> bool {
    record
        .status
        .values()
        .any(|s| s.state == WAITING || s.state == WORKING)
}

#[derive(Deserialize)]
pub struct GroupInstall {
    pub chart: String,
    #[serde(default)]
    pub repo: Option<String>,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub config: Map<String, Value>,
}

pub async fn install_group(
    State(state): State<AppState>,
    Json(body): Json<GroupInstall>,
) -> axum::response::Response {
    if !crate::folders::is_name(&body.name) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "a group name may only use lowercase letters, numbers and hyphens",
        );
    }
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    match read_record(&b.kube, &body.name).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return refuse(
                StatusCode::CONFLICT,
                format!(
                    "you already have a group named {} — pick another name",
                    body.name
                ),
            )
        }
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    }
    let Some((repo, dir)) =
        crate::charts::resolve_chart(&b.kube, &body.chart, body.repo.as_deref()).await
    else {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("no group named {}", body.chart),
        );
    };
    let Some(meta) = read_chart(&dir).filter(is_group) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("{} is not a group of apps", body.chart),
        );
    };
    let sources = crate::charts::chart_sources(&b.kube).await;
    let app = match resolved_schema(&meta, &sources) {
        Ok(app) => app,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why),
    };
    if let Err(why) = check_config(&app, &body.config) {
        return refuse(StatusCode::BAD_REQUEST, why);
    }
    let planned = plan_members(&b, &body.name, &dir, &body.config, &BTreeMap::new()).await;
    let (members, namespaces) = match planned {
        Ok(planned) => planned,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why),
    };
    let record = GroupRecord {
        name: body.name.clone(),
        title: body
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| meta.display_name()),
        chart: body.chart.clone(),
        repo,
        version: meta.chart.version.clone(),
        values: body.config,
        status: waiting_for(&namespaces),
        members: namespaces,
        left: Vec::new(),
        reused: crate::group_chart::reused(&members),
    };
    if let Err(e) = write_record(&b.kube, &record).await {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    let cfg = state.config.clone();
    tokio::spawn(async move { run_group(&b, &cfg, record, members, BTreeMap::new()).await });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "name": body.name })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct GroupEdit {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub config: Map<String, Value>,
}

pub async fn edit_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<GroupEdit>,
) -> axum::response::Response {
    let b = match state.backend().await {
        Ok(b) => b,
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    let mut record = match read_record(&b.kube, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, format!("no group named {name}")),
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    if busy(&record) {
        return refuse(
            StatusCode::CONFLICT,
            "this group is still being set up — wait until it finishes",
        );
    }
    let Some((_, dir)) =
        crate::charts::resolve_chart(&b.kube, &record.chart, Some(&record.repo)).await
    else {
        return refuse(
            StatusCode::NOT_FOUND,
            format!("the group's chart {} is gone", record.chart),
        );
    };
    let Some(meta) = read_chart(&dir).filter(is_group) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("{} is not a group of apps", record.chart),
        );
    };
    let sources = crate::charts::chart_sources(&b.kube).await;
    let app = match resolved_schema(&meta, &sources) {
        Ok(app) => app,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why),
    };
    let config = crate::group_chart::keep_secrets(&body.config, &record.values, &app.config());
    if let Err(why) = check_config(&app, &config) {
        return refuse(StatusCode::BAD_REQUEST, why);
    }
    let before = record.members.clone();
    let (members, namespaces) = match plan_members(&b, &name, &dir, &config, &before).await {
        Ok(planned) => planned,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why),
    };
    let leaving: BTreeMap<String, String> = before
        .into_iter()
        .filter(|(k, _)| !namespaces.contains_key(k))
        .collect();
    if let Some(title) = body.title.filter(|t| !t.trim().is_empty()) {
        record.title = title;
    }
    record.version = meta.chart.version.clone();
    record.values = config;
    record.status = waiting_for(&namespaces);
    record.members = namespaces;
    record.reused = crate::group_chart::reused(&members);
    if let Err(e) = write_record(&b.kube, &record).await {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    let cfg = state.config.clone();
    tokio::spawn(async move { run_group(&b, &cfg, record, members, leaving).await });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "name": name })),
    )
        .into_response()
}

async fn set_status(
    client: &Client,
    record: &mut GroupRecord,
    key: &str,
    state: &str,
    message: String,
) {
    record.status.insert(
        key.to_string(),
        MemberStatus {
            state: state.to_string(),
            message,
        },
    );
    write_record(client, record).await.warn_on_err(format!(
        "group {}: could not save its progress",
        record.name
    ));
}

async fn run_group(
    b: &Backend,
    cfg: &Config,
    mut record: GroupRecord,
    members: Vec<Member>,
    leaving: BTreeMap<String, String>,
) {
    let log = install::Log::to_tracing(format!("group {}", record.name));
    for (key, ns) in &leaving {
        crate::groups::set(&b.kube, ns, None)
            .await
            .warn_on_err(format!("group {}: could not take {key} out", record.name));
        record
            .left
            .push(ns.trim_start_matches("yolab-").to_string());
    }
    for key in crate::group_chart::install_order(&members, &record.members) {
        let (Some(member), Some(ns)) = (
            members.iter().find(|m| m.key() == key),
            record.members.get(&key).cloned(),
        ) else {
            continue;
        };
        set_status(&b.kube, &mut record, &key, WORKING, String::new()).await;
        let membership = Membership {
            name: record.name.clone(),
            title: record.title.clone(),
            main: member.main(),
        };
        match apply_member(b, cfg, member, &ns, &membership, &log).await {
            Ok(()) => set_status(&b.kube, &mut record, &key, DONE, String::new()).await,
            Err(e) => set_status(&b.kube, &mut record, &key, FAILED, format!("{e:#}")).await,
        }
    }
}

async fn apply_member(
    b: &Backend,
    cfg: &Config,
    member: &Member,
    ns: &str,
    membership: &Membership,
    log: &install::Log,
) -> anyhow::Result<()> {
    let instance = ns.trim_start_matches("yolab-").to_string();
    let (chart, version, values) = match member {
        Member::Use { .. } => {
            namespace(&b.kube, ns)
                .await?
                .ok_or_else(|| anyhow::anyhow!("{instance} is not installed"))?;
            return crate::groups::set(&b.kube, ns, Some(membership)).await;
        }
        Member::Install {
            chart,
            version,
            values,
            ..
        } => (chart, version, values),
    };
    if read_definition_opt(&b.kube, ns).await.is_some() {
        let mut plan = stored_upgrade_plan(&b.kube, &instance).await?;
        let app = installed_schema(&b.kube, ns, chart, &cfg.catalog_dir()).await;
        let mut config = values.clone();
        for field in app.credentials() {
            if let (false, Some(kept)) = (config.contains_key(&field), plan.config.get(&field)) {
                config.insert(field, kept.clone());
            }
        }
        plan.config = config;
        crate::groups::set(&b.kube, ns, Some(membership)).await?;
        return install::upgrade(b, cfg, &plan, log).await;
    }
    let (repo, _) = crate::charts::resolve_chart(&b.kube, chart, None)
        .await
        .ok_or_else(|| anyhow::anyhow!("no chart named {chart} in any catalog"))?;
    let schema = chart_schema(&b.kube, chart)
        .await
        .unwrap_or_else(|| crate::appschema::AppSchema::new(Value::Null));
    let plan = install::InstallPlan {
        app_id: chart.clone(),
        instance_name: instance.clone(),
        release: instance.clone(),
        config: crate::group_chart::generated(&schema.config(), values),
        backup: BackupPolicy::default(),
        data: None,
        chart: match version {
            Some(v) => install::ChartPin::Exact {
                repo: repo.clone(),
                version: v.clone(),
            },
            None => install::ChartPin::Newest,
        },
    };
    open_app_namespace(
        &b.kube,
        chart,
        &instance,
        &ChartAt::Catalog {
            repo: Some(repo.as_str()),
        },
    )
    .await?;
    crate::groups::set(&b.kube, ns, Some(membership)).await?;
    install::execute(b, cfg, &plan, log).await
}

#[derive(Serialize)]
pub struct GroupView {
    #[serde(flatten)]
    pub record: GroupRecord,
    pub schema: Value,
}

pub async fn list_groups(State(state): State<AppState>) -> Result<Json<Vec<GroupRecord>>> {
    let client = state.kube.client().await?;
    let mut records = list_records(&client).await?;
    for record in &mut records {
        record.values = Map::new();
    }
    records.sort_by_key(|r| r.title.to_lowercase());
    Ok(Json(records))
}

pub async fn get_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    let mut record = match read_record(&client, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, format!("no group named {name}")),
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    let sources = crate::charts::chart_sources(&client).await;
    let schema = crate::charts::resolve_chart(&client, &record.chart, Some(&record.repo))
        .await
        .and_then(|(_, dir)| read_chart(&dir))
        .and_then(|meta| resolved_schema(&meta, &sources).ok())
        .map(|app| app.config())
        .unwrap_or(Value::Null);
    record.values = crate::group_chart::redact(&record.values, &schema);
    Json(GroupView { record, schema }).into_response()
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    let record = match read_record(&client, &name).await {
        Ok(Some(r)) => r,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, format!("no group named {name}")),
        Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    };
    if busy(&record) {
        return refuse(
            StatusCode::CONFLICT,
            "this group is still being set up — wait until it finishes",
        );
    }
    for ns in record.members.values() {
        if let Err(e) = crate::groups::set(&client, ns, None).await {
            if !crate::k8s::refused_with(&e, 404) {
                return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
            }
        }
    }
    let secret = crate::k8s::reference("v1", "Secret", GROUPS_NS, &name);
    match crate::k8s::delete_if_present(&client, &secret).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str) -> MemberStatus {
        MemberStatus {
            state: state.into(),
            message: String::new(),
        }
    }

    #[test]
    fn a_group_record_lives_in_a_secret_because_its_choices_hold_passwords() {
        let record = GroupRecord {
            name: "movies-tv".into(),
            title: "Movies & TV".into(),
            chart: "movies-tv".into(),
            repo: "official".into(),
            version: "0.1.0".into(),
            ..Default::default()
        };
        let secret = record_secret(&record).unwrap();
        assert_eq!(secret["kind"], "Secret");
        assert_eq!(secret["metadata"]["namespace"], GROUPS_NS);
        assert_eq!(secret["metadata"]["labels"][LABEL_RECORD], "true");
        let text = secret["stringData"][RECORD_KEY].as_str().unwrap();
        assert_eq!(serde_json::from_str::<GroupRecord>(text).unwrap(), record);
    }

    #[test]
    fn a_stored_record_reads_back_from_the_secret_kubernetes_returns() {
        use base64::Engine as _;
        let record = GroupRecord {
            name: "ai".into(),
            ..Default::default()
        };
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&record).unwrap());
        let secret = serde_json::json!({ "data": { RECORD_KEY: encoded } });
        assert_eq!(record_of(&secret), Some(record));
        assert_eq!(record_of(&serde_json::json!({ "data": {} })), None);
    }

    #[test]
    fn the_group_template_sees_the_choices_and_where_each_app_lives() {
        let config = serde_json::json!({ "media_folder": "movies" });
        let apps: BTreeMap<String, String> = [(
            "qbittorrent".to_string(),
            "yolab-qbittorrent-ab12".to_string(),
        )]
        .into();
        assert_eq!(
            values_file(config.as_object().unwrap(), &apps),
            serde_json::json!({
                "config": { "media_folder": "movies" },
                "yolab": { "apps": { "qbittorrent": "yolab-qbittorrent-ab12" } },
            })
        );
    }

    #[test]
    fn a_group_that_is_still_being_set_up_is_busy() {
        let mut record = GroupRecord::default();
        assert!(!busy(&record));
        record.status.insert("a".into(), status(DONE));
        record.status.insert("b".into(), status(FAILED));
        assert!(!busy(&record));
        record.status.insert("c".into(), status(WORKING));
        assert!(busy(&record));
    }

    #[test]
    fn choices_that_break_the_groups_form_are_refused_before_anything_is_installed() {
        let app = crate::appschema::AppSchema::new(serde_json::json!({
            "properties": { "config": {
                "type": "object",
                "properties": { "media_folder": { "type": "string", "pattern": "^[a-z]*$" } },
                "required": ["media_folder"],
            }}
        }));
        let ok = serde_json::json!({ "media_folder": "movies" });
        assert!(check_config(&app, ok.as_object().unwrap()).is_ok());
        let bad = serde_json::json!({ "media_folder": "Movies!" });
        assert!(check_config(&app, bad.as_object().unwrap()).is_err());
        assert!(check_config(&app, &Map::new()).is_err());
    }
}
