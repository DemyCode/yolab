use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::Config;
use crate::proc::KillOnDrop;
use crate::routers::apps::{
    chart_uischema, collect_runtime, merge_credentials, rollback_failed_install, stage_install,
    write_definition, AppDefinition, BackupPolicy, StagedInstall, DEFINITION_SCHEMA,
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
pub(crate) struct InstallPlan {
    pub(crate) app_id: String,
    pub(crate) instance_name: String,
    pub(crate) config: Map<String, Value>,
    pub(crate) backup: BackupPolicy,
    pub(crate) data: Option<DataOrigin>,
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
    source: Option<&AppDefinition>,
    data: Option<DataOrigin>,
    uischema: &Value,
) -> Result<InstallPlan, String> {
    let mut plan = InstallPlan {
        app_id: app_id.to_string(),
        instance_name: instance_name.to_string(),
        config,
        backup: BackupPolicy::default(),
        data,
    };
    let Some(source) = source else {
        return Ok(plan);
    };
    same_app(app_id, &source.app_id, "the app you are copying")?;
    plan.config = merge_credentials(plan.config, &source.config, uischema);
    plan.backup = source.backup.clone();
    Ok(plan)
}

pub(crate) async fn source_definition(
    origin: &ConfigOrigin,
) -> anyhow::Result<Option<AppDefinition>> {
    match origin {
        ConfigOrigin::Fresh => Ok(None),
        ConfigOrigin::LiveApp { namespace } => crate::routers::apps::read_definition(namespace)
            .await
            .map(Some),
        ConfigOrigin::Backup {
            namespace,
            snapshot_id,
        } => crate::routers::restore::definition_from_backup(namespace, snapshot_id)
            .await
            .map(Some),
    }
}

pub(crate) struct Log(tokio::sync::mpsc::UnboundedSender<String>);

impl Log {
    pub(crate) fn say(&self, line: impl Into<String>) {
        let _ = self.0.send(line.into());
    }
}

struct Rollback {
    namespace: String,
    instance_name: String,
    armed: bool,
}

impl Drop for Rollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (namespace, instance_name) = (self.namespace.clone(), self.instance_name.clone());
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::error!("{namespace}: no runtime left to undo the failed install");
            return;
        };
        handle.spawn(async move { rollback_failed_install(&namespace, &instance_name).await });
    }
}

struct ChartJob<'a> {
    app_id: &'a str,
    instance_name: &'a str,
    config: &'a Map<String, Value>,
    chart_repo: Option<&'a str>,
    backup: &'a BackupPolicy,
    verb: &'static str,
}

enum DataFill<'a> {
    None,
    Backup(Box<crate::routers::restore::BackupPayload>),
    Live { source_namespace: &'a str },
}

async fn apply_chart(
    cfg: &Config,
    job: &ChartJob<'_>,
    fill: &DataFill<'_>,
    log: &Log,
) -> anyhow::Result<()> {
    let staged = stage_install(
        cfg,
        job.app_id,
        job.instance_name,
        job.config,
        job.chart_repo,
    )
    .await?;

    match fill {
        DataFill::Backup(payload) => {
            log.say("Copying this app's files…");
            payload.fill_volumes(&staged.ns, job.instance_name).await?;
        }
        DataFill::Live { source_namespace } => {
            log.say("Copying this app's files…");
            crate::routers::copy::copy_live_volumes(
                source_namespace,
                &staged.ns,
                job.instance_name,
            )
            .await?;
        }
        DataFill::None => {}
    }

    log.say(job.verb);
    helm_install(&staged, job.instance_name, log).await?;

    if let DataFill::Backup(payload) = fill {
        log.say("Putting this app's saved settings back…");
        payload.reapply().await?;
    }

    let (volumes, resources) = collect_runtime(&staged.ns).await;
    let definition = AppDefinition {
        schema: DEFINITION_SCHEMA,
        app_id: job.app_id.to_string(),
        chart_repo: job.chart_repo.unwrap_or(&staged.chart_repo).to_string(),
        chart_version: staged.chart_version.clone(),
        instance_name: job.instance_name.to_string(),
        service_name: staged.service_name.clone(),
        config: job.config.clone(),
        volumes,
        resources,
        backup: job.backup.clone(),
    };
    write_definition(
        &staged.ns,
        &definition,
        &chart_uischema(&cfg.catalog_dir(), job.app_id),
    )
    .await
    .map_err(|e| anyhow::anyhow!("save this app's settings: {e}"))
}

pub(crate) async fn execute(cfg: &Config, plan: &InstallPlan, log: &Log) -> anyhow::Result<()> {
    let mut rollback = Rollback {
        namespace: format!("yolab-{}", plan.instance_name),
        instance_name: plan.instance_name.clone(),
        armed: true,
    };
    let outcome = install_inner(cfg, plan, log).await;
    rollback.armed = outcome.is_err();
    outcome
}

