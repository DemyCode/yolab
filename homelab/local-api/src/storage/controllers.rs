//! The storage jobs as controllers — what used to be systemd timers.
//!
//! Each one:
//!   - takes the same lock as the job's boot oneshot and skips the tick if the
//!     boot unit is running it (see `storage/mod.rs`);
//!   - keeps the old timer's `OnBootSec` as `not_before_uptime`, so boot-time
//!     ordering against the oneshots is what it always was;
//!   - declares NO requirement unless the job genuinely has one. The image-store
//!     repair in particular must not wait for Ceph or Kubernetes: its whole
//!     point is releasing a dead mount when neither can answer (the "recovery
//!     before preflight" lesson from the 2026-09-10 outage).
//!
//! Intervals are the old timers' `OnUnitInactiveSec`: measured from the END of
//! one run to the start of the next, which is what the runtime does.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;

use crate::host::{Host, RealHost};
use crate::runtime::{lock, Controller, Ctx, Requirement, Scope, Tick};

use super::{
    bootstrap, containerd_store, csi_secrets, dashboard, images_grow, images_rbd, keys, lock_name,
    mon_member, osd, StorageEnv,
};

/// Runs `job` under its lock, or reports that the boot unit has it.
async fn locked<F, Fut>(job: &str, f: F) -> Result<Tick>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let Some(_guard) = lock::try_acquire(&lock_name(job))? else {
        return Ok(Tick::Idle(format!(
            "yolab-{job} (the boot unit) is running this right now"
        )));
    };
    f().await?;
    Ok(Tick::Done)
}

fn root() -> &'static std::path::Path {
    std::path::Path::new("/")
}

macro_rules! storage_controller {
    (
        $(#[$doc:meta])*
        $ty:ident, name: $name:literal, job: $job:literal,
        every: $every:expr, after_boot: $boot:expr,
        requires: [$($req:expr),*],
        run: |$env:ident, $node:ident| $body:expr
    ) => {
        $(#[$doc])*
        pub struct $ty {
            pub env: StorageEnv,
        }

        impl Controller for $ty {
            fn name(&self) -> &'static str {
                $name
            }
            fn scope(&self) -> Scope {
                Scope::Node
            }
            fn interval(&self) -> Duration {
                $every
            }
            fn not_before_uptime(&self) -> Duration {
                $boot
            }
            fn requires(&self) -> &'static [Requirement] {
                &[$($req),*]
            }
            async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
                let $env = &self.env;
                let $node = ctx.node.as_str();
                locked($job, || $body).await
            }
        }
    };
}

storage_controller! {
    /// Starts every prepared OSD whose daemon is not running. The boot unit does
    /// this once; this keeps doing it, so an OSD whose daemon died comes back
    /// without a reboot.
    OsdActivateController, name: "osd-activate", job: "osd-activate",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(180),
    requires: [],
    run: |_env, _node| osd::run(&RealHost)
}

storage_controller! {
    /// Keeps containerd's data-root on a working RBD — or off Ceph entirely when
    /// the pool cannot serve. No requirements, deliberately: see the module header.
    ContainerdStoreController, name: "containerd-store", job: "containerd-store",
    every: Duration::from_secs(300), after_boot: Duration::from_secs(240),
    requires: [],
    run: |env, node| async {
        let policy = env.containerd_store_policy();
        containerd_store::run(&RealHost, root(), node, &policy).await
    }
}

storage_controller! {
    /// Creates the images pool and this node's RBD once an OSD exists.
    ImagesRbdController, name: "images-rbd", job: "images-rbd",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(120),
    requires: [],
    run: |env, node| async {
        let policy = env.images_rbd_policy();
        images_rbd::run(&RealHost, node, &policy).await
    }
}

storage_controller! {
    /// Grows the images RBD as the pool grows.
    ImagesGrowController, name: "images-grow", job: "images-grow",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(600),
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let policy = env.grow_policy();
        images_grow::run(&RealHost, root(), node, &policy).await
    }
}

storage_controller! {
    /// Configures the Ceph dashboard on this node's mgr and keeps its password in
    /// step with the one the Storage page shows.
    DashboardController, name: "ceph-dashboard", job: "dashboard",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(240),
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let policy = env.dashboard_policy();
        dashboard::run(&RealHost, node, &policy).await
    }
}

storage_controller! {
    /// Adds this node's mon to the monmap when joining did not.
    MonMemberController, name: "mon-member", job: "mon-member",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(180),
    requires: [],
    run: |env, node| async {
        let args = env.mon_member_args();
        mon_member::run(&RealHost, root(), node, &args).await
    }
}

/// Publishes the host cluster's credentials into Kubernetes for ceph-csi.
/// Cluster-scoped: every node would write the same Secrets.
pub struct CsiSecretsController;

impl Controller for CsiSecretsController {
    fn name(&self) -> &'static str {
        "csi-secrets"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(240)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph, Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        locked("csi-secrets", || csi_secrets::run(&RealHost)).await
    }
}

/// Mints the mgr (and MDS) cephx keys when they are missing, then starts the
/// daemon. The boot oneshot's first attempt necessarily fails on a joining node
/// — the cluster credentials have not arrived yet — and a failed oneshot is
/// never retried on its own; this is the retry.
pub struct CephKeysController {
    pub env: StorageEnv,
}

