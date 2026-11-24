use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{publish_everywhere, topic, Notification, Tunnel};
use crate::backup::common::Backend;
use crate::host::Host;
use crate::runtime::{Ctx, Resource, Scope, Tick};

const NAME: &str = "notifier";
const STATE_FILE: &str = "var/lib/yolab/ntfy/alerts.json";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    pub key: String,
    pub title: String,
    pub message: String,
    pub page: String,
}

pub(crate) struct Source {
    pub prefix: &'static str,
    pub alerts: Option<Vec<Alert>>,
    pub silent: bool,
}

fn is_cluster_wide(key: &str) -> bool {
    key.starts_with("heal:") || key.starts_with("backup:")
}

fn sends_for_cluster(me: &str, answering: &[String]) -> bool {
    answering
        .iter()
        .map(String::as_str)
        .min()
        .is_none_or(|lowest| lowest == me)
}

#[derive(Debug, PartialEq)]
enum Change {
    Raised(Alert),
    Cleared(Alert),
}

fn changes(sent: &BTreeMap<String, Alert>, sources: &[Source]) -> Vec<Change> {
    let mut out = Vec::new();
    for source in sources {
        let Some(now) = &source.alerts else {
            continue;
        };
        for alert in now {
            if !sent.contains_key(&alert.key) {
                out.push(Change::Raised(alert.clone()));
            }
        }
        for (key, alert) in sent.range(source.prefix.to_string()..) {
            if !key.starts_with(source.prefix) {
                break;
            }
            if !now.iter().any(|a| &a.key == key) {
                out.push(Change::Cleared(alert.clone()));
            }
        }
    }
    out
}

fn notification(change: &Change, tunnel: &Tunnel) -> Notification {
    let alert = match change {
        Change::Raised(a) | Change::Cleared(a) => a,
    };
    let (who, host) = if is_cluster_wide(&alert.key) {
        (
            "YoLab".to_string(),
            tunnel
                .shared_host("cluster")
                .unwrap_or_else(|| tunnel.host.clone()),
        )
    } else {
        (tunnel.machine_label(), tunnel.host.clone())
    };
    let click = |page: &str| Some(format!("https://{host}{page}"));
    match change {
        Change::Raised(a) => Notification {
            title: format!("{who}: {}", a.title),
            message: a.message.clone(),
            priority: 4,
            tags: vec!["warning".into()],
            click: click(&a.page),
        },
        Change::Cleared(a) => Notification {
            title: format!("{who}: resolved"),
            message: a.title.clone(),
            priority: 3,
            tags: vec!["white_check_mark".into()],
            click: click(&a.page),
        },
    }
}

#[cfg(not(test))]
fn state_root() -> PathBuf {
    PathBuf::from("/")
}

#[cfg(test)]
fn state_root() -> PathBuf {
    std::env::temp_dir().join(format!("yolab-test-notifier-{}", std::process::id()))
}

fn state_path(root: &Path) -> PathBuf {
    root.join(STATE_FILE)
}

fn load(root: &Path) -> Result<BTreeMap<String, Alert>> {
    match std::fs::read(state_path(root)) {
        Ok(raw) => serde_json::from_slice(&raw)
            .with_context(|| format!("{} is unreadable", state_path(root).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", state_path(root).display())),
    }
}

fn save(root: &Path, sent: &BTreeMap<String, Alert>) -> Result<()> {
    crate::config::write_private_file(&state_path(root), &serde_json::to_vec_pretty(sent)?)
}

fn heal_alert(problem: &str) -> Alert {
    let title = match problem {
        "machines_gone" => "A machine does not answer",
        "ceph_no_quorum" => "Storage has lost its quorum",
        "kubernetes_down" => "The cluster is not answering",
        "data_unreachable" => "Some of your files have no reachable copy",
        other => other,
    };
    Alert {
        key: format!("heal:{problem}"),
        title: title.to_string(),
        message: "Open the Storage page to see what is wrong.".into(),
        page: "/system/storage".into(),
    }
}

fn heal_source(problems: &[&str], silent: bool) -> Source {
    Source {
        prefix: "heal:",
        alerts: Some(problems.iter().copied().map(heal_alert).collect()),
        silent,
    }
}

const BACKUP_STALE_AFTER_HOURS: i64 = 36;

fn app_label(namespace: &str) -> String {
    if namespace.is_empty() {
        "This machine".to_string()
    } else {
        namespace
            .strip_prefix("yolab-")
            .unwrap_or(namespace)
            .to_string()
    }
}

fn newest_for<'a>(sets: &'a [serde_json::Value], namespace: &str) -> Option<&'a serde_json::Value> {
    sets.iter()
        .find(|s| s["namespace"].as_str().unwrap_or("") == namespace)
}

