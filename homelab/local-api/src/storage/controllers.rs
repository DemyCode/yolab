use std::future::Future;
use std::time::Duration;

use anyhow::Result;

use crate::host::{Host, HOST};
use crate::runtime::{lock, Controller, Ctx, Requirement, Scope, Tick};

use super::{
    bootstrap, containerd_store, csi_secrets, dashboard, images_grow, images_rbd, keys, lock_name,
    mon_member, osd, StorageEnv,
};

use super::wait::Attempt;

pub(crate) fn tick_of(a: Attempt<()>) -> Tick {
    match a {
        Attempt::Ready(()) => Tick::Done,
        Attempt::NotYet(why) => Tick::NotYet(why),
    }
}

async fn done<F>(f: F) -> Result<Tick>
where
    F: Future<Output = Result<()>>,
{
    f.await?;
    Ok(Tick::Done)
}

async fn locked<F, Fut>(job: &str, f: F) -> Result<Tick>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    locked_tick(job, || done(f())).await
}

async fn locked_tick<F, Fut>(job: &str, f: F) -> Result<Tick>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Tick>>,
{
    locked_tick_in(std::path::Path::new(lock::LOCK_DIR), job, f).await
}

async fn locked_tick_in<F, Fut>(dir: &std::path::Path, job: &str, f: F) -> Result<Tick>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Tick>>,
{
    let Some(_guard) = lock::try_acquire_in(dir, &lock_name(job))? else {
        return Ok(Tick::Idle(format!(
            "another run of {job} holds its lock right now"
        )));
    };
    f().await
}

#[cfg(test)]
async fn locked_in<F, Fut>(dir: &std::path::Path, job: &str, f: F) -> Result<Tick>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    locked_tick_in(dir, job, || done(f())).await
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
                locked_tick($job, || $body).await
            }
        }
    };
}

storage_controller! {
    OsdActivateController, name: "osd-activate", job: "osd-activate",
    every: Duration::from_secs(120), after_boot: Duration::ZERO,
    requires: [Requirement::Ceph],
    run: |_env, _node| done(osd::run(&HOST))
}

storage_controller! {
    SystemOsdController, name: "system-osd", job: "system-osd",
    every: Duration::from_secs(60), after_boot: Duration::ZERO,
    requires: [Requirement::Ceph],
    run: |_env, _node| async {
        Ok(tick_of(crate::disks_reconciler::system_osd_attempt(&HOST).await?))
    }
}

storage_controller! {
    ImagesRbdController, name: "images-rbd", job: "images-rbd",
    every: Duration::from_secs(60), after_boot: Duration::ZERO,
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let policy = env.images_rbd_policy();
        Ok(tick_of(images_rbd::attempt(&HOST, node, &policy).await?))
    }
}

storage_controller! {
    ImagesGrowController, name: "images-grow", job: "images-grow",
    every: Duration::from_secs(120), after_boot: Duration::from_secs(600),
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let policy = env.grow_policy();
        done(images_grow::run(&HOST, root(), node, &policy)).await
    }
}

storage_controller! {
    DashboardController, name: "ceph-dashboard", job: "dashboard",
    every: Duration::from_secs(120), after_boot: Duration::ZERO,
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let policy = env.dashboard_policy();
        done(dashboard::run(&HOST, node, &policy)).await
    }
}

storage_controller! {
    MonMemberController, name: "mon-member", job: "mon-member",
    every: Duration::from_secs(120), after_boot: Duration::ZERO,
    requires: [Requirement::Ceph],
    run: |env, node| async {
        let args = env.mon_member_args();
        done(mon_member::run(&HOST, root(), node, &args)).await
    }
}

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
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph, Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        locked("csi-secrets", || csi_secrets::run(&HOST)).await
    }
}

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
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph]
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
                keys::mint(&HOST, daemon).await?;
                ensure_started(&HOST, &format!("ceph-{daemon}-{}.service", ctx.node)).await;
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
            bootstrap::run(&HOST, root(), &ctx.node, &args).await?;
            ensure_started(&HOST, &format!("ceph-mon-{}.service", ctx.node)).await;
            Ok(())
        })
        .await
    }
}

