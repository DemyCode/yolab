use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backup::common::*;
use crate::host::Host;
use crate::records::Store;
use crate::runtime::{Activity, Ctx, Resource, Scope, Tick};

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
    std::iter::once(crate::backup::points::SETTINGS_REPO.to_string())
        .chain(
            services
                .into_iter()
                .map(|ns| crate::routers::folder_backup::store(&ns)),
        )
        .collect()
}

pub struct BackupCheckController;

impl Resource for BackupCheckController {
    fn name(&self) -> &'static str {
        "backup-check"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(6 * 3600)
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    fn pauses_during(&self) -> &'static [Activity] {
        &[Activity::Restore]
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(600)
    }
    async fn converge(&self, ctx: &Ctx) -> anyhow::Result<Tick> {
        check_due(&ctx.tools.backend().await?).await
    }
}

pub(crate) async fn check_due<H: Host>(b: &Backend<H>) -> anyhow::Result<Tick> {
    let Some(cfg) = master_config(&b.kube).await? else {
        return Ok(Tick::Idle("backups are not enabled".into()));
    };
    let services = crate::backup::points::read_sizes(&b.kube).await?;
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
    fn a_failed_check_with_no_words_says_how_many_problems_restic_counted() {
        assert_eq!(
            verdict(false, "", " \n"),
            Verdict::Broken("restic found 0 problem(s)".into())
        );
        let summary = r#"{"message_type":"summary","num_errors":2,"suggest_repair_index":false}"#;
        assert_eq!(
            verdict(true, summary, ""),
            Verdict::Broken("restic found 2 problem(s)".into())
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

#[cfg(test)]
mod against_restic {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::k8s::testing::{api_server, secret_with, status};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CLEAN: &str = r#"{"message_type":"summary","num_errors":0,"broken_packs":null,"suggest_repair_index":false}"#;
    const DAMAGED_INDEX: &str = r#"{"message_type":"summary","num_errors":1,"broken_packs":[],"suggest_repair_index":true}"#;

    fn cfg() -> BackupConfig {
        BackupConfig {
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            bucket: "b".into(),
            endpoint: "https://s3.example".into(),
            restic_password: "pw".into(),
        }
    }

    async fn checked(host: FakeHost) -> anyhow::Result<Option<Checked>> {
        check_and_repair(&host, &cfg(), "cluster-backup").await
    }

    #[tokio::test]
    async fn a_clean_store_is_recorded_healthy() {
        let found = checked(FakeHost::new().ok("restic check", CLEAN))
            .await
            .unwrap()
            .unwrap();
        assert!(found.ok);
        assert!(!found.repaired);
        assert_eq!(found.problem, None);
    }

    #[tokio::test]
    async fn a_damaged_index_is_repaired_then_checked_again() {
        let host = FakeHost::new()
            .ok("restic check", DAMAGED_INDEX)
            .ok("restic check", CLEAN)
            .ok("restic repair index", "");
        let found = checked(host).await.unwrap().unwrap();
        assert!(found.ok);
        assert!(found.repaired);
    }

    #[tokio::test]
    async fn an_index_still_damaged_after_a_repair_is_a_problem() {
        let host = FakeHost::new()
            .ok("restic check", DAMAGED_INDEX)
            .ok("restic check", DAMAGED_INDEX)
            .ok("restic repair index", "");
        let found = checked(host).await.unwrap().unwrap();
        assert!(!found.ok);
        assert!(found.repaired);
        assert_eq!(
            found.problem.as_deref(),
            Some("its index is still damaged after a repair")
        );
    }

    #[tokio::test]
    async fn a_repair_that_fails_is_reported_with_restic_s_words() {
        let host = FakeHost::new()
            .ok("restic check", DAMAGED_INDEX)
            .fail("restic repair index", "permission denied");
        let found = checked(host).await.unwrap().unwrap();
        assert!(!found.ok);
        let why = found.problem.unwrap();
        assert!(
            why.starts_with("its index is damaged and could not be repaired: ")
                && why.ends_with(": permission denied"),
            "{why}"
        );
    }

    #[tokio::test]
    async fn a_locked_store_is_left_for_later_and_a_missing_restic_is_an_error() {
        let busy = FakeHost::new().fail("restic check", "repository is already locked by PID 7");
        assert_eq!(checked(busy).await.unwrap(), None);
        assert!(checked(FakeHost::new().unstartable("restic check"))
            .await
            .is_err());
        let repair_unstartable = FakeHost::new()
            .ok("restic check", DAMAGED_INDEX)
            .unstartable("restic repair");
        assert!(checked(repair_unstartable).await.is_err());
    }

    #[tokio::test]
    async fn a_check_that_cannot_start_again_after_a_repair_is_an_error() {
        let host = FakeHost::new()
            .ok("restic check", DAMAGED_INDEX)
            .ok("restic repair index", "");
        let later = host.clone();
        let host = host.effect("restic repair index", move |_: &str, _: &[&str]| {
            later.clone().unstartable("restic check");
        });
        assert!(checked(host).await.is_err());
    }

    async fn backups_enabled(server: &MockServer, on: bool) {
        let response = if on {
            ResponseTemplate::new(200).set_body_json(secret_with("restic_password", "pw"))
        } else {
            ResponseTemplate::new(404).set_body_json(status(404, "NotFound"))
        };
        Mock::given(method("GET"))
            .and(path(
                "/api/v1/namespaces/kube-system/secrets/yolab-backup-config",
            ))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn record(server: &MockServer, name: &str, key: &str, value: Option<Value>) {
        let response = match value {
            Some(v) => ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": { "name": name, "namespace": "kube-system", "resourceVersion": "1" },
                "data": { key: v.to_string() }
            })),
            None => ResponseTemplate::new(404).set_body_json(status(404, "NotFound")),
        };
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/v1/namespaces/kube-system/configmaps/{name}"
            )))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn accept_writes(server: &MockServer) {
        for (verb, code) in [("PUT", 200), ("POST", 201)] {
            Mock::given(method(verb))
                .respond_with(ResponseTemplate::new(code).set_body_json(json!({
                    "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "yolab-backup-checks" }
                })))
                .mount(server)
                .await;
        }
    }

    async fn written(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| matches!(r.method.as_str(), "PUT" | "POST"))
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .filter_map(|v| {
                v["data"]["stores"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
            })
            .collect()
    }

    async fn tick(
        server_setup: impl std::future::Future<Output = ()>,
        server: &MockServer,
        kube: kube::Client,
        host: FakeHost,
    ) -> Tick {
        server_setup.await;
        accept_writes(server).await;
        check_due(&Backend { kube, host }).await.unwrap()
    }

    #[tokio::test]
    async fn nothing_is_checked_while_backups_are_off() {
        let (server, kube) = api_server().await;
        let t = tick(
            backups_enabled(&server, false),
            &server,
            kube,
            FakeHost::new(),
        )
        .await;
        assert!(matches!(t, Tick::Idle(why) if why == "backups are not enabled"));
    }

    #[tokio::test]
    async fn the_first_store_due_is_checked_and_its_result_recorded() {
        let (server, kube) = api_server().await;
        backups_enabled(&server, true).await;
        record(&server, "yolab-backup-sizes", "services", None).await;
        record(&server, "yolab-backup-checks", "stores", None).await;
        let t = tick(
            async {},
            &server,
            kube,
            FakeHost::new().ok("restic check", CLEAN),
        )
        .await;
        assert!(matches!(t, Tick::Done));
        assert_eq!(written(&server).await[0]["cluster-backup"]["ok"], true);
    }

    #[tokio::test]
    async fn a_store_with_a_problem_is_recorded_with_it() {
        let (server, kube) = api_server().await;
        backups_enabled(&server, true).await;
        record(&server, "yolab-backup-sizes", "services", None).await;
        record(&server, "yolab-backup-checks", "stores", None).await;
        let host = FakeHost::new().fail("restic check", "Fatal: wrong password");
        let t = tick(async {}, &server, kube, host).await;
        assert!(matches!(t, Tick::Done));
        let recorded = written(&server).await;
        let why = recorded[0]["cluster-backup"]["problem"].as_str().unwrap();
        assert!(why.ends_with("Fatal: wrong password"), "{why}");
    }

    #[tokio::test]
    async fn a_store_checked_this_week_or_in_use_waits() {
        let now = Utc::now().to_rfc3339();
        let (server, kube) = api_server().await;
        backups_enabled(&server, true).await;
        record(&server, "yolab-backup-sizes", "services", None).await;
        record(
            &server,
            "yolab-backup-checks",
            "stores",
            Some(json!({ "cluster-backup": { "ok": true, "checked_at": now } })),
        )
        .await;
        let t = tick(async {}, &server, kube, FakeHost::new()).await;
        assert!(matches!(t, Tick::Idle(why) if why == "every backup store was checked this week"));

        let (server, kube) = api_server().await;
        backups_enabled(&server, true).await;
        record(&server, "yolab-backup-sizes", "services", None).await;
        record(&server, "yolab-backup-checks", "stores", None).await;
        let busy = FakeHost::new().fail("restic check", "repository is already locked");
        let t = tick(async {}, &server, kube, busy).await;
        assert!(
            matches!(t, Tick::Idle(why) if why == "cluster-backup is in use, it is checked later")
        );
        assert!(written(&server).await.is_empty());
    }

    async fn enabled_with(
        sizes: Option<Value>,
        checks: Option<Value>,
    ) -> (MockServer, kube::Client) {
        let (server, kube) = api_server().await;
        backups_enabled(&server, true).await;
        record(&server, "yolab-backup-sizes", "services", sizes).await;
        record(&server, "yolab-backup-checks", "stores", checks).await;
        (server, kube)
    }

    #[tokio::test]
    async fn unreadable_sizes_or_checks_are_errors_not_a_quiet_week() {
        let (_server, kube) = enabled_with(Some(json!("garbage")), None).await;
        let host = FakeHost::new().ok("restic check", CLEAN);
        assert!(check_due(&Backend { kube, host }).await.is_err());
        let (_server, kube) = enabled_with(None, Some(json!("garbage"))).await;
        let host = FakeHost::new().ok("restic check", CLEAN);
        assert!(check_due(&Backend { kube, host }).await.is_err());
    }

    #[tokio::test]
    async fn a_check_that_cannot_start_is_an_error_and_nothing_is_recorded() {
        let (server, kube) = enabled_with(None, None).await;
        accept_writes(&server).await;
        let host = FakeHost::new().unstartable("restic check");
        assert!(check_due(&Backend { kube, host }).await.is_err());
        assert!(written(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_result_that_cannot_be_recorded_is_an_error() {
        let (server, kube) = enabled_with(None, None).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_json(status(500, "InternalError")))
            .mount(&server)
            .await;
        let host = FakeHost::new().ok("restic check", CLEAN);
        assert!(check_due(&Backend { kube, host }).await.is_err());
    }

    #[tokio::test]
    async fn the_check_controller_without_a_cluster_is_an_error() {
        let ctx = Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools::real(),
        };
        assert!(BackupCheckController.converge(&ctx).await.is_err());
    }

    #[tokio::test]
    async fn the_check_controller_runs_weekly_checks_away_from_restores_and_boot() {
        let c = BackupCheckController;
        assert_eq!(c.name(), "backup-check");
        assert_eq!(c.scope(), Scope::Cluster);
        assert_eq!(c.interval(), Duration::from_secs(6 * 3600));
        assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
        assert_eq!(c.pauses_during(), &[Activity::Restore]);
        assert_eq!(c.not_before_uptime(), Duration::from_secs(600));
        let ctx = Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools::unreachable(),
        };
        assert!(c.converge(&ctx).await.is_err());
    }
}
