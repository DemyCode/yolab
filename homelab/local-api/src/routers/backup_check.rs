use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::Host;
use crate::records::Store;
use crate::routers::backup_common::*;
use crate::runtime::{Activity, Controller, Ctx, Requirement, Scope, Tick};

pub(crate) const CHECKS: Store = Store {
    name: "yolab-backup-checks",
    namespace: "kube-system",
    key: "stores",
};
const EVERY: chrono::Duration = chrono::Duration::days(7);
const CHECK_TIMEOUT: Duration = Duration::from_secs(3 * 3600);
const REPAIR_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct Checked {
    pub ok: bool,
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default)]
    pub repaired: bool,
}

pub(crate) type Checks = BTreeMap<String, Checked>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Busy,
    Healthy,
    RepairIndex,
    Broken(String),
}

fn summary_of(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v["message_type"] == "summary")
}

fn first_error(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v["message_type"] == "error")
        .and_then(|v| v["message"].as_str().map(str::to_string))
}

pub(crate) fn verdict(success: bool, stdout: &str, stderr: &str) -> Verdict {
    let summary = summary_of(stdout);
    let errors = summary
        .as_ref()
        .and_then(|s| s["num_errors"].as_u64())
        .unwrap_or(0);
    let broken_packs = summary
        .as_ref()
        .and_then(|s| s["broken_packs"].as_array())
        .is_some_and(|p| !p.is_empty());
    if success && errors == 0 {
        return Verdict::Healthy;
    }
    if !success && summary.is_none() && stderr.contains("already locked") {
        return Verdict::Busy;
    }
    if summary
        .as_ref()
        .is_some_and(|s| s["suggest_repair_index"] == true)
        && !broken_packs
    {
        return Verdict::RepairIndex;
    }
    let why = first_error(stdout)
        .or_else(|| {
            stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_else(|| format!("restic found {errors} problem(s)"));
    Verdict::Broken(why)
}

pub(crate) fn due(last: Option<&Checked>, now: DateTime<Utc>) -> bool {
    let Some(at) = last
        .and_then(|c| DateTime::parse_from_rfc3339(&c.checked_at).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return true;
    };
    now - at >= EVERY
}

async fn check<H: Host>(host: &H, cfg: &BackupConfig, store: &str) -> anyhow::Result<Verdict> {
    cfg.unlock(host, store).await;
    let out = restic_with(
        host,
        &cfg.restic_repo(store),
        cfg,
        &["check", "--json"],
        CHECK_TIMEOUT,
    )
    .await?;
    Ok(verdict(out.success, &out.stdout, &out.stderr))
}

async fn check_and_repair<H: Host>(
    host: &H,
    cfg: &BackupConfig,
    store: &str,
) -> anyhow::Result<Option<Checked>> {
    let now = Utc::now().to_rfc3339();
    let mut repaired = false;
    let mut found = check(host, cfg, store).await?;
    if found == Verdict::RepairIndex {
        let out = restic_with(
            host,
            &cfg.restic_repo(store),
            cfg,
            &["repair", "index"],
            REPAIR_TIMEOUT,
        )
        .await?;
        if !out.success {
            found = Verdict::Broken(format!(
                "its index is damaged and could not be repaired: {}",
                out.stderr.trim()
            ));
        } else {
            repaired = true;
            found = check(host, cfg, store).await?;
        }
    }
    Ok(Some(match found {
        Verdict::Busy => return Ok(None),
        Verdict::Healthy => Checked {
            ok: true,
            checked_at: now,
            problem: None,
            repaired,
        },
        Verdict::RepairIndex => Checked {
            ok: false,
            checked_at: now,
            problem: Some("its index is still damaged after a repair".into()),
            repaired,
        },
        Verdict::Broken(why) => Checked {
            ok: false,
            checked_at: now,
            problem: Some(why),
            repaired,
        },
    }))
}

pub(crate) async fn read_checks(client: &kube::Client) -> anyhow::Result<Checks> {
    Ok(CHECKS.read(client).await?)
}

pub(crate) fn stores_to_check(services: impl IntoIterator<Item = String>) -> Vec<String> {
    std::iter::once(crate::routers::backup_points::SETTINGS_REPO.to_string())
        .chain(
            services
                .into_iter()
                .map(|ns| crate::routers::folder_backup::store(&ns)),
        )
        .collect()
}

pub struct BackupCheckController;

impl Controller for BackupCheckController {
    fn name(&self) -> &'static str {
        "backup-check"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(6 * 3600)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[Activity::Restore]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(600)
    }
    async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
        check_due(&Backend::real().await?).await
    }
}