impl Controller for CephKeysController {
    fn name(&self) -> &'static str {
        "ceph-keys"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(300)
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(120)
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        let mut daemons = vec!["mgr"];
        if self.env.mds {
            daemons.push("mds");
        }
        let mut problems = Vec::new();
        for daemon in daemons {
            let job = format!("{daemon}-key");
            let outcome = locked(&job, || async {
                keys::mint(&RealHost, daemon).await?;
                ensure_started(&RealHost, &format!("ceph-{daemon}-{}.service", ctx.node)).await;
                Ok(())
            })
            .await;
            if let Err(e) = outcome {
                problems.push(format!("{daemon}: {e:#}"));
            }
        }
        if problems.is_empty() {
            Ok(Tick::Done)
        } else {
            anyhow::bail!("{}", problems.join("; "))
        }
    }
}

/// Retries joining the Ceph cluster on a machine whose first boot could not.
/// Only on joining machines; the machine that created the cluster has nothing
/// to retry. Once the mon store exists `bootstrap::run` is a no-op.
pub struct CephJoinController {
    pub env: StorageEnv,
}

impl Controller for CephJoinController {
    fn name(&self) -> &'static str {
        "ceph-join"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
    fn not_before_uptime(&self) -> Duration {
        Duration::from_secs(60)
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        if self.env.join_seed_addr.is_empty() {
            return Ok(Tick::Idle("this machine created the cluster".into()));
        }
        let args = self.env.bootstrap_args();
        locked("bootstrap", || async {
            bootstrap::run(&RealHost, root(), &ctx.node, &args).await?;
            ensure_started(&RealHost, &format!("ceph-mon-{}.service", ctx.node)).await;
            Ok(())
        })
        .await
    }
}

/// Starts a unit if it is not already active. `--no-block`: several of these
/// units are ordered after the jobs that start them.
async fn ensure_started<H: Host>(host: &H, unit: &str) {
    let active = host
        .systemctl(&["is-active", "--quiet", unit])
        .await
        .is_ok_and(|o| o.success);
    if active {
        return;
    }
    match host.systemctl(&["start", "--no-block", unit]).await {
        Ok(o) if o.success => tracing::info!("started {unit}"),
        Ok(o) => tracing::warn!("could not start {unit}: {}", o.stderr.trim()),
        Err(e) => tracing::warn!("could not start {unit}: {e}"),
    }
}

/// Clears the CephFS CSI plugin's stale volume locks once per boot. Was
/// `yolab-csi-recovery.service`; a local-api restart must not repeat it, so
/// "once" is recorded in /run, which only a reboot clears.
pub struct CsiRecoveryController;

const CSI_RECOVERED_MARKER: &str = "/run/yolab/csi-recovered-this-boot";

impl Controller for CsiRecoveryController {
    fn name(&self) -> &'static str {
        "csi-recovery"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        once_per_boot(std::path::Path::new(CSI_RECOVERED_MARKER), &RealHost).await
    }
}

async fn once_per_boot<H: Host>(marker: &std::path::Path, host: &H) -> Result<Tick> {
    if marker.exists() {
        return Ok(Tick::Idle("already done this boot".into()));
    }
    // Rook may not have created the DaemonSet yet; a later tick tries again —
    // sooner than the interval, because the first mount after boot is what waits
    // on it.
    if !crate::csi::plugin_daemonset_exists(host).await? {
        return Ok(Tick::RequeueAfter(Duration::from_secs(15)));
    }
    crate::csi::restart_plugins(host, crate::csi::Which::ThisNode).await;
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(marker, "")?;
    Ok(Tick::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn csi_recovery_runs_once_per_boot() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("run/yolab/csi-recovered");
        let host = FakeHost::new()
            .ok("kubectl get daemonset csi-cephfsplugin", "{}")
            .ok("kubectl delete pod", "");
        assert_eq!(once_per_boot(&marker, &host).await.unwrap(), Tick::Done);
        assert!(host.ran("kubectl delete pod"));

        let again = FakeHost::new();
        assert!(matches!(
            once_per_boot(&marker, &again).await.unwrap(),
            Tick::Idle(_)
        ));
        assert!(again.calls().is_empty());
    }

    #[tokio::test]
    async fn csi_recovery_waits_for_the_daemonset_without_claiming_it_ran() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let host = FakeHost::new().fail(
            "kubectl get daemonset csi-cephfsplugin",
            "Error from server (NotFound): daemonsets.apps \"csi-cephfsplugin\" not found",
        );
        assert!(matches!(
            once_per_boot(&marker, &host).await.unwrap(),
            Tick::RequeueAfter(_)
        ));
        assert!(!marker.exists());
        assert!(!host.ran("delete pod"));
    }

    #[tokio::test]
    async fn an_already_running_unit_is_not_started_again() {
        let host = FakeHost::new().ok("systemctl is-active", "");
        ensure_started(&host, "ceph-mgr-n1.service").await;
        assert!(!host.ran("systemctl start"));

        let stopped = FakeHost::new()
            .fail("systemctl is-active", "inactive")
            .ok("systemctl start", "");
        ensure_started(&stopped, "ceph-mgr-n1.service").await;
        assert!(stopped.ran("systemctl start --no-block ceph-mgr-n1.service"));
    }
}
