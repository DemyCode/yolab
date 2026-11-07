use super::*;

pub(crate) fn uninstall_lock_is_fresh(ann: &serde_json::Map<String, Value>) -> bool {
    ann.get(ANN_UNINSTALLING)
        .and_then(|v| v.as_str())
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| {
            chrono::Utc::now().signed_duration_since(t).num_seconds()
                <= UNINSTALL_LOCK_TTL.as_secs() as i64
        })
        .unwrap_or(false)
}

pub(crate) fn removal_registered(ann: &serde_json::Map<String, Value>) -> bool {
    ann.contains_key(ANN_UNINSTALLING)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Removal {
    pub since: Option<String>,
    pub detail: String,
    pub technical: String,
}

pub(crate) fn removal_of(namespace: &Value, ann: &serde_json::Map<String, Value>) -> Option<Removal> {
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
    Some(Removal { since, detail, technical })
}

static DRIVING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

pub(crate) struct Driving(String);

impl Driving {
    pub(crate) fn take(ns: &str) -> Option<Self> {
        let mut driving = DRIVING.lock().unwrap_or_else(|e| e.into_inner());
        driving.insert(ns.to_string()).then(|| Driving(ns.to_string()))
    }
}

impl Drop for Driving {
    fn drop(&mut self) {
        DRIVING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

pub(crate) async fn drive_teardown<H: crate::host::Host>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    let Some(_driving) = Driving::take(ns) else {
        tracing::info!("uninstall {instance_name}: already being torn down here");
        return Ok(());
    };
    match run_teardown(b, instance_name, ns).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!("uninstall {instance_name}: {e:#}");
            annotate_ns(&b.kube, ns, ANN_UNINSTALL_FAILED, &format!("{e:#}")).await;
            Err(e)
        }
    }
}

pub(crate) async fn claim_uninstall_lock(
    client: &Client,
    ns: &str,
    delete: &[String],
) -> anyhow::Result<bool> {
    let Some(existing) = namespace(client, ns).await? else {
        return Ok(true);
    };
    let ann = existing["metadata"]["annotations"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if uninstall_lock_is_fresh(&ann) {
        return Ok(false);
    }
    if ann.contains_key(ANN_UNINSTALLING) {
        tracing::warn!("uninstall {ns}: reclaiming a stale uninstall lock");
    }
    let mut claim = namespace_ref(ns);
    claim["metadata"]["resourceVersion"] = existing["metadata"]["resourceVersion"].clone();
    let doomed = match delete {
        [] => Value::Null,
        folders => Value::String(serde_json::to_string(folders)?),
    };
    claim["metadata"]["annotations"] = serde_json::json!({
        ANN_UNINSTALLING: chrono::Utc::now().to_rfc3339(),
        ANN_DELETE_FOLDERS: doomed,
    });
    match crate::k8s::merge_patch(client, &claim).await {
        Ok(()) => Ok(true),
        Err(e) if crate::k8s::refused_with(&e, 409) => Ok(false),
        Err(e) if crate::k8s::refused_with(&e, 404) => Ok(true),
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
    pub delete_data: bool,
}

pub async fn uninstall_app(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<UninstallQuery>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>)> {
    let ns = format!("yolab-{instance_name}");
    let b = state.backend().await?;

    let doomed = if query.delete_data {
        crate::tree::own_folders(&crate::tree::volumes(&b.kube).await?, &ns)
    } else {
        Vec::new()
    };
    let accepted = (
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({"ok": true})),
    );
    if !claim_uninstall_lock(&b.kube, &ns, &doomed).await? {
        tracing::info!("uninstall {instance_name}: already registered");
        return Ok(accepted);
    }

    tokio::spawn(async move {
        if drive_teardown(&b, &instance_name, &ns).await.is_ok() {
            tracing::info!("uninstall {instance_name}: done");
        }
    });

    Ok(accepted)
}

pub async fn app_data(
    State(state): State<AppState>,
    Path(instance_name): Path<String>,
) -> Result<Json<crate::tree::DataVerdict>> {
    let ns = format!("yolab-{instance_name}");
    let b = state.backend().await?;
    let volumes = crate::tree::volumes(&b.kube).await?;
    let own = crate::tree::own_folders(&volumes, &ns);
    Ok(Json(crate::tree::data_verdict(&own, &volumes, &ns)))
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
    crate::tree::run(&b.kube, &crate::tree::Op::Remove(verdict.delete)).await
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

pub(crate) async fn run_teardown<H: crate::host::Host>(
    b: &Backend<H>,
    instance_name: &str,
    ns: &str,
) -> anyhow::Result<()> {
    let found = namespace(&b.kube, ns).await?;
    let doomed = match &found {
        Some(v) => folders_in(v)?,
        None => Vec::new(),
    };
    if found.is_some_and(|v| v["status"]["phase"] == "Terminating") {
        tracing::info!(
            "uninstall {instance_name}: namespace is already terminating — waiting for it \
             to finish rather than re-running helm"
        );
        delete_data(b, instance_name, ns, &doomed).await?;
        delete_namespace_with_retry(&b.kube, ns).await;
        release_tree_volumes(&b.kube, ns).await?;
        return wait_for_volumes_deleted(&b.kube, ns, VOLUME_DELETE_WAIT, VOLUME_DELETE_POLL).await;
    }

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

    delete_data(b, instance_name, ns, &doomed).await?;
    delete_namespace_with_retry(&b.kube, ns).await;
    release_tree_volumes(&b.kube, ns).await?;
    wait_for_volumes_deleted(&b.kube, ns, VOLUME_DELETE_WAIT, VOLUME_DELETE_POLL).await
}

pub(crate) fn abandoned_in(namespaces: &[Value]) -> Vec<(String, String)> {
    namespaces
        .iter()
        .filter_map(|ns| {
            let name = ns["metadata"]["name"].as_str()?;
            let ann = ns["metadata"]["annotations"].as_object()?;
            ann.get(ANN_UNINSTALLING)?;
            if uninstall_lock_is_fresh(ann) {
                return None;
            }
            let instance = name.strip_prefix("yolab-")?.to_string();
            Some((name.to_string(), instance))
        })
        .collect()
}

pub(crate) async fn abandoned_uninstalls(client: &Client) -> anyhow::Result<Vec<(String, String)>> {
    let managed = kube::api::ListParams::default().labels(&format!("{LABEL_MANAGED}=true"));
    let namespaces = crate::k8s::list(client, "v1", "Namespace", None, &managed).await?;
    Ok(abandoned_in(&namespaces))
}

pub struct UninstallWatchdogController;

impl crate::runtime::Controller for UninstallWatchdogController {
    fn name(&self) -> &'static str {
        "uninstall-watchdog"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    fn not_before_uptime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(90)
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        finish_abandoned_uninstalls(&Backend::real().await?).await
    }
}

pub(crate) async fn finish_abandoned_uninstalls<H: crate::host::Host>(
    b: &Backend<H>,
) -> anyhow::Result<crate::runtime::Tick> {
    let abandoned = abandoned_uninstalls(&b.kube).await?;
    if abandoned.is_empty() {
        return Ok(crate::runtime::Tick::Idle("no abandoned uninstalls".into()));
    }
    for (ns, instance) in abandoned {
        tracing::warn!(
            "uninstall {instance}: claim is stale and nothing is driving it — \
             finishing the teardown"
        );
        if drive_teardown(b, &instance, &ns).await.is_ok() {
            tracing::info!("uninstall {instance}: done");
        }
    }
    Ok(crate::runtime::Tick::Done)
}