async fn ensure_started<H: Host>(host: &H, unit: &str) {
    let active = host
        .systemctl(&["is-active", "--quiet", unit])
        .await
        .is_ok_and(|o| o.success);
    if active {
        return;
    }
    let _ = host.systemctl(&["reset-failed", unit]).await;
    match host.systemctl(&["start", "--no-block", unit]).await {
        Ok(o) if o.success => tracing::info!("started {unit}"),
        Ok(o) => tracing::warn!("could not start {unit}: {}", o.stderr.trim()),
        Err(e) => tracing::warn!("could not start {unit}: {e}"),
    }
}

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
        once_per_boot(std::path::Path::new(CSI_RECOVERED_MARKER), &HOST).await
    }
}

async fn once_per_boot<H: Host>(marker: &std::path::Path, host: &H) -> Result<Tick> {
    if marker.exists() {
        return Ok(Tick::Idle("already done this boot".into()));
    }
    if !crate::csi::plugin_daemonset_exists(host).await? {
        return Ok(Tick::RequeueAfter(Duration::from_secs(15)));
    }
    crate::csi::restart_local_plugin(host).await?;
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(marker, "")?;
    Ok(Tick::Done)
}

pub const K3S_UNIT: &str = "k3s.service";

pub struct ContainerdStoreResource {
    pub env: StorageEnv,
}

impl crate::runtime::resource::Resource for ContainerdStoreResource {
    fn name(&self) -> &'static str {
        "containerd-store"
    }
    fn depends_on(&self) -> &[&'static str] {
        &["images-rbd"]
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn disruption(&self) -> crate::runtime::resource::Disruption {
        crate::runtime::resource::Disruption::RestartsWorkloads
    }
    async fn check(&self, _ctx: &Ctx) -> crate::runtime::resource::State {
        use crate::runtime::resource::State;
        if containerd_store::is_mounted(&HOST, root()).await {
            State::Ready
        } else {
            State::NotYet("containerd's data-root is still on the root filesystem".into())
        }
    }
    async fn converge(&self, ctx: &Ctx) -> Result<Tick> {
        let policy = self.env.containerd_store_policy();
        locked_tick("containerd-store", || async {
            Ok(tick_of(
                containerd_store::pivot(&HOST, root(), &ctx.node, &policy, K3S_UNIT).await?,
            ))
        })
        .await
    }
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
    async fn a_restart_that_failed_is_not_marked_done_for_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let host = FakeHost::new()
            .ok("kubectl get daemonset csi-cephfsplugin", "{}")
            .fail("kubectl delete pod", "etcd timeout");
        assert!(once_per_boot(&marker, &host).await.is_err());
        assert!(!marker.exists(), "the next tick must try again");
    }

    #[tokio::test]
    async fn a_job_whose_lock_is_held_is_skipped_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let _boot_unit = lock::try_acquire_in(dir.path(), &lock_name("images-rbd"))
            .unwrap()
            .unwrap();
        let ran = std::sync::atomic::AtomicBool::new(false);
        let tick = locked_in(dir.path(), "images-rbd", || async {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap();
        assert!(matches!(tick, Tick::Idle(_)));
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    async fn succeed() -> Result<()> {
        Ok(())
    }

    async fn time_out() -> Result<()> {
        anyhow::bail!("ceph-volume timed out")
    }

    #[tokio::test]
    async fn a_free_job_runs_and_its_error_is_the_tick_error() {
        let dir = tempfile::tempdir().unwrap();
        let ok = locked_in(dir.path(), "osd-activate", succeed).await;
        assert_eq!(ok.unwrap(), Tick::Done);
        let failed = locked_in(dir.path(), "osd-activate", time_out).await;
        assert!(failed.is_err());
        let free = lock::try_acquire_in(dir.path(), &lock_name("osd-activate"));
        assert!(free.unwrap().is_some());
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

    #[tokio::test]
    async fn a_running_daemon_is_left_alone() {
        let host = FakeHost::new().ok("systemctl is-active", "");
        ensure_started(&host, "ceph-mgr-n1.service").await;
        assert!(!host.ran("systemctl start"));
        assert!(!host.ran("systemctl reset-failed"));
    }

    #[tokio::test]
    async fn a_daemon_that_hit_its_start_limit_is_cleared_before_being_started() {
        let host = FakeHost::new()
            .fail("systemctl is-active", "inactive")
            .ok("systemctl reset-failed", "")
            .ok("systemctl start", "");
        ensure_started(&host, "ceph-mgr-n1.service").await;
        let cleared = host
            .position("systemctl reset-failed")
            .expect("a failed unit was never cleared, so systemctl start refuses it");
        let started = host.position("systemctl start").expect("never started");
        assert!(
            cleared < started,
            "reset-failed must come before start: {:?}",
            host.calls()
        );
    }
}
