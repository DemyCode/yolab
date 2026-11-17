use super::*;

pub(crate) fn removal_registered(ann: &serde_json::Map<String, Value>) -> bool {
    ann.contains_key(ANN_UNINSTALLING)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Removal {
    pub since: Option<String>,
    pub detail: String,
    pub technical: String,
}

pub(crate) fn removal_of(
    namespace: &Value,
    ann: &serde_json::Map<String, Value>,
) -> Option<Removal> {
    let terminating = namespace["status"]["phase"] == "Terminating";
    if !terminating && !removal_registered(ann) {
        return None;
    }
    let since = namespace["metadata"]["deletionTimestamp"]
        .as_str()
        .or_else(|| ann.get(ANN_UNINSTALLING).and_then(Value::as_str))
        .map(str::to_string);
    let (detail, technical) = match ann.get(ANN_UNINSTALL_FAILED).and_then(Value::as_str) {
        Some(why) => (
            "The last try to remove it did not finish. It is tried again in the background."
                .to_string(),
            why.to_string(),
        ),
        None => (String::new(), String::new()),
    };
    Some(Removal {
        since,
        detail,
        technical,
    })
}

pub(crate) async fn drive_teardown<H: crate::host::Host + Clone + 'static>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    match run_teardown(b, instance_name, ns).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!("uninstall {instance_name}: {e:#}");
            annotate_ns(&b.kube, ns, ANN_UNINSTALL_FAILED, &format!("{e:#}")).await;
            Err(e)
        }
    }
}

pub(crate) fn uninstall_request(delete: &[String], delete_backups: bool) -> anyhow::Result<Value> {
    let doomed = match delete {
        [] => Value::Null,
        folders => Value::String(serde_json::to_string(folders)?),
    };
    let backups = if delete_backups {
        Value::String("true".into())
    } else {
        Value::Null
    };
    Ok(serde_json::json!({
        ANN_UNINSTALLING: chrono::Utc::now().to_rfc3339(),
        ANN_DELETE_FOLDERS: doomed,
        ANN_DELETE_BACKUPS: backups,
    }))
}

pub(crate) async fn register_uninstall(
    client: &Client,
    ns: &str,
    delete: &[String],
    delete_backups: bool,
) -> anyhow::Result<bool> {
    let Some(existing) = namespace(client, ns).await? else {
        return Ok(false);
    };
    let ann = existing["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if removal_registered(&ann) {
        return Ok(false);
    }
    let mut request = namespace_ref(ns);
    request["metadata"]["resourceVersion"] = existing["metadata"]["resourceVersion"].clone();
    request["metadata"]["annotations"] = uninstall_request(delete, delete_backups)?;
    match crate::k8s::merge_patch(client, &request).await {
        Ok(()) => Ok(true),
        Err(e) if crate::k8s::refused_with(&e, 409) => Ok(false),
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) const NAMESPACE_DELETE_ATTEMPTS: u32 = 4;
pub(crate) const NAMESPACE_DELETE_FIRST_RETRY: std::time::Duration =
    std::time::Duration::from_secs(2);

pub(crate) async fn delete_namespace_with_retry(client: &Client, ns: &str) {
    let mut delay = NAMESPACE_DELETE_FIRST_RETRY;
    for attempt in 1..=NAMESPACE_DELETE_ATTEMPTS {
        match crate::k8s::delete_if_present(client, &namespace_ref(ns)).await {
            Ok(()) => return,
            Err(e) if attempt == NAMESPACE_DELETE_ATTEMPTS => {
                tracing::warn!(
                    "uninstall {ns}: delete namespace failed after {NAMESPACE_DELETE_ATTEMPTS} \
                     attempts, leaving it for a future retry: {e}"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "uninstall {ns}: delete namespace attempt {attempt} failed, retrying: {e}"
                );
                tokio::time::sleep(delay).await;
                delay *= 3;
            }
        }
    }
}

#[derive(Deserialize, Default)]
pub struct UninstallQuery {
    #[serde(default)]
    pub delete_shared: bool,
    #[serde(default)]
    pub delete_backups: bool,
}

pub async fn uninstall_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<UninstallQuery>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>)> {
    let ns = format!("yolab-{instance_name}");
    let b = state.backend().await?;

    let volumes = crate::tree::volumes(&b.kube).await?;
    let folders = crate::tree::service_folders(&volumes, &ns);
    let doomed = crate::tree::data_verdict(&folders, &volumes, &ns).doomed(query.delete_shared);
    let accepted = (
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({"ok": true})),
    );
    if !register_uninstall(&b.kube, &ns, &doomed, query.delete_backups).await? {
        tracing::info!("uninstall {instance_name}: already registered or already gone");
    }
    crate::runtime::wake("uninstalls");
    Ok(accepted)
}