fn check_alerts(checks: &crate::backup::check::Checks) -> Vec<Alert> {
    checks
        .iter()
        .filter(|(_, c)| !c.ok)
        .map(|(store, c)| Alert {
            key: format!("backup:check:{store}"),
            title: "A backup store has a problem".to_string(),
            message: format!(
                "The weekly check of {store} found: {}. Your newest backups may not restore until this is fixed.",
                c.problem.as_deref().unwrap_or("an unknown problem")
            ),
            page: "/system/backups".into(),
        })
        .collect()
}

fn backup_alerts(
    sets: &[serde_json::Value],
    namespaces: &[String],
    now: chrono::DateTime<chrono::Utc>,
    stale_after_hours: i64,
) -> Vec<Alert> {
    let mut alerts = Vec::new();
    for namespace in namespaces {
        let newest = newest_for(sets, namespace);
        let state = newest.and_then(|s| s["state"].as_str()).unwrap_or("never");
        if matches!(state, "running" | "queued") {
            continue;
        }
        let name = app_label(namespace);
        if state == "crashed" {
            let why = newest
                .and_then(|s| s["error"].as_str())
                .filter(|e| !e.is_empty())
                .unwrap_or("it did not finish");
            alerts.push(Alert {
                key: format!("backup:{namespace}"),
                title: format!("{name} could not be backed up"),
                message: why.to_string(),
                page: "/system/backups".into(),
            });
            continue;
        }
        if let Some(warning) = newest
            .and_then(|s| s["warning"].as_str())
            .filter(|w| !w.is_empty())
        {
            alerts.push(Alert {
                key: format!("backup:{namespace}:budget"),
                title: format!("{name}'s backups need your attention"),
                message: warning.to_string(),
                page: "/system/backups".into(),
            });
        }
        let age = newest
            .and_then(|s| s["finished_at"].as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| (now - t.with_timezone(&chrono::Utc)).num_hours());
        let stale = match age {
            Some(hours) => hours >= stale_after_hours,
            None => true,
        };
        if stale {
            alerts.push(Alert {
                key: format!("backup:{namespace}"),
                title: format!("{name} has no recent backup"),
                message: match age {
                    Some(hours) => format!("The last good copy is {hours}h old."),
                    None => "It has never been backed up.".to_string(),
                },
                page: "/system/backups".into(),
            });
        }
    }
    alerts
}

