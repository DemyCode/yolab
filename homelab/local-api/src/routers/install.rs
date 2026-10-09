use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::Config;
use crate::host::Host;
use crate::routers::apps::{
    app_schema, clear_install_failed, collect_runtime, mark_install_failed, merge_credentials,
    stage_install, write_definition, AppDefinition, BackupPolicy, ChartAt, StagedInstall,
    DEFINITION_SCHEMA,
};
use crate::routers::backup_common::Backend;

const HELM_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_LABEL_LEN: usize = 63;
const MAX_SNAPSHOT_ID_LEN: usize = 64;

#[derive(Deserialize, Clone, Default, Debug)]
pub struct InstallSource {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub from_instance: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    #[serde(default)]
    pub with_data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Fresh,
    Duplicate,
    Backup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigOrigin {
    Fresh,
    LiveApp {
        namespace: String,
    },
    Backup {
        namespace: String,
        snapshot_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DataOrigin {
    Backup {
        namespace: String,
        snapshot_id: String,
    },
    Live {
        namespace: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sources {
    pub(crate) config: ConfigOrigin,
    pub(crate) data: Option<DataOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChartPin {
    Newest,
    Saved {
        repo: String,
        version: String,
        tgz: Vec<u8>,
    },
    Exact {
        repo: String,
        version: String,
    },
}

impl ChartPin {
    pub(crate) fn recorded(&self) -> Option<(&str, &str)> {
        match self {
            ChartPin::Newest => None,
            ChartPin::Saved { repo, version, .. } | ChartPin::Exact { repo, version } => {
                Some((repo, version))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) definition: AppDefinition,
    pub(crate) chart: Option<Vec<u8>>,
    pub(crate) schema: Option<Value>,
}

pub(crate) fn pin_chart(source: Option<&Source>) -> Result<ChartPin, String> {
    let Some(source) = source else {
        return Ok(ChartPin::Newest);
    };
    let def = &source.definition;
    let repo = if def.chart_repo.is_empty() {
        crate::charts::OFFICIAL.to_string()
    } else {
        def.chart_repo.clone()
    };
    let version = def.chart_version.clone();
    if let Some(tgz) = &source.chart {
        return Ok(ChartPin::Saved {
            repo,
            version,
            tgz: tgz.clone(),
        });
    }
    if version.is_empty() {
        return Err(
            "the app you are copying never recorded which version of its chart it runs, so it cannot be copied exactly"
                .to_string(),
        );
    }
    Ok(ChartPin::Exact { repo, version })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallPlan {
    pub(crate) app_id: String,
    pub(crate) instance_name: String,
    pub(crate) release: String,
    pub(crate) config: Map<String, Value>,
    pub(crate) backup: BackupPolicy,
    pub(crate) data: Option<DataOrigin>,
    pub(crate) chart: ChartPin,
}

fn parse_kind(kind: &str) -> Result<SourceKind, String> {
    match kind {
        "" | "fresh" => Ok(SourceKind::Fresh),
        "duplicate" => Ok(SourceKind::Duplicate),
        "backup" => Ok(SourceKind::Backup),
        other => Err(format!(
            "{other:?} is not somewhere an app can be installed from"
        )),
    }
}

pub(crate) fn is_instance_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LABEL_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn given(field: &Option<String>) -> Option<String> {
    field
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn data_origin(
    with_data: bool,
    namespace: &str,
    snapshot_id: Option<String>,
) -> Result<Option<DataOrigin>, String> {
    if !with_data {
        return Ok(None);
    }
    let snapshot_id = snapshot_id
        .ok_or_else(|| "pick the backup this app's data should be copied from".to_string())?;
    Ok(Some(DataOrigin::Backup {
        namespace: namespace.to_string(),
        snapshot_id,
    }))
}

pub(crate) fn resolve_sources(source: Option<&InstallSource>) -> Result<Sources, String> {
    let Some(source) = source else {
        return Ok(Sources {
            config: ConfigOrigin::Fresh,
            data: None,
        });
    };
    match parse_kind(&source.kind)? {
        SourceKind::Fresh => {
            if source.with_data {
                return Err("a new app starts empty — there is no data to copy".to_string());
            }
            Ok(Sources {
                config: ConfigOrigin::Fresh,
                data: None,
            })
        }
        SourceKind::Duplicate => {
            let from = given(&source.from_instance)
                .ok_or_else(|| "which app should be duplicated?".to_string())?;
            if !is_instance_name(&from) {
                return Err(format!("{from:?} is not the name of an app"));
            }
            let namespace = format!("yolab-{from}");
            let data = source.with_data.then(|| DataOrigin::Live {
                namespace: namespace.clone(),
            });
            Ok(Sources {
                config: ConfigOrigin::LiveApp { namespace },
                data,
            })
        }
        SourceKind::Backup => {
            let namespace = given(&source.namespace)
                .ok_or_else(|| "which app should be restored?".to_string())?;
            let snapshot_id = given(&source.snapshot_id)
                .ok_or_else(|| "which backup should this app be restored from?".to_string())?;
            check_backup_ref(&namespace, &snapshot_id)?;
            let data = data_origin(source.with_data, &namespace, Some(snapshot_id.clone()))?;
            Ok(Sources {
                config: ConfigOrigin::Backup {
                    namespace,
                    snapshot_id,
                },
                data,
            })
        }
    }
}

pub(crate) fn is_app_namespace(namespace: &str) -> bool {
    namespace
        .strip_prefix("yolab-")
        .is_some_and(is_instance_name)
}

pub(crate) fn check_backup_ref(namespace: &str, snapshot_id: &str) -> Result<(), String> {
    if !is_app_namespace(namespace) {
        return Err(format!("{namespace:?} is not an app namespace"));
    }
    if snapshot_id.is_empty()
        || snapshot_id.len() > MAX_SNAPSHOT_ID_LEN
        || !snapshot_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(format!("{snapshot_id:?} is not a backup"));
    }
    Ok(())
}

pub(crate) fn same_app(expected: &str, found: &str, what: &str) -> Result<(), String> {
    if found.is_empty() || found == expected {
        return Ok(());
    }
    Err(format!("{what} is a {found}, not a {expected}"))
}

pub(crate) fn plan(
    app_id: &str,
    instance_name: &str,
    config: Map<String, Value>,
    source: Option<&Source>,
    data: Option<DataOrigin>,
    app: &crate::appschema::AppSchema,
) -> Result<InstallPlan, String> {
    let mut plan = InstallPlan {
        app_id: app_id.to_string(),
        instance_name: instance_name.to_string(),
        release: source
            .map(|s| s.definition.release())
            .unwrap_or(instance_name)
            .to_string(),
        config,
        backup: BackupPolicy::default(),
        data,
        chart: pin_chart(source)?,
    };
    let Some(source) = source.map(|s| &s.definition) else {
        return Ok(plan);
    };
    same_app(app_id, &source.app_id, "the app you are copying")?;
    plan.config = merge_credentials(plan.config, &source.config, app);
    plan.backup = source.backup.clone();
    Ok(plan)
}

pub(crate) async fn source<H: Host>(
    b: &Backend<H>,
    origin: &ConfigOrigin,
) -> anyhow::Result<Option<Source>> {
    match origin {
        ConfigOrigin::Fresh => Ok(None),
        ConfigOrigin::LiveApp { namespace } => {
            let definition = crate::routers::apps::read_definition(&b.kube, namespace).await?;
            let chart = crate::saved_chart::read(&b.kube, namespace).await?;
            let schema = crate::saved_chart::read_schema(&b.kube, namespace).await?;
            Ok(Some(Source {
                definition,
                chart,
                schema,
            }))
        }
        ConfigOrigin::Backup {
            namespace,
            snapshot_id,
        } => {
            let definition =
                crate::routers::restore::definition_from_backup(b, namespace, snapshot_id).await?;
            let (chart, schema) =
                crate::routers::restore::kept_from_backup(b, namespace, snapshot_id).await?;
            Ok(Some(Source {
                definition,
                chart,
                schema,
            }))
        }
    }
}

struct PinnedChart {
    repo: String,
    dir: std::path::PathBuf,
    _unpacked: Option<crate::saved_chart::Unpacked>,
    _pulled: Option<tempfile::TempDir>,
}

impl PinnedChart {
    fn at(&self) -> ChartAt<'_> {
        ChartAt::Dir {
            repo: &self.repo,
            dir: &self.dir,
        }
    }
}

async fn pinned_chart<H: Host>(
    b: &Backend<H>,
    app_id: &str,
    pin: &ChartPin,
    log: &Log,
) -> anyhow::Result<Option<PinnedChart>> {
    match pin {
        ChartPin::Newest => Ok(None),
        ChartPin::Saved { repo, tgz, .. } => {
            log.say("Using the exact chart the original runs…");
            let unpacked = crate::saved_chart::unpack(&b.host, tgz, app_id).await?;
            Ok(Some(PinnedChart {
                repo: repo.clone(),
                dir: unpacked.chart_dir().to_path_buf(),
                _unpacked: Some(unpacked),
                _pulled: None,
            }))
        }
        ChartPin::Exact { repo, version } => {
            let cache = std::path::Path::new(crate::charts::CACHE_DIR);
            if let Some(dir) = crate::charts::cached_at_version(cache, repo, app_id, version) {
                return Ok(Some(PinnedChart {
                    repo: repo.clone(),
                    dir,
                    _unpacked: None,
                    _pulled: None,
                }));
            }
            log.say(format!(
                "Fetching version {version} of {app_id}, the one the original runs…"
            ));
            let pulled = tempfile::tempdir()?;
            let repos = crate::charts::list_repos(&b.kube).await;
            let dir = crate::charts::fetch_exact(
                &b.host,
                pulled.path(),
                &repos,
                app_id,
                version,
                repo,
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "no copy of this app's chart was kept, and version {version} could not be fetched again: {e:#}"
                )
            })?;
            Ok(Some(PinnedChart {
                repo: repo.clone(),
                dir,
                _unpacked: None,
                _pulled: Some(pulled),
            }))
        }
    }
}

pub(crate) struct Log(tokio::sync::mpsc::UnboundedSender<String>);

impl Log {
    pub(crate) fn to_tracing(subject: String) -> Log {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                tracing::info!("{subject}: {line}");
            }
        });
        Log(tx)
    }

    pub(crate) fn say(&self, line: impl Into<String>) {
        let _ = self.0.send(line.into());
    }
}

struct ChartJob<'a> {
    app_id: &'a str,
    instance_name: &'a str,
    release: &'a str,
    config: &'a Map<String, Value>,
    chart: ChartAt<'a>,
    backup: &'a BackupPolicy,
    verb: &'static str,
}

enum DataFill<'a> {
    None,
    Backup(Box<crate::routers::restore::BackupPayload>),
    Live { source_namespace: &'a str },
}

async fn apply_chart<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    job: &ChartJob<'_>,
    fill: &DataFill<'_>,
    log: &Log,
) -> anyhow::Result<String> {
    let staged = stage_install(
        &b.kube,
        cfg,
        job.app_id,
        job.instance_name,
        job.config,
        &job.chart,
    )
    .await?;

    match fill {
        DataFill::Backup(payload) => {
            log.say("Copying this app's files…");
            payload.fill_volumes(b, &staged.ns, job.release).await?;
        }
        DataFill::Live { source_namespace } => {
            log.say("Copying this app's files…");
            crate::routers::copy::copy_live_volumes(
                &b.kube,
                source_namespace,
                &staged.ns,
                job.release,
            )
            .await?;
        }
        DataFill::None => {}
    }

    log.say(job.verb);
    helm_install(&b.host, &staged, job.release, log).await?;

    if let DataFill::Backup(payload) = fill {
        log.say("Putting this app's saved settings back…");
        payload.reapply(&b.kube).await?;
    }

    if let Err(e) = crate::saved_chart::save(&b.host, &b.kube, &staged.ns, &staged.chart_dir).await
    {
        log.say(format!(
            "[WARN] could not keep a copy of this app's chart ({e:#}) — copying this app later will fetch version {} again",
            staged.chart_version
        ));
    }

    let (volumes, resources) = collect_runtime(&b.kube, &staged.ns).await;
    let definition = AppDefinition {
        schema: DEFINITION_SCHEMA,
        app_id: job.app_id.to_string(),
        chart_repo: staged.chart_repo.clone(),
        chart_version: staged.chart_version.clone(),
        instance_name: job.instance_name.to_string(),
        release: job.release.to_string(),
        service_name: staged.service_name.clone(),
        config: job.config.clone(),
        volumes,
        resources,
        backup: job.backup.clone(),
    };
    write_definition(
        &b.kube,
        &staged.ns,
        &definition,
        &app_schema(
            staged
                .chart_dir
                .parent()
                .unwrap_or(cfg.catalog_dir().as_path()),
            job.app_id,
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("save this app's settings: {e}"))?;
    Ok(staged.chart_version)
}

pub(crate) async fn execute<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    plan: &InstallPlan,
    log: &Log,
) -> anyhow::Result<()> {
    let outcome = install_inner(b, cfg, plan, log).await;
    if let Err(e) = &outcome {
        mark_install_failed(
            &b.kube,
            &format!("yolab-{}", plan.instance_name),
            &format!("{e:#}"),
        )
        .await;
    }
    outcome
}

async fn install_inner<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    plan: &InstallPlan,
    log: &Log,
) -> anyhow::Result<()> {
    log.say("Getting things ready…");
    let fill = match &plan.data {
        Some(DataOrigin::Backup {
            namespace,
            snapshot_id,
        }) => {
            log.say("Reading the backup…");
            let payload =
                crate::routers::restore::backup_payload(b, namespace, snapshot_id).await?;
            same_app(&plan.app_id, payload.app_id(), "that backup")
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            DataFill::Backup(Box::new(payload))
        }
        Some(DataOrigin::Live { namespace }) => DataFill::Live {
            source_namespace: namespace.as_str(),
        },
        None => DataFill::None,
    };

    let pinned = pinned_chart(b, &plan.app_id, &plan.chart, log).await?;
    let job = ChartJob {
        app_id: &plan.app_id,
        instance_name: &plan.instance_name,
        release: &plan.release,
        config: &plan.config,
        chart: match &pinned {
            Some(p) => p.at(),
            None => ChartAt::Catalog { repo: None },
        },
        backup: &plan.backup,
        verb: "Installing…",
    };
    apply_chart(b, cfg, &job, &fill, log).await?;

    let namespace = format!("yolab-{}", plan.instance_name);
    if let Err(e) = crate::routers::backups::setup_namespace_backup(b, &namespace).await {
        log.say(format!(
            "[WARN] backups are not wired up for this app yet ({e}) — it will be picked up automatically within the hour"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpgradePlan {
    pub(crate) app_id: String,
    pub(crate) instance_name: String,
    pub(crate) release: String,
    pub(crate) config: Map<String, Value>,
    pub(crate) chart_repo: Option<String>,
    pub(crate) backup: BackupPolicy,
    pub(crate) keep_version: bool,
}

pub(crate) async fn upgrade<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    plan: &UpgradePlan,
    log: &Log,
) -> anyhow::Result<()> {
    let from = plan.chart_repo.as_deref();
    let pinned = if plan.keep_version {
        log.say("Keeping the version it runs now…");
        let namespace = format!("yolab-{}", plan.instance_name);
        let current = source(b, &ConfigOrigin::LiveApp { namespace }).await?;
        let pin = pin_chart(current.as_ref()).map_err(|_| {
            anyhow::anyhow!(
                "this app never recorded which version of its chart it runs, so its settings can only change with an update to the newest version"
            )
        })?;
        pinned_chart(b, &plan.app_id, &pin, log).await?
    } else {
        log.say("Fetching the newest version…");
        let repos = crate::charts::list_repos(&b.kube).await;
        if let Err(e) = crate::charts::fetch_newest(
            &b.host,
            std::path::Path::new(crate::charts::CACHE_DIR),
            &repos,
            &plan.app_id,
            from,
        )
        .await
        {
            log.say(format!(
                "Could not fetch the newest version ({e:#}) — using the one fetched last"
            ));
        }
        None
    };
    let job = ChartJob {
        app_id: &plan.app_id,
        instance_name: &plan.instance_name,
        release: &plan.release,
        config: &plan.config,
        chart: pinned
            .as_ref()
            .map(PinnedChart::at)
            .unwrap_or(ChartAt::Catalog { repo: from }),
        backup: &plan.backup,
        verb: "Updating…",
    };
    let version = apply_chart(b, cfg, &job, &DataFill::None, log).await?;
    clear_install_failed(&b.kube, &format!("yolab-{}", plan.instance_name)).await;
    log.say(format!("Now on version {version}"));
    Ok(())
}

async fn helm_install<H: Host>(
    host: &H,
    staged: &StagedInstall,
    release: &str,
    log: &Log,
) -> anyhow::Result<()> {
    let chart_dir = staged.chart_dir.to_string_lossy();
    let values = staged.values.path().to_string_lossy();
    let helm_said = std::sync::Mutex::new(None::<String>);
    let say = |line: String| {
        if let Some(error) = helm_error(&line) {
            *helm_said.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
        log.say(line)
    };
    let finished = host
        .run_lines(
            "helm",
            &[
                "upgrade",
                "--install",
                "--dependency-update",
                release,
                &chart_dir,
                "-n",
                &staged.ns,
                "--values",
                &values,
            ],
            HELM_TIMEOUT,
            &say,
        )
        .await;
    match finished {
        Ok(true) => Ok(()),
        Ok(false) => match helm_said.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(error) => anyhow::bail!("{release} could not be installed: {error}"),
            None => anyhow::bail!("{release} could not be installed — the log above is helm's own"),
        },
        Err(crate::exec::CmdError::Timeout { .. }) => {
            anyhow::bail!("installing {release} took too long and was stopped")
        }
        Err(e) => anyhow::bail!("could not run helm: {e}"),
    }
}

fn helm_error(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix("Error:")
        .map(|rest| rest.trim().to_string())
        .filter(|rest| !rest.is_empty())
}

pub(crate) fn verdict(
    outcome: Option<anyhow::Result<()>>,
    done: &str,
    after_failure: &str,
) -> String {
    match outcome {
        Some(Ok(())) => format!("[DONE] {done}"),
        Some(Err(e)) => format!("[ERROR] {e}. {after_failure}"),
        None => format!("[ERROR] it stopped before it finished. {after_failure}"),
    }
}

fn sse<F, Fut>(
    subject: String,
    work: F,
    done: String,
    after_failure: &'static str,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>>
where
    F: FnOnce(Log) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let running = work(Log(tx));
    let task = tokio::spawn(async move {
        let outcome = running.await;
        match &outcome {
            Ok(()) => tracing::info!("{subject}: finished"),
            Err(e) => tracing::warn!("{subject}: failed: {e:#}"),
        }
        outcome
    });
    async_stream::stream! {
        while let Some(line) = rx.recv().await {
            yield Ok(Event::default().data(line));
        }
        let outcome = task.await.ok();
        yield Ok(Event::default().data(verdict(outcome, &done, after_failure)));
    }
}

pub(crate) fn start(
    b: Backend,
    cfg: Arc<Config>,
    plan: InstallPlan,
    http: crate::http::Client,
) -> tokio::task::JoinHandle<()> {
    let subject = format!("install yolab-{}", plan.instance_name);
    let ns = format!("yolab-{}", plan.instance_name);
    let kube = b.kube.clone();
    let log = Log::to_tracing(subject.clone());
    run_detached(kube, ns, subject, async move {
        execute(&b, &cfg, &plan, &log).await?;
        crate::routers::store::report_install(&b.kube, &http, &cfg, &plan.app_id).await;
        Ok(())
    })
}

pub(crate) fn run_detached<F>(
    kube: kube::Client,
    ns: String,
    subject: String,
    work: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        match tokio::spawn(work).await {
            Ok(Ok(())) => tracing::info!("{subject}: finished"),
            Ok(Err(e)) => tracing::warn!("{subject}: failed: {e:#}"),
            Err(e) => {
                mark_install_failed(
                    &kube,
                    &ns,
                    &format!("the install stopped unexpectedly: {e}"),
                )
                .await
            }
        }
    })
}

pub(crate) fn upgrade_stream(
    b: Backend,
    cfg: Arc<Config>,
    plan: UpgradePlan,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    let done = format!("{} updated", plan.app_id);
    sse(
        format!("update yolab-{}", plan.instance_name),
        move |log| async move { upgrade(&b, &cfg, &plan, &log).await },
        done,
        "Your app was left as it was.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn helm_s_own_error_line_becomes_the_failure_reason() {
        assert_eq!(
            helm_error("Error: Deployment.apps \"steam-headless\" is invalid: ports[5].name: must be no more than 15 characters").as_deref(),
            Some("Deployment.apps \"steam-headless\" is invalid: ports[5].name: must be no more than 15 characters")
        );
        assert_eq!(helm_error("Release \"x\" does not exist. Installing it now."), None);
        assert_eq!(helm_error("Error:   "), None);
    }

    fn no_schema() -> crate::appschema::AppSchema {
        crate::appschema::AppSchema::new(Value::Null)
    }

    fn source(kind: &str) -> InstallSource {
        InstallSource {
            kind: kind.to_string(),
            ..Default::default()
        }
    }

    fn definition(app_id: &str) -> AppDefinition {
        AppDefinition {
            schema: DEFINITION_SCHEMA,
            app_id: app_id.to_string(),
            chart_repo: "official".into(),
            chart_version: "1.0.0".into(),
            instance_name: format!("{app_id}-ab12"),
            release: String::new(),
            service_name: String::new(),
            config: Map::new(),
            volumes: Vec::new(),
            resources: Default::default(),
            backup: BackupPolicy::default(),
        }
    }

    fn copied(def: &AppDefinition) -> Source {
        Source {
            definition: def.clone(),
            chart: None,
            schema: None,
        }
    }

    #[test]
    fn a_fresh_install_releases_under_its_own_instance_name() {
        let plan = plan("gitea", "gitea-ab12", Map::new(), None, None, &no_schema()).unwrap();
        assert_eq!(plan.release, "gitea-ab12");
    }

    #[test]
    fn a_copy_keeps_the_originals_release_so_its_cloned_bytes_line_up() {
        let mut def = definition("gitea");
        def.instance_name = "gitea-cd34".into();
        def.release = "gitea-ab12".into();
        let plan = plan(
            "gitea",
            "gitea-ef56",
            Map::new(),
            Some(&copied(&def)),
            None,
            &no_schema(),
        )
        .unwrap();
        assert_eq!(plan.instance_name, "gitea-ef56");
        assert_eq!(plan.release, "gitea-ab12");
    }

    #[test]
    fn an_app_saved_before_releases_were_recorded_released_under_its_instance_name() {
        let saved: AppDefinition = serde_json::from_value(json!({
            "schema": DEFINITION_SCHEMA,
            "app_id": "gitea",
            "instance_name": "gitea-ab12",
            "config": {}
        }))
        .unwrap();
        assert_eq!(saved.release(), "gitea-ab12");
        let plan = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&AppDefinition {
                chart_version: "1.0.0".into(),
                ..saved
            })),
            None,
            &no_schema(),
        )
        .unwrap();
        assert_eq!(plan.release, "gitea-ab12");
    }

    #[test]
    fn a_fresh_install_takes_the_newest_chart_from_the_catalog() {
        assert_eq!(pin_chart(None), Ok(ChartPin::Newest));
        let plan = plan("gitea", "gitea-ab12", Map::new(), None, None, &no_schema()).unwrap();
        assert_eq!(plan.chart, ChartPin::Newest);
        assert_eq!(plan.chart.recorded(), None);
    }

    #[test]
    fn a_copy_installs_the_chart_kept_with_the_original_not_the_catalogs() {
        let source = Source {
            definition: definition("gitea"),
            chart: Some(vec![1, 2, 3]),
            schema: None,
        };
        assert_eq!(
            pin_chart(Some(&source)),
            Ok(ChartPin::Saved {
                repo: "official".into(),
                version: "1.0.0".into(),
                tgz: vec![1, 2, 3],
            })
        );
    }

    #[test]
    fn a_copy_of_an_app_with_no_kept_chart_fetches_the_originals_exact_version() {
        let mut def = definition("gitea");
        def.chart_repo = "community".into();
        def.chart_version = "0.2.7".into();
        let plan = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&def)),
            None,
            &no_schema(),
        )
        .unwrap();
        assert_eq!(
            plan.chart,
            ChartPin::Exact {
                repo: "community".into(),
                version: "0.2.7".into(),
            }
        );
        assert_eq!(plan.chart.recorded(), Some(("community", "0.2.7")));
    }

    #[test]
    fn a_copy_whose_repository_was_never_recorded_came_from_the_official_catalog() {
        let mut def = definition("gitea");
        def.chart_repo = String::new();
        assert_eq!(
            pin_chart(Some(&copied(&def))),
            Ok(ChartPin::Exact {
                repo: crate::charts::OFFICIAL.into(),
                version: "1.0.0".into(),
            })
        );
    }

    #[test]
    fn a_copy_that_cannot_name_its_chart_version_is_refused_rather_than_upgraded() {
        let mut def = definition("gitea");
        def.chart_version = String::new();
        let err = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&def)),
            None,
            &no_schema(),
        )
        .unwrap_err();
        assert!(err.contains("version"), "{err}");
    }

    #[test]
    fn a_plain_install_reads_nothing_and_copies_nothing() {
        assert_eq!(
            resolve_sources(None).unwrap(),
            Sources {
                config: ConfigOrigin::Fresh,
                data: None
            }
        );
        assert_eq!(
            resolve_sources(Some(&source(""))).unwrap(),
            Sources {
                config: ConfigOrigin::Fresh,
                data: None
            }
        );
    }

    #[test]
    fn a_brand_new_app_has_no_data_to_ask_for() {
        let src = InstallSource {
            with_data: true,
            ..source("fresh")
        };
        assert!(resolve_sources(Some(&src)).is_err());
    }

    #[test]
    fn duplicating_without_data_still_reads_the_living_app() {
        let src = InstallSource {
            from_instance: Some("gitea-ab12".into()),
            ..source("duplicate")
        };
        assert_eq!(
            resolve_sources(Some(&src)).unwrap(),
            Sources {
                config: ConfigOrigin::LiveApp {
                    namespace: "yolab-gitea-ab12".into()
                },
                data: None,
            }
        );
    }

    #[test]
    fn duplicating_with_data_copies_the_originals_live_files() {
        let src = InstallSource {
            from_instance: Some("gitea-ab12".into()),
            with_data: true,
            ..source("duplicate")
        };
        let sources = resolve_sources(Some(&src)).unwrap();
        assert_eq!(
            sources.data,
            Some(DataOrigin::Live {
                namespace: "yolab-gitea-ab12".into(),
            })
        );
    }

    #[test]
    fn duplicating_with_data_needs_no_backup_to_copy_from() {
        let src = InstallSource {
            from_instance: Some("gitea-ab12".into()),
            with_data: true,
            ..source("duplicate")
        };
        assert!(resolve_sources(Some(&src)).is_ok());
    }

    #[test]
    fn duplicating_needs_to_know_what_to_duplicate() {
        assert!(resolve_sources(Some(&source("duplicate"))).is_err());
        let blank = InstallSource {
            from_instance: Some("   ".into()),
            ..source("duplicate")
        };
        assert!(resolve_sources(Some(&blank)).is_err());
    }

    #[test]
    fn restoring_from_a_backup_without_its_data_is_allowed() {
        let src = InstallSource {
            namespace: Some("yolab-gitea-ab12".into()),
            snapshot_id: Some("deadbeef".into()),
            with_data: false,
            ..source("backup")
        };
        let sources = resolve_sources(Some(&src)).unwrap();
        assert_eq!(
            sources.config,
            ConfigOrigin::Backup {
                namespace: "yolab-gitea-ab12".into(),
                snapshot_id: "deadbeef".into(),
            },
            "its settings still come from the backup"
        );
        assert_eq!(sources.data, None, "but none of its files do");
    }

    #[test]
    fn restoring_from_a_backup_with_its_data_uses_the_same_snapshot() {
        let src = InstallSource {
            namespace: Some("yolab-gitea-ab12".into()),
            snapshot_id: Some("deadbeef".into()),
            with_data: true,
            ..source("backup")
        };
        let sources = resolve_sources(Some(&src)).unwrap();
        assert_eq!(
            sources.data,
            Some(DataOrigin::Backup {
                namespace: "yolab-gitea-ab12".into(),
                snapshot_id: "deadbeef".into(),
            })
        );
    }

    #[test]
    fn restoring_needs_both_an_app_and_a_backup() {
        assert!(resolve_sources(Some(&source("backup"))).is_err());
        let no_snapshot = InstallSource {
            namespace: Some("yolab-gitea".into()),
            ..source("backup")
        };
        assert!(resolve_sources(Some(&no_snapshot)).is_err());
    }

    #[test]
    fn a_source_that_is_not_one_of_the_five_ways_in_is_refused() {
        let err = resolve_sources(Some(&source("clone-from-the-internet"))).unwrap_err();
        assert!(err.contains("clone-from-the-internet"), "{err}");
    }

    #[test]
    fn a_name_that_could_reach_outside_its_namespace_is_refused() {
        for bad in [
            "../kube-system",
            "gitea; rm -rf /",
            "Gitea",
            "gitea/../../etc",
        ] {
            let src = InstallSource {
                from_instance: Some(bad.into()),
                ..source("duplicate")
            };
            assert!(resolve_sources(Some(&src)).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_backup_namespace_outside_the_apps_is_refused() {
        for bad in ["kube-system", "yolab-", "yolab-../x", ""] {
            let src = InstallSource {
                namespace: Some(bad.into()),
                snapshot_id: Some("deadbeef".into()),
                ..source("backup")
            };
            assert!(resolve_sources(Some(&src)).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_snapshot_id_that_is_not_one_is_refused() {
        for bad in ["*", "../../latest", "dead beef"] {
            let src = InstallSource {
                namespace: Some("yolab-gitea".into()),
                snapshot_id: Some(bad.into()),
                ..source("backup")
            };
            assert!(resolve_sources(Some(&src)).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_fresh_install_keeps_what_the_form_sent() {
        let config = Map::from_iter([("subdomain".into(), json!("git"))]);
        let plan = plan(
            "gitea",
            "gitea-ab12",
            config.clone(),
            None,
            None,
            &no_schema(),
        )
        .unwrap();
        assert_eq!(plan.config, config);
        assert_eq!(plan.backup, BackupPolicy::default());
        assert_eq!(plan.data, None);
    }

    #[test]
    fn a_copy_keeps_the_originals_password_when_the_form_never_showed_it() {
        let app =
            crate::appschema::AppSchema::new(json!({ "properties": { "config": { "properties": {
                "admin_password": { "type": "string", "writeOnly": true, "generate": true }
            }}}}));
        let mut source = definition("gitea");
        source.config = Map::from_iter([("admin_password".into(), json!("original"))]);
        let form = Map::from_iter([
            ("admin_password".into(), json!("__redacted__")),
            ("subdomain".into(), json!("git-copy")),
        ]);
        let plan = plan(
            "gitea",
            "gitea-cd34",
            form,
            Some(&copied(&source)),
            None,
            &app,
        )
        .unwrap();
        assert_eq!(plan.config["admin_password"], json!("original"));
        assert_eq!(plan.config["subdomain"], json!("git-copy"));
    }

    #[test]
    fn a_copy_inherits_the_originals_backup_schedule() {
        let mut source = definition("gitea");
        source.backup = BackupPolicy {
            enabled: false,
            schedule: "0 5 * * 0".into(),
        };
        let plan = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&source)),
            None,
            &no_schema(),
        )
        .unwrap();
        assert_eq!(plan.backup, source.backup);
    }

    #[test]
    fn a_source_from_a_different_chart_is_refused_before_anything_is_created() {
        let source = definition("nextcloud");
        let err = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&source)),
            None,
            &no_schema(),
        )
        .unwrap_err();
        assert!(err.contains("nextcloud") && err.contains("gitea"), "{err}");
    }

    #[test]
    fn a_source_that_never_recorded_its_chart_is_taken_at_its_word() {
        let source = definition("");
        assert!(plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&source)),
            None,
            &no_schema()
        )
        .is_ok());
        assert_eq!(same_app("gitea", "", "that backup"), Ok(()));
    }

    #[test]
    fn an_instance_name_is_a_kubernetes_label() {
        assert!(is_instance_name("gitea-ab12"));
        assert!(!is_instance_name(""));
        assert!(!is_instance_name(&"a".repeat(MAX_LABEL_LEN + 1)));
        assert!(!is_instance_name("has_underscore"));
    }

    async fn events_of(
        stream: impl futures::Stream<Item = std::result::Result<Event, Infallible>>,
    ) -> usize {
        use futures::StreamExt as _;
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.count())
            .await
            .expect("the stream must end once the work is over")
    }

    #[tokio::test]
    async fn a_finished_install_closes_the_stream_instead_of_waiting_on_its_log() {
        let stream = sse(
            "install yolab-gitea-ab12".to_string(),
            |log| async move {
                log.say("one");
                log.say("two");
                Ok(())
            },
            "gitea installed".to_string(),
            "Nothing was left behind.",
        );
        assert_eq!(
            events_of(stream).await,
            3,
            "both log lines, then exactly one verdict"
        );
    }

    #[tokio::test]
    async fn a_failed_install_closes_the_stream_too() {
        let stream = sse(
            "install yolab-gitea-ab12".to_string(),
            |log| async move {
                log.say("starting");
                anyhow::bail!("the chart exploded")
            },
            "gitea installed".to_string(),
            "Nothing was left behind.",
        );
        assert_eq!(events_of(stream).await, 2);
    }

    #[tokio::test]
    async fn an_install_keeps_going_after_the_page_stops_listening() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (finished, finished_rx) = tokio::sync::oneshot::channel::<()>();
        let stream = sse(
            "install yolab-gitea-ab12".to_string(),
            move |log| async move {
                log.say("started");
                let _ = released.await;
                log.say("nobody is reading this any more");
                let _ = finished.send(());
                Ok(())
            },
            "gitea installed".to_string(),
            "unused",
        );
        drop(stream);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), finished_rx)
            .await
            .expect("the install must run to its end, not stop with the connection")
            .expect("the install was dropped instead of finishing");
    }

    fn blow_up() -> anyhow::Result<()> {
        panic!("the install blew up")
    }

    #[tokio::test]
    async fn an_install_that_panics_still_ends_the_stream_with_a_verdict() {
        let stream = sse(
            "install yolab-gitea-ab12".to_string(),
            |_log| async move { blow_up() },
            "gitea installed".to_string(),
            "unused",
        );
        assert_eq!(events_of(stream).await, 1, "only the verdict");
    }

    #[test]
    fn a_failure_is_reported_as_a_failure_and_says_what_is_left() {
        let line = verdict(
            Some(Err(anyhow::anyhow!("the chart exploded"))),
            "gitea installed",
            "Nothing was left behind.",
        );
        assert!(line.starts_with("[ERROR] "), "{line}");
        assert!(line.contains("the chart exploded"), "{line}");
        assert!(line.contains("Nothing was left behind."), "{line}");
    }

    #[test]
    fn a_success_is_reported_as_done_so_the_page_stops_waiting() {
        let line = verdict(Some(Ok(())), "gitea installed", "unused");
        assert_eq!(line, "[DONE] gitea installed");
    }

    #[test]
    fn work_that_vanished_without_a_word_is_still_a_failure() {
        let line = verdict(None, "gitea installed", "Nothing was left behind.");
        assert!(line.starts_with("[ERROR] "), "{line}");
        assert!(line.contains("Nothing was left behind."), "{line}");
    }

    #[test]
    fn a_backup_reference_that_could_reach_another_namespace_is_refused() {
        assert!(check_backup_ref("yolab-gitea-ab12", "deadbeef").is_ok());
        for (ns, snap) in [
            ("kube-system", "deadbeef"),
            ("yolab-gitea/../secrets", "deadbeef"),
            ("yolab-", "deadbeef"),
            ("yolab-gitea", "*"),
            ("yolab-gitea", "../latest"),
            ("yolab-gitea", ""),
        ] {
            assert!(
                check_backup_ref(ns, snap).is_err(),
                "{ns} @ {snap} was accepted"
            );
        }
    }

    fn install_failed_marks(patches: &[Value]) -> Vec<String> {
        patches
            .iter()
            .filter_map(|p| p["metadata"]["annotations"]["yolab.io/install-failed"].as_str())
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn an_install_that_panics_is_still_left_marked_as_failed() {
        use crate::k8s::testing::{accept_patches, api_server, patched};
        let (server, kube) = api_server().await;
        accept_patches(&server).await;

        run_detached(
            kube,
            "yolab-gitea-ab12".into(),
            "install yolab-gitea-ab12".into(),
            async move { blow_up() },
        )
        .await
        .unwrap();

        let marks = install_failed_marks(&patched(&server).await);
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert!(marks[0].contains("the install blew up"), "{marks:?}");
    }

    #[tokio::test]
    async fn a_detached_install_that_succeeds_leaves_no_mark() {
        use crate::k8s::testing::{accept_patches, api_server, patched};
        let (server, kube) = api_server().await;
        accept_patches(&server).await;

        run_detached(
            kube,
            "yolab-gitea-ab12".into(),
            "install yolab-gitea-ab12".into(),
            async move { Ok(()) },
        )
        .await
        .unwrap();

        assert!(install_failed_marks(&patched(&server).await).is_empty());
    }

    #[tokio::test]
    async fn an_install_runs_to_its_end_with_nobody_waiting_on_it() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (finished, finished_rx) = tokio::sync::oneshot::channel::<()>();
        drop(run_detached(
            crate::k8s::testing::unreachable(),
            "yolab-gitea-ab12".into(),
            "install yolab-gitea-ab12".into(),
            async move {
                let _ = released.await;
                let _ = finished.send(());
                Ok(())
            },
        ));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), finished_rx)
            .await
            .expect("the install must run to its end")
            .expect("the install was dropped instead of finishing");
    }
}