async fn delete_backups_of<H: crate::host::Host + Clone + 'static>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) {
    let cfg = match crate::routers::backup_common::master_config(&b.kube).await {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                "uninstall {instance_name}: its backups were kept, the backup settings are unreadable: {e:#}"
            );
            return;
        }
    };
    match crate::routers::backup_points::delete_all(b, &cfg, ns).await {
        Ok(n) => tracing::info!("uninstall {instance_name}: deleted its {n} backups"),
        Err(e) => tracing::warn!(
            "uninstall {instance_name}: its backups were kept and can be deleted from the Backups page: {e:#}"
        ),
    }
}

pub async fn app_data(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<crate::tree::DataVerdict>> {
    let ns = format!("yolab-{instance_name}");
    let b = state.backend().await?;
    let volumes = crate::tree::volumes(&b.kube).await?;
    let folders = crate::tree::service_folders(&volumes, &ns);
    Ok(Json(crate::tree::data_verdict(&folders, &volumes, &ns)))
}

pub(crate) fn folders_in(namespace: &Value) -> anyhow::Result<Vec<String>> {
    match namespace["metadata"]["annotations"][ANN_DELETE_FOLDERS].as_str() {
        None => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("{ANN_DELETE_FOLDERS} is not a list of folders: {e}")),
    }
}

async fn delete_data<H: crate::host::Host>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
    doomed: &[String],
) -> anyhow::Result<()> {
    if doomed.is_empty() {
        return Ok(());
    }
    let volumes = crate::tree::volumes(&b.kube).await?;
    let verdict = crate::tree::data_verdict(doomed, &volumes, ns);
    for kept in &verdict.kept {
        tracing::info!(
            "uninstall {instance_name}: keeping {} because {} uses it",
            kept.folder,
            kept.used_by.join(", ")
        );
    }
    crate::tree::run(&b.kube, &verdict.removal(true)).await
}

pub(crate) async fn release_tree_volumes(client: &Client, ns: &str) -> anyhow::Result<()> {
    let volumes = crate::tree::volumes(client).await?;
    let names = volumes
        .iter()
        .filter(|v| v["spec"]["claimRef"]["namespace"].as_str() == Some(ns))
        .filter(|v| crate::tree::folder_of(v).is_some())
        .filter_map(|v| v["metadata"]["name"].as_str());
    for name in names {
        let volume = crate::k8s::cluster_reference("v1", "PersistentVolume", name);
        crate::k8s::delete_if_present(client, &volume).await?;
    }
    Ok(())
}

pub(crate) const HELM_UNINSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) const VOLUME_DELETE_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
pub(crate) const VOLUME_DELETE_POLL: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) fn volumes_of(pvs: &[Value], ns: &str) -> Vec<String> {
    pvs.iter()
        .filter(|pv| pv["spec"]["claimRef"]["namespace"].as_str() == Some(ns))
        .filter_map(|pv| pv["metadata"]["name"].as_str().map(str::to_string))
        .collect()
}

