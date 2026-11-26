use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::backup::common::Backend;
use crate::config::Config;
use crate::error::Outcome;
use crate::host::Host;
use crate::ops::{self, Claim, Claimed};
use crate::records::Store;
use crate::routers::apps::{
    app_schema, clear_install_failed, collect_runtime, mark_install_failed, merge_credentials,
    stage_install, write_definition, AppDefinition, BackupPolicy, ChartAt, StagedInstall,
    DEFINITION_SCHEMA,
};

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
    #[serde(default)]
    pub choices: crate::backup::restore_plan::Choices,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum DataOrigin {
    Backup {
        namespace: String,
        snapshot_id: String,
        choices: crate::backup::restore_plan::Choices,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ChartPin {
    Newest,
    Saved {
        repo: String,
        version: String,
        #[serde(with = "tgz_text")]
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

mod tgz_text {
    use base64::Engine as _;

    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
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
    snapshot_id: String,
    choices: &crate::backup::restore_plan::Choices,
) -> Option<DataOrigin> {
    with_data.then(|| DataOrigin::Backup {
        namespace: namespace.to_string(),
        snapshot_id,
        choices: choices.clone(),
    })
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
            let data = data_origin(
                source.with_data,
                &namespace,
                snapshot_id.clone(),
                &source.choices,
            );
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
    new_backup: BackupPolicy,
) -> Result<InstallPlan, String> {
    let mut plan = InstallPlan {
        app_id: app_id.to_string(),
        instance_name: instance_name.to_string(),
        release: source
            .map(|s| s.definition.release())
            .unwrap_or(instance_name)
            .to_string(),
        config,
        backup: new_backup,
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
                crate::backup::restore::definition_from_backup(b, namespace, snapshot_id).await?;
            let (chart, schema) =
                crate::backup::restore::kept_from_backup(b, namespace, snapshot_id).await?;
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
            let cache = crate::charts::cache_root();
            if let Some(dir) = crate::charts::cached_at_version(&cache, repo, app_id, version) {
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
            let pulled = tempfile::tempdir_in(crate::scratch::dir())?;
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
    Backup(Box<crate::backup::restore::BackupPayload>),
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

    let folders = crate::tree::render(
        &b.host,
        job.release,
        &staged.chart_dir.to_string_lossy(),
        &staged.ns,
        &staged.values.path().to_string_lossy(),
    )
    .await?;
    crate::tree::refuse_older_volumes(&b.kube, &staged.ns, &folders).await?;

    if let DataFill::Live { source_namespace } = fill {
        log.say("Copying this service's files…");
        crate::routers::copy::copy_live_folders(&b.kube, source_namespace, &folders).await?;
    }
    log.say("Making its folders…");
    crate::tree::run(
        &b.kube,
        &crate::tree::Op::Make(folders.iter().map(|p| p.claimed.folder.clone()).collect()),
    )
    .await?;
    if let DataFill::Backup(payload) = fill {
        log.say("Copying this service's files…");
        payload
            .fill_volumes(b, &staged.ns, job.release, &folders)
            .await?;
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
        config: staged.config.clone(),
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
            choices,
        }) => {
            log.say("Reading the backup…");
            let payload =
                crate::backup::restore::backup_payload(b, namespace, snapshot_id, choices.clone())
                    .await?;
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
    upgrade_one(b, cfg, plan, log).await
}

async fn upgrade_one<H: Host + 'static>(
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
            &crate::charts::cache_root(),
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
            *helm_said
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
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
    helm_outcome(
        release,
        finished,
        helm_said
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn helm_outcome(
    release: &str,
    finished: crate::host::HostResult<bool>,
    helm_said: Option<String>,
) -> anyhow::Result<()> {
    match finished {
        Ok(true) => Ok(()),
        Ok(false) => match helm_said {
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

fn log_outcome(subject: &str, outcome: &anyhow::Result<()>) {
    match outcome {
        Ok(()) => tracing::info!("{subject}: finished"),
        Err(e) => tracing::warn!("{subject}: failed: {e:#}"),
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
        log_outcome(&subject, &outcome);
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

pub(crate) const INSTALLS: Store = Store {
    name: "yolab-installs",
    namespace: "kube-system",
    key: "sets",
};
const MAX_INSTALLS: usize = 50;
const MAX_ATTEMPTS: u32 = 3;
const PLAN_SECRET: &str = "yolab-install-plan";
const PLAN_KEY: &str = "plan";
const LOOK_FOR_NEW_INSTALLS: Duration = Duration::from_secs(5);

#[derive(Clone, serde::Serialize, Deserialize)]
pub(crate) struct InstallRun {
    id: String,
    namespace: String,
    started_at: String,
    state: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    claim: Claim,
}

impl Claimed for InstallRun {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_running(&self) -> bool {
        self.state == ops::RUNNING
    }
    fn claim(&self) -> &Claim {
        &self.claim
    }
    fn claim_mut(&mut self) -> &mut Claim {
        &mut self.claim
    }
}

fn queued_run(namespace: &str) -> InstallRun {
    InstallRun {
        id: format!("in-{}", crate::backup::common::random_hex(8)),
        namespace: namespace.to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        state: ops::QUEUED.to_string(),
        attempts: 0,
        error: None,
        claim: Claim::default(),
    }
}

pub(crate) fn plan_secret(namespace: &str, plan: &InstallPlan) -> Value {
    let text = serde_json::to_string(plan).expect("an install plan is plain data");
    crate::k8s::secret_manifest(
        PLAN_SECRET,
        namespace,
        &[(PLAN_KEY, &text)],
        &[("app.kubernetes.io/managed-by", "yolab")],
    )
}

pub(crate) fn plan_from_secret(secret: &Value) -> anyhow::Result<InstallPlan> {
    use base64::Engine as _;
    let Some(encoded) = secret["data"][PLAN_KEY].as_str() else {
        anyhow::bail!("{PLAN_SECRET} holds no {PLAN_KEY}");
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) async fn queue(client: &kube::Client, plan: &InstallPlan) -> anyhow::Result<()> {
    let namespace = format!("yolab-{}", plan.instance_name);
    crate::k8s::apply(client, &plan_secret(&namespace, plan)).await?;
    let run = queued_run(&namespace);
    INSTALLS
        .update(client, |runs: &mut Vec<InstallRun>| {
            ops::upsert(runs, run.clone(), MAX_INSTALLS)
        })
        .await?;
    crate::runtime::wake("installs");
    Ok(())
}

fn takeable(r: &InstallRun) -> bool {
    r.state == ops::QUEUED || (r.is_running() && !ops::is_driven(r, ops::silent_for(r)))
}

fn begin_attempt(r: &mut InstallRun) {
    r.state = ops::RUNNING.to_string();
    r.attempts += 1;
}

async fn take_waiting(client: &kube::Client) -> anyhow::Result<Vec<InstallRun>> {
    let runs: Vec<InstallRun> = INSTALLS.read(client).await?;
    let mut taken = Vec::new();
    for run in runs.iter().rev().filter(|r| takeable(r)) {
        if let Some(t) = ops::take(INSTALLS, client, &run.id, takeable, begin_attempt).await? {
            taken.push(t);
        }
    }
    Ok(taken)
}

async fn record_run(client: &kube::Client, id: &str, result: &anyhow::Result<()>) {
    let state = if result.is_ok() {
        ops::SUCCEEDED
    } else {
        ops::FAILED
    };
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    let written = INSTALLS
        .update(client, |runs: &mut Vec<InstallRun>| {
            if let Some(r) = runs.iter_mut().find(|r| r.id == id) {
                r.state = state.to_string();
                r.error = error.clone();
            }
        })
        .await;
    written.warn_on_err(format!(
        "install {id}: could not record the outcome — it is run again once its heartbeat stops"
    ));
}

async fn forget_plan(client: &kube::Client, namespace: &str) {
    crate::k8s::delete_if_present(
        client,
        &crate::k8s::reference("v1", "Secret", namespace, PLAN_SECRET),
    )
    .await
    .warn_on_err(format!("{namespace}: remove its install plan"));
}

async fn give_up(client: &kube::Client, namespace: &str, why: String) -> anyhow::Error {
    mark_install_failed(client, namespace, &why).await;
    forget_plan(client, namespace).await;
    anyhow::anyhow!(why)
}

async fn run_install<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    http: &crate::http::Client,
    run: &InstallRun,
) -> anyhow::Result<()> {
    if run.attempts > MAX_ATTEMPTS {
        let why = format!("the install was interrupted {} times", run.attempts - 1);
        return Err(give_up(&b.kube, &run.namespace, why).await);
    }
    let found = crate::k8s::get(
        &b.kube,
        &crate::k8s::reference("v1", "Secret", &run.namespace, PLAN_SECRET),
    )
    .await?;
    let Some(secret) = found else {
        let why = "what to install was lost before it ran".to_string();
        return Err(give_up(&b.kube, &run.namespace, why).await);
    };
    let plan = match plan_from_secret(&secret) {
        Ok(plan) => plan,
        Err(e) => return Err(give_up(&b.kube, &run.namespace, format!("{e:#}")).await),
    };
    let log = Log::to_tracing(format!("install {}", run.namespace));
    let outcome = execute(b, cfg, &plan, &log).await;
    forget_plan(&b.kube, &run.namespace).await;
    outcome?;
    crate::routers::store::report_install(&b.kube, http, cfg, &plan.app_id).await;
    Ok(())
}

async fn drive_install<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    http: &crate::http::Client,
    run: InstallRun,
) {
    tracing::info!("{}: install attempt {}", run.namespace, run.attempts);
    let work = run_install(b, cfg, http, &run);
    let result = ops::drive::<InstallRun, _>(INSTALLS, &b.kube, &run.id, work).await;
    record_run(&b.kube, &run.id, &result).await;
}

pub(crate) async fn drive_installs<H: Host + 'static>(
    b: &Backend<H>,
    cfg: &Config,
    http: &crate::http::Client,
) -> anyhow::Result<crate::runtime::Tick> {
    use futures::StreamExt as _;
    let mut running = futures::stream::FuturesUnordered::new();
    let mut started = 0usize;
    loop {
        for run in take_waiting(&b.kube).await? {
            started += 1;
            running.push(drive_install(b, cfg, http, run));
        }
        if running.is_empty() {
            break;
        }
        tokio::select! {
            _ = running.next() => {}
            () = tokio::time::sleep(LOOK_FOR_NEW_INSTALLS) => {}
        }
    }
    Ok(if started == 0 {
        crate::runtime::Tick::Idle("no install is waiting".into())
    } else {
        crate::runtime::Tick::Done
    })
}

pub struct InstallsController;

impl crate::runtime::Resource for InstallsController {
    fn name(&self) -> &'static str {
        "installs"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    async fn converge(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let cfg = Config::from_env();
        drive_installs(&ctx.tools.backend().await?, &cfg, &ctx.tools.http).await
    }
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
        assert_eq!(
            helm_error("Release \"x\" does not exist. Installing it now."),
            None
        );
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
        let plan = plan(
            "gitea",
            "gitea-ab12",
            Map::new(),
            None,
            None,
            &no_schema(),
            BackupPolicy::default(),
        )
        .unwrap();
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
            BackupPolicy::default(),
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
            BackupPolicy::default(),
        )
        .unwrap();
        assert_eq!(plan.release, "gitea-ab12");
    }

    #[test]
    fn a_fresh_install_takes_the_newest_chart_from_the_catalog() {
        assert_eq!(pin_chart(None), Ok(ChartPin::Newest));
        let plan = plan(
            "gitea",
            "gitea-ab12",
            Map::new(),
            None,
            None,
            &no_schema(),
            BackupPolicy::default(),
        )
        .unwrap();
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
            BackupPolicy::default(),
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
            BackupPolicy::default(),
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
                choices: Default::default(),
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
            BackupPolicy::default(),
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
            BackupPolicy::default(),
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
            budget_bytes: Some(5 << 30),
        };
        let plan = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&source)),
            None,
            &no_schema(),
            BackupPolicy::default(),
        )
        .unwrap();
        assert_eq!(plan.backup, source.backup);
    }

    fn monthly_within_10_gb() -> BackupPolicy {
        BackupPolicy {
            enabled: true,
            schedule: "0 3 1 * *".into(),
            budget_bytes: Some(10 << 30),
        }
    }

    #[test]
    fn a_fresh_install_backs_up_the_way_the_cluster_settings_say() {
        let plan = plan(
            "gitea",
            "gitea-ab12",
            Map::new(),
            None,
            None,
            &no_schema(),
            monthly_within_10_gb(),
        )
        .unwrap();
        assert_eq!(plan.backup, monthly_within_10_gb());
    }

    #[test]
    fn a_copy_keeps_the_originals_backup_policy_over_the_cluster_settings() {
        let mut source = definition("gitea");
        source.backup = BackupPolicy {
            enabled: false,
            schedule: "0 5 * * 0".into(),
            budget_bytes: None,
        };
        let plan = plan(
            "gitea",
            "gitea-cd34",
            Map::new(),
            Some(&copied(&source)),
            None,
            &no_schema(),
            monthly_within_10_gb(),
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
            BackupPolicy::default(),
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
            &no_schema(),
            BackupPolicy::default()
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

    fn as_the_api_returns(manifest: &Value) -> Value {
        use base64::Engine as _;
        let mut data = serde_json::Map::new();
        for (k, v) in manifest["stringData"].as_object().unwrap() {
            let encoded = base64::engine::general_purpose::STANDARD.encode(v.as_str().unwrap());
            data.insert(k.clone(), Value::String(encoded));
        }
        json!({ "metadata": manifest["metadata"].clone(), "data": data })
    }

    fn a_plan(chart: ChartPin, data: Option<DataOrigin>) -> InstallPlan {
        let mut config = Map::new();
        config.insert("password".into(), json!("hunter2"));
        InstallPlan {
            app_id: "gitea".into(),
            instance_name: "gitea-ab12".into(),
            release: "gitea-ab12".into(),
            config,
            backup: definition("gitea").backup,
            data,
            chart,
        }
    }

    #[test]
    fn an_install_plan_comes_back_whole_from_its_secret() {
        let mut choices = crate::backup::restore_plan::Choices::default();
        choices
            .data
            .insert("data".into(), crate::backup::restore_plan::Choice::Use);
        let plans = [
            a_plan(ChartPin::Newest, None),
            a_plan(
                ChartPin::Saved {
                    repo: "official".into(),
                    version: "1.2.3".into(),
                    tgz: vec![0x1f, 0x8b, 0, 255],
                },
                Some(DataOrigin::Backup {
                    namespace: "yolab-gitea-old".into(),
                    snapshot_id: "snap-1".into(),
                    choices,
                }),
            ),
            a_plan(
                ChartPin::Exact {
                    repo: "official".into(),
                    version: "1.2.3".into(),
                },
                Some(DataOrigin::Live {
                    namespace: "yolab-gitea-old".into(),
                }),
            ),
        ];
        for plan in plans {
            let secret = plan_secret("yolab-gitea-ab12", &plan);
            assert_eq!(secret["metadata"]["namespace"], "yolab-gitea-ab12");
            let back = plan_from_secret(&as_the_api_returns(&secret)).unwrap();
            assert_eq!(back, plan);
        }
    }

    #[test]
    fn a_secret_without_a_plan_is_an_error() {
        assert!(plan_from_secret(&json!({ "data": {} })).is_err());
    }

    fn run(state: &str) -> InstallRun {
        let mut r = queued_run("yolab-gitea-ab12");
        r.state = state.into();
        r
    }

    #[test]
    fn a_queued_install_is_takeable_and_a_finished_one_never() {
        assert!(takeable(&run(ops::QUEUED)));
        assert!(!takeable(&run(ops::SUCCEEDED)));
        assert!(!takeable(&run(ops::FAILED)));
    }

    #[test]
    fn a_running_install_first_seen_now_is_not_taken_from_its_machine() {
        let mut r = run(ops::RUNNING);
        r.claim = Claim {
            owner: "node1".into(),
            heartbeat: chrono::Utc::now().to_rfc3339(),
        };
        assert!(!takeable(&r));
    }

    #[test]
    fn each_attempt_counts() {
        let mut r = run(ops::QUEUED);
        begin_attempt(&mut r);
        begin_attempt(&mut r);
        assert_eq!(r.state, ops::RUNNING);
        assert_eq!(r.attempts, 2);
    }

    #[test]
    fn a_queued_install_names_its_namespace_and_nobody_owns_it_yet() {
        let r = queued_run("yolab-gitea-ab12");
        assert!(r.id.starts_with("in-"));
        assert_eq!(r.namespace, "yolab-gitea-ab12");
        assert_eq!(r.state, ops::QUEUED);
        assert_eq!(r.claim, Claim::default());
    }

    fn test_backend(kube: kube::Client) -> Backend<crate::host::fake::FakeHost> {
        Backend {
            kube,
            host: crate::host::fake::FakeHost::new(),
        }
    }

    #[tokio::test]
    async fn an_install_interrupted_too_often_is_marked_failed_without_running() {
        use crate::k8s::testing::{accept_patches, api_server, patched};
        let (server, kube) = api_server().await;
        accept_patches(&server).await;
        let b = test_backend(kube);
        let mut r = run(ops::RUNNING);
        r.attempts = MAX_ATTEMPTS + 1;
        let cfg = Config::from_env();
        let http = crate::http::Client::new();

        let err = run_install(&b, &cfg, &http, &r).await.unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err}");
        let marks = install_failed_marks(&patched(&server).await);
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert!(b.host.calls().is_empty(), "{:?}", b.host.calls());
    }

    #[tokio::test]
    async fn an_install_whose_plan_is_gone_is_marked_failed() {
        use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};
        let (server, kube) = api_server().await;
        serve(
            &server,
            "/api/v1/namespaces/yolab-gitea-ab12/secrets/yolab-install-plan",
            404,
            status(404, "NotFound"),
        )
        .await;
        accept_patches(&server).await;
        let b = test_backend(kube);
        let cfg = Config::from_env();
        let http = crate::http::Client::new();

        let mut r = run(ops::RUNNING);
        r.attempts = 1;
        assert!(run_install(&b, &cfg, &http, &r).await.is_err());
        let marks = install_failed_marks(&patched(&server).await);
        assert!(marks[0].contains("lost"), "{marks:?}");
    }

    #[test]
    fn a_record_written_before_attempts_were_counted_still_reads() {
        let old: InstallRun = serde_json::from_value(json!({
            "id": "in-1", "namespace": "yolab-gitea-ab12", "started_at": "t",
            "state": "running", "owner": "node1", "heartbeat": "h"
        }))
        .unwrap();
        assert_eq!(old.attempts, 0);
    }

    mod end_to_end {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{accept_patches, api_server, patched, secret_with, status};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        struct World {
            server: MockServer,
            b: Backend<FakeHost>,
            cfg: Config,
            _cache: crate::charts::testing::CacheRoot,
            _dir: tempfile::TempDir,
        }

        async fn world(host: FakeHost) -> World {
            let cache = crate::charts::testing::cache_in_temp();
            crate::charts::testing::chart(cache.dir.path(), "official", "notes", "1.2.3");
            crate::charts::testing::chart(cache.dir.path(), "mine", "notes", "2.0.0");
            let (server, kube) = api_server().await;
            accept_patches(&server).await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(200).set_body_json(status(200, "Success")))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
                .with_priority(u8::MAX)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.toml");
            std::fs::write(&config, crate::testkit::config_with_tunnel("")).unwrap();
            World {
                server,
                b: Backend { kube, host },
                cfg: Config::for_test(&config),
                _cache: cache,
                _dir: dir,
            }
        }

        async fn serve(w: &World, at: &str, body: Value) {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&w.server)
                .await;
        }

        fn helm() -> FakeHost {
            FakeHost::new()
                .ok("helm template", "")
                .ok(
                    "helm upgrade --install",
                    "Release \"notes\" has been upgraded.",
                )
                .fail("helm package", "no chart here")
        }

        fn logged() -> (Log, tokio::sync::mpsc::UnboundedReceiver<String>) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            (Log(tx), rx)
        }

        fn lines(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        }

        fn notes(chart: ChartPin, data: Option<DataOrigin>) -> InstallPlan {
            InstallPlan {
                app_id: "notes".into(),
                instance_name: "notes".into(),
                release: "notes".into(),
                config: Map::new(),
                backup: Default::default(),
                data,
                chart,
            }
        }

        fn exact(repo: &str, version: &str) -> ChartPin {
            ChartPin::Exact {
                repo: repo.into(),
                version: version.into(),
            }
        }

        #[tokio::test]
        async fn an_install_pinned_to_a_cached_version_runs_helm_and_saves_the_settings() {
            let w = world(helm()).await;
            let (log, mut rx) = logged();
            execute(&w.b, &w.cfg, &notes(exact("official", "1.2.3"), None), &log)
                .await
                .unwrap();
            assert!(w.b.host.ran("helm template notes"));
            assert!(w
                .b
                .host
                .ran("helm upgrade --install --dependency-update notes"));
            let patched = patched(&w.server).await;
            let ns = patched
                .iter()
                .find(|p| p["kind"] == "Namespace")
                .expect("the namespace is applied");
            assert_eq!(
                ns["metadata"]["annotations"][crate::routers::apps::ANN_CHART_VERSION],
                "1.2.3"
            );
            assert!(patched
                .iter()
                .any(|p| p["metadata"]["name"] == "yolab-config"));
            assert!(install_failed_marks(&patched).is_empty());
            let said = lines(&mut rx);
            assert!(said.iter().any(|l| l == "Installing…"), "{said:?}");
            assert!(
                said.iter()
                    .any(|l| l.starts_with("[WARN] could not keep a copy")),
                "{said:?}"
            );
        }

        async fn failed_install(host: FakeHost, plan: InstallPlan) -> String {
            let w = world(host).await;
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &plan, &log).await.unwrap_err();
            let marks = install_failed_marks(&patched(&w.server).await);
            assert_eq!(marks.len(), 1, "{marks:?}");
            format!("{err:#}")
        }

        #[tokio::test]
        async fn a_failed_install_says_why_and_is_marked_failed() {
            let pinned = || notes(exact("official", "1.2.3"), None);
            let said = failed_install(
                FakeHost::new()
                    .ok("helm template", "")
                    .fail("helm upgrade", "\nError: the chart is broken"),
                pinned(),
            )
            .await;
            assert_eq!(said, "notes could not be installed: the chart is broken");
            let quiet = failed_install(
                FakeHost::new()
                    .ok("helm template", "")
                    .fail("helm upgrade", "exit 1"),
                pinned(),
            )
            .await;
            assert_eq!(
                quiet,
                "notes could not be installed — the log above is helm's own"
            );
            let missing = failed_install(
                FakeHost::new()
                    .ok("helm template", "")
                    .unstartable("helm upgrade"),
                pinned(),
            )
            .await;
            assert!(missing.starts_with("could not run helm:"), "{missing}");
            let unrendered = failed_install(
                FakeHost::new().fail("helm template", "parse error"),
                pinned(),
            )
            .await;
            assert!(
                unrendered.starts_with("could not read which folders notes uses"),
                "{unrendered}"
            );
            let gone = failed_install(helm(), notes(exact("gone", "1.0.0"), None)).await;
            assert!(
                gone.contains("which is no longer a chart repository here"),
                "{gone}"
            );
            let no_backups = failed_install(
                helm(),
                notes(
                    exact("official", "1.2.3"),
                    Some(DataOrigin::Backup {
                        namespace: "yolab-old".into(),
                        snapshot_id: "abc123".into(),
                        choices: Default::default(),
                    }),
                ),
            )
            .await;
            assert_eq!(no_backups, "backup not configured");
        }

        #[tokio::test]
        async fn a_copy_with_its_data_copies_the_living_services_folders_first() {
            let w = world(helm()).await;
            serve(
                &w,
                "/api/v1/persistentvolumes",
                crate::k8s::testing::list("PersistentVolume", vec![]),
            )
            .await;
            let (log, mut rx) = logged();
            let plan = notes(
                exact("official", "1.2.3"),
                Some(DataOrigin::Live {
                    namespace: "yolab-old".into(),
                }),
            );
            execute(&w.b, &w.cfg, &plan, &log).await.unwrap();
            assert!(lines(&mut rx)
                .iter()
                .any(|l| l == "Copying this service's files…"));
        }

        fn upgrade_of(chart_repo: Option<&str>, keep_version: bool) -> UpgradePlan {
            UpgradePlan {
                app_id: "notes".into(),
                instance_name: "notes".into(),
                release: "notes".into(),
                config: Map::new(),
                chart_repo: chart_repo.map(str::to_string),
                backup: Default::default(),
                keep_version,
            }
        }

        #[tokio::test]
        async fn an_update_takes_the_newest_chart_its_repository_holds_even_offline() {
            let w = world(helm()).await;
            serve(
                &w,
                "/api/v1/namespaces/kube-system/configmaps/yolab-chart-repos",
                json!({
                    "apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": { "name": "yolab-chart-repos", "namespace": "kube-system" },
                    "data": { "mine": format!("{}/catalog.yaml", w.server.uri()) }
                }),
            )
            .await;
            let (log, mut rx) = logged();
            upgrade(&w.b, &w.cfg, &upgrade_of(Some("mine"), false), &log)
                .await
                .unwrap();
            let said = lines(&mut rx);
            assert!(
                said.iter()
                    .any(|l| l.starts_with("Could not fetch the newest version")),
                "{said:?}"
            );
            assert!(said.iter().any(|l| l == "Now on version 2.0.0"), "{said:?}");
            let patched = patched(&w.server).await;
            assert!(patched.iter().any(|p| p["metadata"]["annotations"]
                .get("yolab.io/install-failed")
                .is_some_and(Value::is_null)));
        }

        #[tokio::test]
        async fn an_update_that_keeps_its_version_reinstalls_the_one_it_runs() {
            let w = world(helm()).await;
            let mut running = definition("notes");
            running.chart_version = "1.2.3".into();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-config",
                secret_with(
                    crate::routers::apps::DEFINITION_SECRET_KEY,
                    &serde_json::to_string(&running).unwrap(),
                ),
            )
            .await;
            let (log, mut rx) = logged();
            upgrade(&w.b, &w.cfg, &upgrade_of(None, true), &log)
                .await
                .unwrap();
            let said = lines(&mut rx);
            assert!(
                said.iter().any(|l| l == "Keeping the version it runs now…"),
                "{said:?}"
            );
            assert!(said.iter().any(|l| l == "Now on version 1.2.3"), "{said:?}");
        }

        #[tokio::test]
        async fn an_update_that_keeps_a_version_it_never_recorded_is_refused() {
            let w = world(helm()).await;
            let mut running = definition("notes");
            running.chart_version = String::new();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-config",
                secret_with(
                    crate::routers::apps::DEFINITION_SECRET_KEY,
                    &serde_json::to_string(&running).unwrap(),
                ),
            )
            .await;
            let (log, _rx) = logged();
            let err = upgrade(&w.b, &w.cfg, &upgrade_of(None, true), &log)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("never recorded which version"),
                "{err}"
            );
            assert!(!w.b.host.ran("helm upgrade"));
        }

        #[tokio::test]
        async fn a_queued_install_runs_once_and_forgets_its_plan() {
            let w = world(helm()).await;
            let kept = crate::records::testing::kept(&w.server, "yolab-installs").await;
            let plan = notes(exact("official", "1.2.3"), None);
            queue(&w.b.kube, &plan).await.unwrap();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-install-plan",
                as_the_api_returns(&plan_secret("yolab-notes", &plan)),
            )
            .await;
            let http = crate::http::Client::new();
            let tick = drive_installs(&w.b, &w.cfg, &http).await.unwrap();
            assert!(matches!(tick, crate::runtime::Tick::Done), "{tick:?}");
            let runs = kept.data("sets");
            assert_eq!(runs[0]["state"], ops::SUCCEEDED, "{runs}");
            assert!(w.b.host.ran("helm upgrade --install"));
            assert!(crate::k8s::testing::asked(&w.server, "yolab-install-plan").await);
            let tick = drive_installs(&w.b, &w.cfg, &http).await.unwrap();
            assert!(matches!(tick, crate::runtime::Tick::Idle(_)), "{tick:?}");
        }

        #[tokio::test]
        async fn a_queued_install_whose_plan_is_unreadable_fails_and_is_recorded() {
            let w = world(helm()).await;
            let kept = crate::records::testing::kept(&w.server, "yolab-installs").await;
            queue(&w.b.kube, &notes(exact("official", "1.2.3"), None))
                .await
                .unwrap();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-install-plan",
                json!({ "metadata": { "name": "yolab-install-plan" }, "data": { "plan": "bm90IGpzb24=" } }),
            )
            .await;
            let http = crate::http::Client::new();
            drive_installs(&w.b, &w.cfg, &http).await.unwrap();
            let runs = kept.data("sets");
            assert_eq!(runs[0]["state"], ops::FAILED, "{runs}");
            assert!(!w.b.host.ran("helm upgrade"));
        }

        #[tokio::test]
        async fn the_installs_controller_runs_for_the_cluster_once_its_api_answers() {
            use crate::runtime::Resource as _;
            let c = InstallsController;
            assert_eq!(c.name(), "installs");
            assert_eq!(c.scope(), crate::runtime::Scope::Cluster);
            assert_eq!(c.interval(), Duration::from_secs(30));
            assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
            let ctx = crate::runtime::Ctx {
                node: "node1".into(),
                tools: crate::tools::Tools::unreachable(),
            };
            assert!(c.converge(&ctx).await.is_err());
            let (_server, kube) = api_server().await;
            let err = queue(&kube, &notes(ChartPin::Newest, None)).await;
            assert!(err.is_err());
        }

        #[test]
        fn a_saved_chart_that_is_not_text_is_not_a_plan() {
            assert!(serde_json::from_value::<ChartPin>(
                json!({ "kind": "saved", "repo": "official", "version": "1", "tgz": 5 })
            )
            .is_err());
            assert!(plan_from_secret(&json!({ "data": { "plan": "!!!" } })).is_err());
        }

        #[test]
        fn a_helm_that_ran_out_of_time_says_so() {
            let said = helm_outcome(
                "notes",
                Err(crate::exec::CmdError::Timeout {
                    cmd: "helm upgrade".into(),
                    after: Duration::from_secs(900),
                }),
                None,
            )
            .unwrap_err();
            assert_eq!(
                said.to_string(),
                "installing notes took too long and was stopped"
            );
        }

        fn packaging(host: FakeHost) -> FakeHost {
            host.ok("helm package", "")
                .effect("helm package", |_, args| {
                    std::fs::write(
                        std::path::Path::new(args[3]).join("notes-1.2.3.tgz"),
                        [1u8, 2, 3],
                    )
                    .unwrap();
                })
        }

        fn kept_helm() -> FakeHost {
            packaging(
                FakeHost::new()
                    .ok("helm template", "")
                    .ok("helm upgrade --install", "upgraded"),
            )
        }

        fn pulling(host: FakeHost) -> FakeHost {
            host.ok("helm pull", "").effect("helm pull", |_, args| {
                let dir = std::path::Path::new(args[6]).join("notes");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("Chart.yaml"),
                    format!("apiVersion: v2\nname: notes\nversion: {}\n", args[3]),
                )
                .unwrap();
            })
        }

        async fn catalog_at(w: &World, body: &str) -> crate::charts::testing::OfficialAt {
            Mock::given(method("GET"))
                .and(path("/catalog.yaml"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&w.server)
                .await;
            crate::charts::testing::official_at(&format!("{}/catalog.yaml", w.server.uri()))
        }

        fn without_tunnel(w: &World) -> Config {
            let path = w._dir.path().join("bare.toml");
            std::fs::write(&path, "[homelab]\nhostname = \"yolab\"\n").unwrap();
            Config::for_test(&path)
        }

        #[tokio::test]
        async fn an_install_of_the_newest_chart_keeps_a_copy_of_it() {
            let w = world(kept_helm()).await;
            let (log, mut rx) = logged();
            execute(&w.b, &w.cfg, &notes(ChartPin::Newest, None), &log)
                .await
                .unwrap();
            let said = lines(&mut rx);
            assert!(!said.iter().any(|l| l.starts_with("[WARN]")), "{said:?}");
            assert!(patched(&w.server)
                .await
                .iter()
                .any(|p| p["metadata"]["name"] == crate::saved_chart::CONFIG_MAP));
        }

        #[tokio::test]
        async fn a_copy_installs_from_the_chart_kept_with_the_original() {
            let host = helm().ok("tar", "").effect("tar", |_, args| {
                let dir = std::path::Path::new(args[3]).join("notes");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("Chart.yaml"),
                    "apiVersion: v2\nname: notes\nversion: 0.9.0\n",
                )
                .unwrap();
            });
            let w = world(host).await;
            let (log, mut rx) = logged();
            let saved = ChartPin::Saved {
                repo: "official".into(),
                version: "0.9.0".into(),
                tgz: vec![1, 2, 3],
            };
            execute(&w.b, &w.cfg, &notes(saved, None), &log)
                .await
                .unwrap();
            assert!(lines(&mut rx)
                .iter()
                .any(|l| l == "Using the exact chart the original runs…"));
        }

        #[tokio::test]
        async fn a_copy_of_a_version_no_longer_cached_is_fetched_again() {
            let w = world(pulling(helm())).await;
            let _official = catalog_at(&w, "registry: oci://ghcr.io/x/charts\ncharts: []\n").await;
            let (log, mut rx) = logged();
            execute(&w.b, &w.cfg, &notes(exact("official", "0.5.0"), None), &log)
                .await
                .unwrap();
            assert!(lines(&mut rx)
                .iter()
                .any(|l| l.starts_with("Fetching version 0.5.0 of notes")));
            assert!(w
                .b
                .host
                .ran("helm pull oci://ghcr.io/x/charts/notes --version 0.5.0"));
        }

        #[tokio::test]
        async fn a_version_that_cannot_be_fetched_into_a_temp_folder_fails_the_install() {
            let w = world(pulling(helm())).await;
            let _official = catalog_at(&w, "registry: oci://ghcr.io/x/charts\ncharts: []\n").await;
            let (log, _rx) = logged();
            crate::scratch::set(Some(w._dir.path().join("no-such-folder")));
            let failed =
                execute(&w.b, &w.cfg, &notes(exact("official", "0.5.0"), None), &log).await;
            crate::scratch::set(None);
            assert!(failed.is_err());
            assert!(!w.b.host.ran("helm pull"));
        }

        #[tokio::test]
        async fn an_install_without_the_tunnel_settings_stops_before_helm() {
            let mut w = world(helm()).await;
            w.cfg = without_tunnel(&w);
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &notes(exact("official", "1.2.3"), None), &log)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "could not read tunnel config");
            assert!(!w.b.host.ran("helm upgrade"));
        }

        fn rendering_a_folder() -> FakeHost {
            let rendered = json!({ "apiVersion": "v1", "kind": "List", "items": [
                {
                    "apiVersion": "v1", "kind": "PersistentVolume",
                    "metadata": { "name": "yolab-notes.data" },
                    "spec": {
                        "claimRef": { "namespace": "yolab-notes", "name": "data" },
                        "csi": {
                            "driver": crate::csi::DRIVER,
                            "nodeStageSecretRef": { "name": crate::csi::STATIC_SECRET, "namespace": crate::csi::NS },
                            "volumeAttributes": { "staticVolume": "true", "rootPath": "/tree/notes" }
                        }
                    }
                },
                {
                    "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                    "metadata": { "name": "data", "namespace": "yolab-notes" }
                }
            ] });
            FakeHost::new()
                .ok("helm template", &rendered.to_string())
                .ok("helm upgrade --install", "upgraded")
        }

        #[tokio::test]
        async fn a_claim_kept_the_old_way_stops_the_install() {
            let w = world(rendering_a_folder()).await;
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/persistentvolumeclaims/data",
                json!({ "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                        "metadata": { "name": "data" }, "spec": { "volumeName": "pvc-old" } }),
            )
            .await;
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &notes(exact("official", "1.2.3"), None), &log)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("the old way"), "{err}");
        }

        #[tokio::test]
        async fn folders_that_cannot_be_made_stop_the_install() {
            let w = world(rendering_a_folder()).await;
            let (log, mut rx) = logged();
            assert!(
                execute(&w.b, &w.cfg, &notes(exact("official", "1.2.3"), None), &log)
                    .await
                    .is_err()
            );
            assert!(lines(&mut rx).iter().any(|l| l == "Making its folders…"));
            assert!(!w.b.host.ran("helm upgrade"));
        }

        #[tokio::test]
        async fn a_copy_whose_living_files_cannot_be_found_stops() {
            let w = world(helm()).await;
            let (log, _rx) = logged();
            let plan = notes(
                exact("official", "1.2.3"),
                Some(DataOrigin::Live {
                    namespace: "yolab-old".into(),
                }),
            );
            assert!(execute(&w.b, &w.cfg, &plan, &log).await.is_err());
            assert!(!w.b.host.ran("helm upgrade"));
        }

        async fn refuse_patch(w: &World, at: &str) {
            Mock::given(method("PATCH"))
                .and(path(at))
                .respond_with(
                    ResponseTemplate::new(500).set_body_json(status(500, "InternalError")),
                )
                .with_priority(1)
                .mount(&w.server)
                .await;
        }

        #[tokio::test]
        async fn settings_that_cannot_be_saved_fail_the_install() {
            let w = world(helm()).await;
            refuse_patch(&w, "/api/v1/namespaces/yolab-notes/secrets/yolab-config").await;
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &notes(exact("official", "1.2.3"), None), &log)
                .await
                .unwrap_err();
            assert!(
                err.to_string().starts_with("save this app's settings: "),
                "{err}"
            );
        }

        fn backup_holding(app_id: &str) -> FakeHost {
            let catalog = json!({
                "namespaces": ["yolab-old"],
                "services": [{ "namespace": "yolab-old", "app_id": app_id, "pvcs": [] }]
            });
            let objects = json!({ "apiVersion": "v1", "kind": "List", "items": [
                { "apiVersion": "v1", "kind": "ConfigMap",
                  "metadata": { "name": "settings", "namespace": "yolab-old" }, "data": { "a": "b" } }
            ] });
            helm()
                .ok("restic unlock", "")
                .restic_restores(vec![
                    ("catalog.json", catalog.to_string()),
                    ("yolab-old.yaml", objects.to_string()),
                ])
                .ok(
                    "restic snapshots --no-lock abc123 --json",
                    r#"[{"id":"abc123","time":"2026-11-20T00:00:00Z"}]"#,
                )
        }

        async fn backups_enabled(w: &World) {
            serve(
                w,
                "/api/v1/namespaces/kube-system/secrets/yolab-backup-config",
                secret_with("restic_password", "pw"),
            )
            .await;
        }

        fn from_backup() -> InstallPlan {
            notes(
                exact("official", "1.2.3"),
                Some(DataOrigin::Backup {
                    namespace: "yolab-old".into(),
                    snapshot_id: "abc123".into(),
                    choices: Default::default(),
                }),
            )
        }

        #[tokio::test]
        async fn a_restore_fills_the_service_and_puts_its_settings_back() {
            let w = world(backup_holding("notes")).await;
            backups_enabled(&w).await;
            let _restores = crate::records::testing::kept(&w.server, "yolab-restores").await;
            let (log, mut rx) = logged();
            execute(&w.b, &w.cfg, &from_backup(), &log).await.unwrap();
            assert!(lines(&mut rx)
                .iter()
                .any(|l| l == "Putting this app's saved settings back…"));
            assert!(patched(&w.server)
                .await
                .iter()
                .any(|p| p["metadata"]["name"] == "settings"));
        }

        #[tokio::test]
        async fn a_restore_that_cannot_record_itself_stops_before_helm() {
            let w = world(backup_holding("notes")).await;
            backups_enabled(&w).await;
            let (log, _rx) = logged();
            assert!(execute(&w.b, &w.cfg, &from_backup(), &log).await.is_err());
            assert!(!w.b.host.ran("helm upgrade"));
        }

        #[tokio::test]
        async fn a_restore_whose_settings_cannot_be_put_back_fails() {
            let w = world(backup_holding("notes")).await;
            backups_enabled(&w).await;
            let _restores = crate::records::testing::kept(&w.server, "yolab-restores").await;
            refuse_patch(&w, "/api/v1/namespaces/yolab-old/configmaps/settings").await;
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &from_backup(), &log)
                .await
                .unwrap_err();
            assert!(err.to_string().starts_with("apply yolab-old.yaml"), "{err}");
        }

        #[tokio::test]
        async fn a_backup_of_a_different_app_is_refused() {
            let w = world(backup_holding("other")).await;
            backups_enabled(&w).await;
            let (log, _rx) = logged();
            let err = execute(&w.b, &w.cfg, &from_backup(), &log)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "that backup is a other, not a notes");
        }

        #[tokio::test]
        async fn a_backup_gives_the_definition_and_the_form_it_was_taken_with() {
            let w = world(FakeHost::new()).await;
            backups_enabled(&w).await;
            let saved = json!({ "properties": { "config": { "properties": {} } } });
            let catalog = json!({
                "namespaces": ["yolab-old"],
                "services": [{ "namespace": "yolab-old", "app_id": "notes",
                               "definition": serde_json::to_value(definition("notes")).unwrap() }]
            });
            let objects =
                json!({ "items": [crate::saved_chart::schema_manifest("yolab-old", &saved)] });
            let b = Backend {
                kube: w.b.kube.clone(),
                host: FakeHost::new()
                    .ok("restic unlock", "")
                    .restic_restores(vec![
                        ("catalog.json", catalog.to_string()),
                        ("yolab-old.yaml", objects.to_string()),
                    ]),
            };
            let found = crate::routers::install::source(
                &b,
                &ConfigOrigin::Backup {
                    namespace: "yolab-old".into(),
                    snapshot_id: "abc123".into(),
                },
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(found.definition, definition("notes"));
            assert_eq!(found.chart, None);
            assert_eq!(found.schema, Some(saved));
        }

        #[tokio::test]
        async fn a_backup_whose_definition_or_kept_chart_cannot_be_read_is_no_source() {
            let w = world(FakeHost::new()).await;
            backups_enabled(&w).await;
            let origin = ConfigOrigin::Backup {
                namespace: "yolab-old".into(),
                snapshot_id: "abc123".into(),
            };
            let nothing_restored = Backend {
                kube: w.b.kube.clone(),
                host: FakeHost::new()
                    .ok("restic unlock", "")
                    .fail("restic restore", "snapshot not found"),
            };
            assert!(crate::routers::install::source(&nothing_restored, &origin)
                .await
                .is_err());
            let catalog = json!({
                "namespaces": ["yolab-old"],
                "services": [{ "namespace": "yolab-old", "app_id": "notes",
                               "definition": serde_json::to_value(definition("notes")).unwrap() }]
            });
            let only_the_catalog = Backend {
                kube: w.b.kube.clone(),
                host: FakeHost::new()
                    .ok("restic unlock", "")
                    .restic_restores(vec![("catalog.json", catalog.to_string())])
                    .fail("restic restore", "snapshot not found"),
            };
            assert!(crate::routers::install::source(&only_the_catalog, &origin)
                .await
                .is_err());
        }

        #[tokio::test]
        async fn a_kept_chart_that_cannot_be_unpacked_fails_the_copy() {
            let w = world(helm().fail("tar", "not a gzip archive")).await;
            let (log, _rx) = logged();
            let saved = ChartPin::Saved {
                repo: "official".into(),
                version: "0.9.0".into(),
                tgz: vec![1, 2, 3],
            };
            assert!(execute(&w.b, &w.cfg, &notes(saved, None), &log)
                .await
                .is_err());
            assert!(!w.b.host.ran("helm upgrade"));
        }

        async fn refuse_get(w: &World, at: &str) {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .with_priority(1)
                .mount(&w.server)
                .await;
        }

        async fn a_living_original(w: &World) {
            serve(
                w,
                "/api/v1/namespaces/yolab-old/secrets/yolab-config",
                secret_with(
                    crate::routers::apps::DEFINITION_SECRET_KEY,
                    &serde_json::to_string(&definition("notes")).unwrap(),
                ),
            )
            .await;
        }

        #[tokio::test]
        async fn an_original_whose_kept_chart_or_form_cannot_be_read_cannot_be_copied() {
            let origin = ConfigOrigin::LiveApp {
                namespace: "yolab-old".into(),
            };
            let w = world(FakeHost::new()).await;
            a_living_original(&w).await;
            refuse_get(&w, "/api/v1/namespaces/yolab-old/configmaps/yolab-chart").await;
            assert!(crate::routers::install::source(&w.b, &origin)
                .await
                .is_err());

            let w = world(FakeHost::new()).await;
            a_living_original(&w).await;
            refuse_get(&w, "/api/v1/namespaces/yolab-old/configmaps/yolab-schema").await;
            assert!(crate::routers::install::source(&w.b, &origin)
                .await
                .is_err());
        }

        #[tokio::test]
        async fn an_update_that_keeps_its_version_needs_to_read_what_runs_now() {
            let w = world(helm()).await;
            refuse_get(&w, "/api/v1/namespaces/yolab-notes/secrets/yolab-config").await;
            let (log, _rx) = logged();
            assert!(upgrade(&w.b, &w.cfg, &upgrade_of(None, true), &log)
                .await
                .is_err());
        }

        #[tokio::test]
        async fn an_update_that_keeps_a_version_no_repository_serves_is_refused() {
            let w = world(helm()).await;
            let mut running = definition("notes");
            running.chart_repo = "gone".into();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-config",
                secret_with(
                    crate::routers::apps::DEFINITION_SECRET_KEY,
                    &serde_json::to_string(&running).unwrap(),
                ),
            )
            .await;
            let (log, _rx) = logged();
            let err = upgrade(&w.b, &w.cfg, &upgrade_of(None, true), &log)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("could not be fetched again"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn an_update_fetches_the_newest_chart_when_it_can() {
            let w = world(pulling(helm())).await;
            let _official = catalog_at(
                &w,
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1.3.0\"\n",
            )
            .await;
            let (log, mut rx) = logged();
            upgrade(&w.b, &w.cfg, &upgrade_of(None, false), &log)
                .await
                .unwrap();
            let said = lines(&mut rx);
            assert!(
                !said.iter().any(|l| l.starts_with("Could not fetch")),
                "{said:?}"
            );
            assert!(said.iter().any(|l| l == "Now on version 1.3.0"), "{said:?}");
        }

        #[tokio::test]
        async fn an_update_without_the_tunnel_settings_fails() {
            let mut w = world(helm()).await;
            w.cfg = without_tunnel(&w);
            let (log, _rx) = logged();
            assert!(
                upgrade(&w.b, &w.cfg, &upgrade_of(Some("mine"), false), &log)
                    .await
                    .is_err()
            );
        }

        #[tokio::test]
        async fn a_queued_install_that_fails_is_recorded_as_failed() {
            let w = world(helm()).await;
            let kept = crate::records::testing::kept(&w.server, "yolab-installs").await;
            let plan = notes(exact("gone", "1.0.0"), None);
            queue(&w.b.kube, &plan).await.unwrap();
            serve(
                &w,
                "/api/v1/namespaces/yolab-notes/secrets/yolab-install-plan",
                as_the_api_returns(&plan_secret("yolab-notes", &plan)),
            )
            .await;
            let http = crate::http::Client::new();
            drive_installs(&w.b, &w.cfg, &http).await.unwrap();
            assert_eq!(kept.data("sets")[0]["state"], ops::FAILED);
        }

        #[tokio::test]
        async fn an_install_whose_plan_cannot_be_read_right_now_is_an_error() {
            let w = world(helm()).await;
            refuse_get(
                &w,
                "/api/v1/namespaces/yolab-gitea-ab12/secrets/yolab-install-plan",
            )
            .await;
            let mut r = run(ops::RUNNING);
            r.attempts = 1;
            let http = crate::http::Client::new();
            assert!(run_install(&w.b, &w.cfg, &http, &r).await.is_err());
        }

        #[tokio::test]
        async fn an_outcome_for_an_install_no_longer_listed_changes_nothing() {
            let w = world(helm()).await;
            let kept = crate::records::testing::kept(&w.server, "yolab-installs").await;
            queue(&w.b.kube, &notes(ChartPin::Newest, None))
                .await
                .unwrap();
            record_run(&w.b.kube, "in-missing", &Ok(())).await;
            assert_eq!(kept.data("sets")[0]["state"], ops::QUEUED);
        }

        async fn installs_read(server: &MockServer, first: Value, then: ResponseTemplate) {
            use crate::records::testing::{path_of, stored};
            Mock::given(method("GET"))
                .and(path(path_of("yolab-installs")))
                .respond_with(ResponseTemplate::new(200).set_body_json(stored(
                    "yolab-installs",
                    &first,
                    "1",
                )))
                .up_to_n_times(1)
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path(path_of("yolab-installs")))
                .respond_with(then)
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn an_install_taken_by_another_machine_meanwhile_is_left_to_it() {
            use crate::records::testing::stored;
            let (server, kube) = api_server().await;
            crate::records::testing::accept_writes(&server).await;
            let queued = run(ops::QUEUED);
            let mut done = queued.clone();
            done.state = ops::SUCCEEDED.into();
            installs_read(
                &server,
                json!([queued]),
                ResponseTemplate::new(200).set_body_json(stored(
                    "yolab-installs",
                    &json!([done]),
                    "2",
                )),
            )
            .await;
            assert!(take_waiting(&kube).await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_install_that_cannot_be_claimed_is_an_error() {
            let (server, kube) = api_server().await;
            installs_read(
                &server,
                json!([run(ops::QUEUED)]),
                ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
            )
            .await;
            assert!(take_waiting(&kube).await.is_err());
        }

        #[tokio::test]
        async fn the_installs_controller_needs_the_cluster() {
            use crate::runtime::Resource as _;
            let ctx = crate::runtime::Ctx {
                node: "node1".into(),
                tools: crate::tools::Tools {
                    host: Default::default(),
                    kube: crate::k8s::Kube::from_environment(),
                    http: crate::http::client(),
                },
            };
            assert!(InstallsController.converge(&ctx).await.is_err());
        }
    }
}