pub(crate) async fn check_due<H: Host>(b: &Backend<H>) -> anyhow::Result<Tick> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Tick::Idle("backups are not enabled".into()));
    };
    let services = crate::routers::backup_points::read_sizes(&b.kube).await?;
    let checks = read_checks(&b.kube).await?;
    let now = Utc::now();
    let due_now: Vec<String> = stores_to_check(services.into_keys())
        .into_iter()
        .filter(|s| due(checks.get(s), now))
        .collect();
    let Some(store) = due_now.first() else {
        return Ok(Tick::Idle(
            "every backup store was checked this week".into(),
        ));
    };
    let Some(result) = check_and_repair(&b.host, &cfg, store).await? else {
        return Ok(Tick::Idle(format!(
            "{store} is in use, it is checked later"
        )));
    };
    if !result.ok {
        tracing::warn!(
            "backup check: {store} has a problem: {}",
            result.problem.as_deref().unwrap_or("unknown")
        );
    }
    CHECKS
        .update(&b.kube, |all: &mut Checks| {
            all.insert(store.clone(), result.clone());
        })
        .await?;
    Ok(Tick::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_check_is_healthy() {
        let out = r#"{"message_type":"summary","num_errors":0,"broken_packs":null,"suggest_repair_index":false,"suggest_prune":true}"#;
        assert_eq!(
            verdict(true, out, ""),
            Verdict::Healthy,
            "a prune hint is not a problem"
        );
    }

    #[test]
    fn a_damaged_index_without_broken_files_is_repaired() {
        let out = "{\"message_type\":\"error\",\"message\":\"index damaged\"}\n{\"message_type\":\"summary\",\"num_errors\":1,\"broken_packs\":[],\"suggest_repair_index\":true}";
        assert_eq!(verdict(false, out, ""), Verdict::RepairIndex);
    }

    #[test]
    fn broken_files_are_reported_never_repaired_quietly() {
        let out = "{\"message_type\":\"error\",\"message\":\"pack 1a2b is damaged\"}\n{\"message_type\":\"summary\",\"num_errors\":1,\"broken_packs\":[\"1a2b\"],\"suggest_repair_index\":true}";
        assert_eq!(
            verdict(false, out, ""),
            Verdict::Broken("pack 1a2b is damaged".into())
        );
    }

    #[test]
    fn a_check_that_could_not_run_is_a_problem_with_restics_own_words() {
        assert_eq!(
            verdict(false, "", "Fatal: wrong password or no key found\n"),
            Verdict::Broken("Fatal: wrong password or no key found".into())
        );
    }

    #[test]
    fn a_store_locked_by_a_running_backup_is_checked_later_not_reported() {
        assert_eq!(
            verdict(
                false,
                "",
                "Fatal: unable to create lock in backend: repository is already locked by PID 7\n"
            ),
            Verdict::Busy
        );
    }

    #[test]
    fn each_store_is_checked_once_a_week() {
        let now = Utc::now();
        let at = |days: i64| Checked {
            ok: true,
            checked_at: (now - chrono::Duration::days(days)).to_rfc3339(),
            ..Default::default()
        };
        assert!(due(None, now));
        assert!(!due(Some(&at(3)), now));
        assert!(due(Some(&at(7)), now));
        assert!(
            due(Some(&Checked::default()), now),
            "an unreadable time is checked again"
        );
    }

    #[test]
    fn the_settings_store_and_every_service_store_are_checked() {
        assert_eq!(
            stores_to_check(["yolab-a".to_string()]),
            vec!["cluster-backup".to_string(), "services/yolab-a".to_string()]
        );
    }
}