pub(crate) async fn wait_for_volumes_deleted(
    client: &Client,
    ns: &str,
    wait: std::time::Duration,
    every: std::time::Duration,
) -> anyhow::Result<()> {
    crate::poll::until(wait, every, move || async move {
        match crate::k8s::list(client, "v1", "PersistentVolume", None, &Default::default()).await {
            Ok(pvs) => match volumes_of(&pvs, ns) {
                left if left.is_empty() => crate::poll::Step::Done(()),
                left => crate::poll::Step::Pending(anyhow::anyhow!(
                    "the app is gone but its volumes {} were not deleted, so their data is still on the disks",
                    left.join(", ")
                )),
            },
            Err(e) => crate::poll::Step::Pending(anyhow::anyhow!(
                "the app is gone but whether its volumes were deleted could not be checked: {e}"
            )),
        }
    })
    .await
}

pub(crate) fn backups_doomed(namespace: &Value) -> bool {
    namespace["metadata"]["annotations"][ANN_DELETE_BACKUPS] == "true"
}

pub(crate) async fn run_teardown<H: crate::host::Host + Clone + 'static>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    let found = namespace(&b.kube, ns).await?;
    let doomed = match &found {
        Some(v) => folders_in(v)?,
        None => Vec::new(),
    };
    let delete_backups = found.as_ref().is_some_and(backups_doomed);
    if found.is_some_and(|v| v["status"]["phase"] == "Terminating") {
        tracing::info!(
            "uninstall {instance_name}: namespace is already terminating — waiting for it \
             to finish rather than re-running helm"
        );
    } else {
        helm_uninstall(b, instance_name, ns).await?;
    }

    delete_data(b, instance_name, ns, &doomed).await?;
    if delete_backups {
        delete_backups_of(b, instance_name, ns).await;
    }
    delete_namespace_with_retry(&b.kube, ns).await;
    release_tree_volumes(&b.kube, ns).await?;
    wait_for_volumes_deleted(&b.kube, ns, VOLUME_DELETE_WAIT, VOLUME_DELETE_POLL).await
}

async fn helm_uninstall<H: crate::host::Host>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    let release = read_definition_opt(&b.kube, ns)
        .await?
        .map(|d| d.release().to_string())
        .unwrap_or_else(|| instance_name.to_string());
    let out = b
        .host
        .run_cmd_bounded(
            "helm",
            &[
                "uninstall",
                &release,
                "-n",
                ns,
                "--ignore-not-found",
                "--wait",
            ],
            HELM_UNINSTALL_TIMEOUT,
        )
        .await;
    match out {
        Ok(o) if !o.success => tracing::warn!(
            "uninstall {instance_name}: helm uninstall failed: {}",
            o.stderr.trim()
        ),
        Err(e) => tracing::warn!(
            "uninstall {instance_name}: helm uninstall did not finish ({e}) — deleting the namespace anyway"
        ),
        Ok(_) => {}
    }
    Ok(())
}

pub(crate) fn registered_in(namespaces: &[Value]) -> Vec<(String, String)> {
    namespaces
        .iter()
        .filter_map(|ns| {
            let name = ns["metadata"]["name"].as_str()?;
            let ann = ns["metadata"]["annotations"].as_object()?;
            if !removal_registered(ann) {
                return None;
            }
            let instance = name.strip_prefix("yolab-")?.to_string();
            Some((name.to_string(), instance))
        })
        .collect()
}

pub(crate) async fn registered_uninstalls(client: &Client) -> anyhow::Result<Vec<(String, String)>> {
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let namespaces = crate::k8s::list(client, "v1", "Namespace", None, &managed).await?;
    Ok(registered_in(&namespaces))
}

pub struct UninstallsController;

impl crate::runtime::Controller for UninstallsController {
    fn name(&self) -> &'static str {
        "uninstalls"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        finish_uninstalls(&ctx.tools.backend().await?).await
    }
}

pub(crate) async fn finish_uninstalls<H: crate::host::Host + Clone + 'static>(
    b: &Backend<H>,
) -> anyhow::Result<crate::runtime::Tick> {
    let registered = registered_uninstalls(&b.kube).await?;
    if registered.is_empty() {
        return Ok(crate::runtime::Tick::Idle("no service is being removed".into()));
    }
    for (ns, instance) in registered {
        if drive_teardown(b, &instance, &ns).await.is_ok() {
            tracing::info!("uninstall {instance}: done");
        }
    }
    Ok(crate::runtime::Tick::Done)
}