async fn backup_source<H: Host>(backend: &anyhow::Result<Backend<H>>, silent: bool) -> Source {
    let read = async {
        let b = backend.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let sets = crate::backup::run::list(&b.kube).await?;
        anyhow::Ok((sets, b))
    };
    let alerts = match read.await {
        Ok((sets, b)) => match crate::backup::common::managed_namespaces(&b.kube).await {
            Ok(mut namespaces) => {
                namespaces.push(String::new());
                let mut alerts = backup_alerts(
                    &sets,
                    &namespaces,
                    chrono::Utc::now(),
                    BACKUP_STALE_AFTER_HOURS,
                );
                match crate::backup::check::read_checks(&b.kube).await {
                    Ok(checks) => {
                        alerts.extend(check_alerts(&checks));
                        Some(alerts)
                    }
                    Err(e) => {
                        tracing::debug!("notifier: backup checks unreadable: {e:#}");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::debug!("notifier: app list unreadable: {e:#}");
                None
            }
        },
        Err(e) => {
            tracing::debug!("notifier: backup records unreadable: {e:#}");
            None
        }
    };
    Source {
        prefix: "backup:",
        alerts,
        silent,
    }
}

fn disk_source() -> Source {
    Source {
        prefix: "disk:",
        alerts: Some(
            crate::disks_reconciler::stuck_disks()
                .into_iter()
                .map(|(disk, message)| Alert {
                    key: format!("disk:{disk}"),
                    title: "A disk could not be added".into(),
                    message,
                    page: "/system/storage".into(),
                })
                .collect(),
        ),
        silent: false,
    }
}

pub struct NotifierController {
    pub config: crate::config::Config,
}

impl Resource for NotifierController {
    fn name(&self) -> &'static str {
        NAME
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(180)
    }
    async fn converge(&self, ctx: &Ctx) -> Result<Tick> {
        let root = state_root();
        let root = root.as_path();
        let tunnel = Tunnel::read(&self.config.config_path)?;
        if !tunnel.enabled {
            return Ok(Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        }
        let topic = topic(&self.config.config_path)?;
        let view = crate::heal::current_view(&ctx.tools.host, &ctx.tools.kube, &self.config).await;
        let quiet = !sends_for_cluster(&ctx.node, &view.answering);
        let backend = ctx.tools.backend().await;
        let sources = [
            heal_source(&view.problems, quiet),
            backup_source(&backend, quiet).await,
            disk_source(),
        ];
        let mut sent = load(root)?;
        let mut failed = None;
        let mut delivered = 0;
        for source in &sources {
            for change in changes(&sent, std::slice::from_ref(source)) {
                if !source.silent {
                    let n = notification(&change, &tunnel);
                    if let Err(e) =
                        publish_everywhere(&self.config, &topic, &n, &view.peer_addrs).await
                    {
                        failed = Some(e);
                        continue;
                    }
                    delivered += 1;
                }
                match change {
                    Change::Raised(a) => {
                        sent.insert(a.key.clone(), a);
                    }
                    Change::Cleared(a) => {
                        sent.remove(&a.key);
                    }
                }
            }
        }
        save(root, &sent)?;
        match failed {
            Some(e) => Err(e.context("send a notification (retried next tick)")),
            None if delivered == 0 => Ok(Tick::Idle(format!(
                "nothing new; {} problem(s) followed",
                sent.len()
            ))),
            None => Ok(Tick::Done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn alert(key: &str) -> Alert {
        Alert {
            key: key.into(),
            title: format!("title of {key}"),
            message: "m".into(),
            page: "/system/storage".into(),
        }
    }

    fn sent(keys: &[&str]) -> BTreeMap<String, Alert> {
        keys.iter().map(|k| (k.to_string(), alert(k))).collect()
    }

    #[test]
    fn a_new_problem_is_sent_once_and_its_end_once() {
        let now = [Source {
            prefix: "heal:",
            alerts: Some(vec![alert("heal:kubernetes_down")]),
            silent: false,
        }];
        assert_eq!(
            changes(&sent(&[]), &now),
            vec![Change::Raised(alert("heal:kubernetes_down"))]
        );
        assert!(
            changes(&sent(&["heal:kubernetes_down"]), &now).is_empty(),
            "not repeated"
        );

        let gone = [Source {
            prefix: "heal:",
            alerts: Some(vec![]),
            silent: false,
        }];
        assert_eq!(
            changes(&sent(&["heal:kubernetes_down"]), &gone),
            vec![Change::Cleared(alert("heal:kubernetes_down"))]
        );
    }

    #[test]
    fn a_source_that_cannot_answer_neither_repeats_nor_resolves() {
        let unknown = [Source {
            prefix: "backup:",
            alerts: None,
            silent: false,
        }];
        assert!(changes(&sent(&["backup:bk-1"]), &unknown).is_empty());
    }

    #[test]
    fn a_source_only_resolves_its_own_problems() {
        let disks = [Source {
            prefix: "disk:",
            alerts: Some(vec![]),
            silent: false,
        }];
        let before = sent(&["disk:sdb", "heal:machines_gone", "backup:bk-1"]);
        assert_eq!(
            changes(&before, &disks),
            vec![Change::Cleared(alert("disk:sdb"))]
        );
    }

    fn at(hours_ago: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(hours_ago)).to_rfc3339()
    }

    fn ns(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn alerts_now(sets: &[serde_json::Value], namespaces: &[String]) -> Vec<Alert> {
        backup_alerts(sets, namespaces, chrono::Utc::now(), 36)
    }

    #[test]
    fn one_apps_failure_is_not_hidden_by_another_apps_success() {
        let sets = [
            json!({"id": "bk-2", "namespace": "yolab-b", "state": "restorable", "finished_at": at(1)}),
            json!({"id": "bk-1", "namespace": "yolab-a", "state": "crashed", "error": "S3 refused"}),
        ];
        let alerts = alerts_now(&sets, &ns(&["yolab-a", "yolab-b"]));
        assert_eq!(alerts.len(), 1, "b is fine, a is not");
        assert_eq!(alerts[0].key, "backup:yolab-a");
        assert!(alerts[0].title.contains('a'), "{}", alerts[0].title);
        assert_eq!(alerts[0].message, "S3 refused");
    }

    #[test]
    fn an_alert_is_keyed_by_app_so_it_clears_when_that_app_succeeds() {
        let broken = [json!({"id": "bk-1", "namespace": "yolab-a", "state": "crashed"})];
        let key = alerts_now(&broken, &ns(&["yolab-a"]))[0].key.clone();
        let fixed = [
            json!({"id": "bk-2", "namespace": "yolab-a", "state": "restorable", "finished_at": at(1)}),
            json!({"id": "bk-1", "namespace": "yolab-a", "state": "crashed"}),
        ];
        assert!(alerts_now(&fixed, &ns(&["yolab-a"])).is_empty());
        assert_eq!(
            key, "backup:yolab-a",
            "the same key, so the old alert clears"
        );
    }

    #[test]
    fn an_app_that_silently_stopped_being_backed_up_is_reported() {
        let sets = [
            json!({"id": "bk-1", "namespace": "yolab-a", "state": "restorable", "finished_at": at(80)}),
        ];
        let alerts = alerts_now(&sets, &ns(&["yolab-a"]));
        assert_eq!(
            alerts.len(),
            1,
            "a backup that stopped running is the worst case"
        );
        assert!(
            alerts[0].title.contains("no recent backup"),
            "{}",
            alerts[0].title
        );
    }

    #[test]
    fn an_app_that_was_never_backed_up_at_all_is_reported() {
        let alerts = alerts_now(&[], &ns(&["yolab-a"]));
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("never"), "{}", alerts[0].message);
    }

    #[test]
    fn a_service_over_its_backup_budget_is_reported_apart_from_its_backup_state() {
        let sets = [json!({
            "id": "bk-1", "namespace": "yolab-a", "state": "restorable",
            "finished_at": at(1), "warning": "a's backups take 30 GB but its budget is 10 GB"
        })];
        let alerts = alerts_now(&sets, &ns(&["yolab-a"]));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "backup:yolab-a:budget");
        assert!(
            alerts[0].message.contains("budget"),
            "{}",
            alerts[0].message
        );
    }

    #[test]
    fn a_backup_store_that_failed_its_check_is_reported_and_a_healthy_one_is_not() {
        let checks = crate::backup::check::Checks::from([
            (
                "services/yolab-a".to_string(),
                crate::backup::check::Checked {
                    ok: false,
                    checked_at: at(1),
                    problem: Some("pack 1a2b is damaged".into()),
                    repaired: false,
                },
            ),
            (
                "cluster-backup".to_string(),
                crate::backup::check::Checked {
                    ok: true,
                    checked_at: at(1),
                    ..Default::default()
                },
            ),
        ]);
        let alerts = check_alerts(&checks);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "backup:check:services/yolab-a");
        assert!(alerts[0].message.contains("pack 1a2b is damaged"));
    }

    #[test]
    fn a_recent_success_raises_nothing() {
        let sets = [
            json!({"id": "bk-1", "namespace": "yolab-a", "state": "restorable", "finished_at": at(2)}),
        ];
        assert!(alerts_now(&sets, &ns(&["yolab-a"])).is_empty());
    }

    #[test]
    fn an_app_being_backed_up_right_now_is_not_nagged_about() {
        for state in ["running", "queued"] {
            let sets = [json!({"id": "bk-1", "namespace": "yolab-a", "state": state})];
            assert!(
                alerts_now(&sets, &ns(&["yolab-a"])).is_empty(),
                "{state} is work in progress, not a problem"
            );
        }
    }

    #[test]
    fn the_machine_snapshot_is_named_for_a_person_not_by_its_empty_namespace() {
        let alerts = alerts_now(&[], &[String::new()]);
        assert_eq!(alerts.len(), 1);
        assert!(
            alerts[0].title.starts_with("This machine"),
            "{}",
            alerts[0].title
        );
    }

    fn tunnel() -> Tunnel {
        Tunnel {
            enabled: true,
            platform_api_url: String::new(),
            account_token: String::new(),
            tunnel_id: "25".into(),
            host: "node1.6.yolab.io".into(),
        }
    }

    #[test]
    fn a_cluster_problem_opens_the_shared_address_a_machine_problem_its_own() {
        let raised = notification(&Change::Raised(heal_alert("data_unreachable")), &tunnel());
        assert_eq!(
            raised.title,
            "YoLab: Some of your files have no reachable copy"
        );
        assert_eq!(raised.priority, 4);
        assert_eq!(
            raised.click.as_deref(),
            Some("https://cluster.6.yolab.io/system/storage")
        );
        let cleared = notification(&Change::Cleared(heal_alert("data_unreachable")), &tunnel());
        assert_eq!(cleared.title, "YoLab: resolved");

        let disk = Alert {
            key: "disk:sdb".into(),
            title: "A disk could not be added".into(),
            message: "busy".into(),
            page: "/system/storage".into(),
        };
        let n = notification(&Change::Raised(disk), &tunnel());
        assert_eq!(n.title, "node1: A disk could not be added");
        assert_eq!(
            n.click.as_deref(),
            Some("https://node1.6.yolab.io/system/storage")
        );
    }

    #[test]
    fn the_lowest_answering_machine_sends_the_clusters_problems() {
        let answering = vec![
            "node2".to_string(),
            "node1".to_string(),
            "node3".to_string(),
        ];
        assert!(sends_for_cluster("node1", &answering));
        assert!(!sends_for_cluster("node2", &answering));
        assert!(sends_for_cluster(
            "node2",
            &["node2".into(), "node3".into()]
        ));
        assert!(sends_for_cluster("node1", &[]), "nothing answers: speak up");
        assert!(is_cluster_wide("backup:bk-1") && !is_cluster_wide("disk:sdb"));
    }

    #[test]
    fn what_was_sent_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        save(dir.path(), &sent(&["disk:sdb"])).unwrap();
        assert_eq!(load(dir.path()).unwrap(), sent(&["disk:sdb"]));
    }

    #[test]
    fn tests_never_write_the_machine_s_own_alert_state() {
        assert!(state_root().starts_with(std::env::temp_dir()));
    }

    #[test]
    fn each_storage_problem_has_a_plain_title_and_unknown_ones_keep_their_name() {
        let titles: Vec<String> = [
            "machines_gone",
            "ceph_no_quorum",
            "kubernetes_down",
            "data_unreachable",
            "something_new",
        ]
        .iter()
        .map(|p| heal_alert(p).title)
        .collect();
        assert_eq!(
            titles,
            [
                "A machine does not answer",
                "Storage has lost its quorum",
                "The cluster is not answering",
                "Some of your files have no reachable copy",
                "something_new",
            ]
        );
        let source = heal_source(&["machines_gone"], true);
        assert!(source.silent);
        assert_eq!(source.alerts.unwrap()[0].key, "heal:machines_gone");
    }

    #[test]
    fn a_cluster_problem_without_a_shared_address_opens_this_machine() {
        let mut bare = tunnel();
        bare.host = "node1".into();
        let raised = notification(&Change::Raised(heal_alert("machines_gone")), &bare);
        assert_eq!(
            raised.click.as_deref(),
            Some("https://node1/system/storage")
        );
    }

    #[test]
    fn an_unreadable_or_unopenable_state_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(state_path(dir.path()), "{broken").unwrap();
        assert!(load(dir.path()).is_err());
        std::fs::remove_file(state_path(dir.path())).unwrap();
        std::fs::create_dir(state_path(dir.path())).unwrap();
        assert!(load(dir.path()).is_err());
        assert!(save(dir.path(), &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_disk_source_is_never_silent() {
        let source = disk_source();
        assert_eq!(source.prefix, "disk:");
        assert!(!source.silent);
        assert!(source.alerts.is_some());
    }

    #[tokio::test]
    async fn backup_alerts_are_unknown_when_any_record_cannot_be_read() {
        use crate::k8s::testing::{api_server, list, serve, status};
        let unreachable: anyhow::Result<Backend<crate::host::fake::FakeHost>> =
            Err(anyhow::anyhow!("no cluster"));
        assert!(backup_source(&unreachable, false).await.alerts.is_none());

        let sets = "/api/v1/namespaces/kube-system/configmaps/yolab-backups";
        let checks = "/api/v1/namespaces/kube-system/configmaps/yolab-backup-checks";
        let namespaces = "/api/v1/namespaces";
        let gone = status(404, "NotFound");
        let down = status(503, "ServiceUnavailable");
        let empty = list("Namespace", vec![]);
        for (sets_code, ns_code, checks_code, known) in [
            (503, 200, 404, false),
            (404, 503, 404, false),
            (404, 200, 503, false),
            (404, 200, 404, true),
        ] {
            let (server, kube) = api_server().await;
            serve(
                &server,
                sets,
                sets_code,
                if sets_code == 404 {
                    gone.clone()
                } else {
                    down.clone()
                },
            )
            .await;
            serve(
                &server,
                namespaces,
                ns_code,
                if ns_code == 200 {
                    empty.clone()
                } else {
                    down.clone()
                },
            )
            .await;
            serve(
                &server,
                checks,
                checks_code,
                if checks_code == 404 {
                    gone.clone()
                } else {
                    down.clone()
                },
            )
            .await;
            let b = Ok(Backend {
                kube,
                host: crate::host::fake::FakeHost::new(),
            });
            let source = backup_source(&b, true).await;
            assert_eq!(
                source.alerts.is_some(),
                known,
                "{sets_code} {ns_code} {checks_code}"
            );
            assert!(source.silent);
        }
    }

    fn controller(config: &str) -> (tempfile::TempDir, NotifierController) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, config).unwrap();
        let config = crate::config::Config::for_test(&file);
        (dir, NotifierController { config })
    }

    fn ctx() -> Ctx {
        Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools::unreachable(),
        }
    }

    #[tokio::test]
    async fn the_notifier_waits_after_boot_and_is_idle_off_the_platform() {
        let (_dir, c) = controller("[tunnel]\nenabled = false\n");
        assert_eq!(c.name(), "notifier");
        assert_eq!(c.scope(), Scope::Node);
        assert_eq!(c.interval(), Duration::from_secs(120));
        assert_eq!(c.not_before_uptime(), Duration::from_secs(180));
        let t = c.converge(&ctx()).await.unwrap();
        assert!(
            matches!(t, Tick::Idle(why) if why == "this machine is not connected to the YoLab platform")
        );
        let (_dir, broken) = controller("[tunnel]\nenabled = true\n");
        assert!(
            broken.converge(&ctx()).await.is_err(),
            "no topic without a k3s token"
        );
        let (_dir, missing) = controller("");
        assert!(missing.converge(&ctx()).await.is_err());
    }

    #[tokio::test]
    async fn the_notifier_sends_what_changed_or_says_it_will_retry() {
        let (_dir, c) = controller(
            "[tunnel]\nenabled = true\ndns_url = \"https://node1.6.yolab.io\"\n[node.k3s]\ntoken = \"t\"\n",
        );
        match c.converge(&ctx()).await {
            Ok(t) => assert!(matches!(t, Tick::Idle(_) | Tick::Done)),
            Err(e) => assert!(format!("{e:#}").contains("send a notification"), "{e:#}"),
        }
        assert!(state_path(&state_root()).exists());
    }
}
