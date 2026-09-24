use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{publish_everywhere, topic, Notification, Tunnel};
use crate::runtime::{Controller, Ctx, Scope, Tick};

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
        page: "/box/storage".into(),
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
                page: "/box/backups".into(),
            });
            continue;
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
                page: "/box/backups".into(),
            });
        }
    }
    alerts
}

async fn backup_source(silent: bool) -> Source {
    let alerts = match crate::routers::backup::list().await {
        Ok(sets) => match crate::routers::backup_common::list_managed_namespaces().await {
            Ok(mut namespaces) => {
                namespaces.push(String::new());
                Some(backup_alerts(
                    &sets,
                    &namespaces,
                    chrono::Utc::now(),
                    BACKUP_STALE_AFTER_HOURS,
                ))
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
                    page: "/box/storage".into(),
                })
                .collect(),
        ),
        silent: false,
    }
}

pub struct NotifierController {
    pub config: crate::config::Config,
}

impl Controller for NotifierController {
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
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let root = Path::new("/");
        let tunnel = Tunnel::read(&self.config.config_path)?;
        if !tunnel.enabled {
            return Ok(Tick::Idle(
                "this machine is not connected to the YoLab platform".into(),
            ));
        }
        let topic = topic(&self.config.config_path)?;
        let view = crate::heal::current_view(&self.config).await;
        let quiet = !sends_for_cluster(&ctx.node, &view.answering);
        let sources = [
            heal_source(&view.problems, quiet),
            backup_source(quiet).await,
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
            page: "/box/storage".into(),
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
            Some("https://cluster.6.yolab.io/box/storage")
        );
        let cleared = notification(&Change::Cleared(heal_alert("data_unreachable")), &tunnel());
        assert_eq!(cleared.title, "YoLab: resolved");

        let disk = Alert {
            key: "disk:sdb".into(),
            title: "A disk could not be added".into(),
            message: "busy".into(),
            page: "/box/storage".into(),
        };
        let n = notification(&Change::Raised(disk), &tunnel());
        assert_eq!(n.title, "node1: A disk could not be added");
        assert_eq!(
            n.click.as_deref(),
            Some("https://node1.6.yolab.io/box/storage")
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
}