async fn install_inner(cfg: &Config, plan: &InstallPlan, log: &Log) -> anyhow::Result<()> {
    log.say("Getting things ready…");
    let fill = match &plan.data {
        Some(DataOrigin::Backup {
            namespace,
            snapshot_id,
        }) => {
            log.say("Reading the backup…");
            let payload = crate::routers::restore::backup_payload(namespace, snapshot_id).await?;
            same_app(&plan.app_id, payload.app_id(), "that backup")
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            DataFill::Backup(Box::new(payload))
        }
        Some(DataOrigin::Live { namespace }) => DataFill::Live {
            source_namespace: namespace.as_str(),
        },
        None => DataFill::None,
    };

    let job = ChartJob {
        app_id: &plan.app_id,
        instance_name: &plan.instance_name,
        config: &plan.config,
        chart_repo: None,
        backup: &plan.backup,
        verb: "Installing…",
    };
    apply_chart(cfg, &job, &fill, log).await?;

    let namespace = format!("yolab-{}", plan.instance_name);
    if let Err(e) = crate::routers::backups::setup_namespace_backup(&namespace).await {
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
    pub(crate) config: Map<String, Value>,
    pub(crate) chart_repo: Option<String>,
    pub(crate) backup: BackupPolicy,
}

pub(crate) async fn upgrade(cfg: &Config, plan: &UpgradePlan, log: &Log) -> anyhow::Result<()> {
    log.say("Getting things ready…");
    let job = ChartJob {
        app_id: &plan.app_id,
        instance_name: &plan.instance_name,
        config: &plan.config,
        chart_repo: plan.chart_repo.as_deref(),
        backup: &plan.backup,
        verb: "Updating…",
    };
    apply_chart(cfg, &job, &DataFill::None, log).await
}

async fn helm_install(
    staged: &StagedInstall,
    instance_name: &str,
    log: &Log,
) -> anyhow::Result<()> {
    let mut child = KillOnDrop(
        tokio::process::Command::new("helm")
            .args([
                "upgrade",
                "--install",
                "--dependency-update",
                instance_name,
                &staged.chart_dir.to_string_lossy(),
                "-n",
                &staged.ns,
                "--values",
                &staged.values.path().to_string_lossy(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not run helm: {e}"))?,
    );
    let status = tokio::time::timeout(HELM_TIMEOUT, pump(&mut child, log))
        .await
        .map_err(|_| {
            anyhow::anyhow!("installing {instance_name} took too long and was stopped")
        })??;
    if !status.success() {
        anyhow::bail!("{instance_name} could not be installed — the log above is helm's own");
    }
    Ok(())
}

async fn pump(child: &mut KillOnDrop, log: &Log) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::io::AsyncBufReadExt;
    let mut out = child
        .0
        .stdout
        .take()
        .map(|s| tokio::io::BufReader::new(s).lines());
    let mut err = child
        .0
        .stderr
        .take()
        .map(|s| tokio::io::BufReader::new(s).lines());
    let mut out_done = out.is_none();
    let mut err_done = err.is_none();
    while !out_done || !err_done {
        tokio::select! {
            line = async { out.as_mut().unwrap().next_line().await }, if !out_done => match line {
                Ok(Some(line)) => log.say(line),
                _ => out_done = true,
            },
            line = async { err.as_mut().unwrap().next_line().await }, if !err_done => match line {
                Ok(Some(line)) => log.say(line),
                _ => err_done = true,
            },
        }
    }
    Ok(child.0.wait().await?)
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
    work: F,
    done: String,
    after_failure: &'static str,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>>
where
    F: FnOnce(Log) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut running = std::pin::pin!(work(Log(tx)));
        let mut outcome = None;
        loop {
            tokio::select! {
                biased;
                Some(line) = rx.recv() => yield Ok(Event::default().data(line)),
                finished = &mut running, if outcome.is_none() => outcome = Some(finished),
                else => break,
            }
        }
        yield Ok(Event::default().data(verdict(outcome, &done, after_failure)));
    }
}

pub(crate) fn install_stream(
    cfg: Arc<Config>,
    plan: InstallPlan,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    let done = format!(
        "{} installed — run 'Scan outputs' once the pod is ready",
        plan.app_id
    );
    sse(
        move |log| async move { execute(&cfg, &plan, &log).await },
        done,
        "Nothing was left behind, so you can try again.",
    )
}

pub(crate) fn upgrade_stream(
    cfg: Arc<Config>,
    plan: UpgradePlan,
) -> impl futures::Stream<Item = std::result::Result<Event, Infallible>> {
    let done = format!("{} updated", plan.app_id);
    sse(
        move |log| async move { upgrade(&cfg, &plan, &log).await },
        done,
        "Your app was left as it was.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            service_name: String::new(),
            config: Map::new(),
            volumes: Vec::new(),
            resources: Default::default(),
            backup: BackupPolicy::default(),
        }
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
            &json!({}),
        )
        .unwrap();
        assert_eq!(plan.config, config);
        assert_eq!(plan.backup, BackupPolicy::default());
        assert_eq!(plan.data, None);
    }

    #[test]
    fn a_copy_keeps_the_originals_password_when_the_form_never_showed_it() {
        let uischema = json!({"admin_password": {"ui:widget": "PasswordWidget"}});
        let mut source = definition("gitea");
        source.config = Map::from_iter([("admin_password".into(), json!("original"))]);
        let form = Map::from_iter([
            ("admin_password".into(), json!("__redacted__")),
            ("subdomain".into(), json!("git-copy")),
        ]);
        let plan = plan("gitea", "gitea-cd34", form, Some(&source), None, &uischema).unwrap();
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
            Some(&source),
            None,
            &json!({}),
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
            Some(&source),
            None,
            &json!({}),
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
            Some(&source),
            None,
            &json!({})
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
            |log| async move {
                log.say("starting");
                anyhow::bail!("the chart exploded")
            },
            "gitea installed".to_string(),
            "Nothing was left behind.",
        );
        assert_eq!(events_of(stream).await, 2);
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
}
