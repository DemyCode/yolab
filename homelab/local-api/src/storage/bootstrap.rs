//! Create or join the Ceph cluster: keyrings, monmap, `ceph-mon --mkfs`.
//!
//! Both paths end at the same place — a mon store this node owns, in
//! `/var/lib/ceph/mon/ceph-<host>` — after which every node is a peer: its
//! own mon, mgr, MDS and OSDs, none a master. The one asymmetry is
//! `join_seed_addr`: empty means this machine creates the cluster, non-empty
//! means it fetches an existing one's credentials first. See
//! homelab/nixos/ceph/default.nix's header for the quorum-membership recovery
//! procedure and the reasoning behind never automating it.
//!
//! THE FSID CHECK (`validate_join_fsid`) IS THE ONE THING IN THIS FILE THAT
//! MUST NEVER BE SKIPPED. A joining node that `ceph-mon --mkfs`s against the
//! wrong cluster's monmap does not fail loudly — it quietly creates a second,
//! isolated cluster that looks healthy on both sides until someone notices
//! their data is not where they left it.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    auth::CLUSTER_AUTH_HEADER, config::read_account_token, host::Host,
    routers::ceph_join::CephJoinBundle,
};

use super::ceph_shared::{addrvec, mon_dir};

pub struct BootstrapArgs {
    pub fsid: String,
    /// This node's own mon address (the WireGuard cluster address).
    pub mon_addr: String,
    /// Empty on the machine that creates the cluster; otherwise the address
    /// of a machine already in it.
    pub join_seed_addr: String,
    /// Where to read `[tunnel] account_token` from, on the join path only.
    pub config_path: String,
}

fn admin_keyring_path(root: &Path) -> std::path::PathBuf {
    root.join("etc/ceph/ceph.client.admin.keyring")
}

fn bootstrap_osd_keyring_path(root: &Path) -> std::path::PathBuf {
    root.join("var/lib/ceph/bootstrap-osd/ceph.keyring")
}

fn tmp_mon_keyring_path(root: &Path) -> std::path::PathBuf {
    root.join("tmp/ceph.mon.keyring")
}

fn tmp_monmap_path(root: &Path) -> std::path::PathBuf {
    root.join("tmp/monmap")
}

/// THE data-loss-prevention check in this file. A mismatch must halt before a
/// single byte is written — see this module's header.
pub fn validate_join_fsid(bundle_fsid: &str, expected_fsid: &str, seed: &str) -> Result<()> {
    if bundle_fsid != expected_fsid {
        bail!(
            "refusing to join: [{seed}] runs cluster {bundle_fsid}, this node is \
             configured for {expected_fsid}"
        );
    }
    Ok(())
}

fn write_keyring(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn fetch_join_bundle(seed_addr: &str, token: &str) -> Result<CephJoinBundle> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("build http client")?;
    client
        .get(format!("http://[{seed_addr}]:3001/api/cluster/ceph-join"))
        .header(CLUSTER_AUTH_HEADER, token)
        .send()
        .await
        .with_context(|| format!("[{seed_addr}] did not hand over the cluster credentials"))?
        .error_for_status()
        .with_context(|| format!("[{seed_addr}] rejected the join request"))?
        .json::<CephJoinBundle>()
        .await
        .context("parse the join bundle")
}

/// Create the cluster: generate the mon/admin/bootstrap-osd keyrings and a
/// fresh one-mon monmap naming this host.
async fn create_cluster<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    mon_addr: &str,
    fsid: &str,
) -> Result<()> {
    let tmp_mon = tmp_mon_keyring_path(root);
    let admin = admin_keyring_path(root);
    let bootstrap_osd = bootstrap_osd_keyring_path(root);
    for p in [&tmp_mon, &admin, &bootstrap_osd] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp_mon_s = tmp_mon.to_string_lossy().into_owned();
    let admin_s = admin.to_string_lossy().into_owned();
    let bootstrap_osd_s = bootstrap_osd.to_string_lossy().into_owned();

    run_ok(
        host,
        "ceph-authtool",
        &[
            "--create-keyring",
            &tmp_mon_s,
            "--gen-key",
            "-n",
            "mon.",
            "--cap",
            "mon",
            "allow *",
        ],
    )
    .await?;
    run_ok(
        host,
        "ceph-authtool",
        &[
            "--create-keyring",
            &admin_s,
            "--gen-key",
            "-n",
            "client.admin",
            "--cap",
            "mon",
            "allow *",
            "--cap",
            "osd",
            "allow *",
            "--cap",
            "mds",
            "allow *",
            "--cap",
            "mgr",
            "allow *",
        ],
    )
    .await?;
    run_ok(
        host,
        "ceph-authtool",
        &[
            "--create-keyring",
            &bootstrap_osd_s,
            "--gen-key",
            "-n",
            "client.bootstrap-osd",
            "--cap",
            "mon",
            "profile bootstrap-osd",
            "--cap",
            "mgr",
            "allow r",
        ],
    )
    .await?;
    run_ok(
        host,
        "ceph-authtool",
        &[&tmp_mon_s, "--import-keyring", &admin_s],
    )
    .await?;
    run_ok(
        host,
        "ceph-authtool",
        &[&tmp_mon_s, "--import-keyring", &bootstrap_osd_s],
    )
    .await?;

    let monmap = tmp_monmap_path(root).to_string_lossy().into_owned();
    let addr = addrvec(mon_addr);
    run_ok(
        host,
        "monmaptool",
        &["--create", "--addv", node, &addr, "--fsid", fsid, &monmap],
    )
    .await?;

    std::fs::create_dir_all(mon_dir(root, node))?;
    Ok(())
}

/// Join an existing cluster: adopt its keyrings, then fetch the live monmap
/// (never a copy carried in the bundle — see routers/ceph_join.rs's header for
/// why: a copy is a snapshot that goes stale the moment mon membership
/// changes, and by the time this runs the admin keyring already lets this
/// node ask any reachable mon for the current one).
async fn join_cluster<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    bundle: &CephJoinBundle,
) -> Result<()> {
    let admin = admin_keyring_path(root);
    let bootstrap_osd = bootstrap_osd_keyring_path(root);
    let tmp_mon = tmp_mon_keyring_path(root);
    write_keyring(&admin, &bundle.admin_keyring)?;
    write_keyring(&bootstrap_osd, &bundle.bootstrap_osd_keyring)?;
    write_keyring(&tmp_mon, &bundle.mon_keyring)?;

    let admin_s = admin.to_string_lossy().into_owned();
    let bootstrap_osd_s = bootstrap_osd.to_string_lossy().into_owned();
    let _ = host.run_cmd("chown", &["ceph:ceph", &admin_s]).await;
    let _ = host
        .run_cmd("chown", &["ceph:ceph", &bootstrap_osd_s])
        .await;

    // Cleared first so a *failed* attempt below can never leave a stale
    // monmap from an earlier retry of this same subcommand sitting there to
    // be misread as fresh by the size check after the loop — `-o` always
    // truncates-and-writes on a successful call, so only a failure can leave
    // old bytes behind.
    let monmap = tmp_monmap_path(root);
    let monmap_s = monmap.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&monmap);
    for _ in 0..30 {
        if host
            .run_cmd(
                "ceph",
                &["--connect-timeout", "10", "mon", "getmap", "-o", &monmap_s],
            )
            .await
            .is_ok_and(|o| o.success)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    // Checked once, after the loop — not per-attempt. A `ceph mon getmap`
    // that exits 0 but somehow wrote nothing is exactly as much "not fetched"
    // as 30 straight connection failures, and both are caught here.
    if !monmap.metadata().is_ok_and(|m| m.len() > 0) {
        bail!("could not fetch a monmap from the cluster; retrying on the timer");
    }

    let dir = mon_dir(root, node);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(())
}

/// The tail shared by both paths: `ceph-mon --mkfs` against the monmap and
/// keyring each path left in the tmp files, then hand the store to `ceph`.
async fn finish_mkfs<H: Host>(host: &H, root: &Path, node: &str) -> Result<()> {
    let tmp_mon = tmp_mon_keyring_path(root);
    let monmap = tmp_monmap_path(root);
    let dir = mon_dir(root, node);
    let tmp_mon_s = tmp_mon.to_string_lossy().into_owned();
    let monmap_s = monmap.to_string_lossy().into_owned();

    run_ok(
        host,
        "ceph-mon",
        &[
            "--mkfs",
            "-i",
            node,
            "--monmap",
            &monmap_s,
            "--keyring",
            &tmp_mon_s,
        ],
    )
    .await?;

    std::fs::copy(&tmp_mon, dir.join("keyring"))?;
    let ceph_dir = root.join("var/lib/ceph");
    let ceph_dir_s = ceph_dir.to_string_lossy().into_owned();
    let _ = host
        .run_cmd("chown", &["-R", "ceph:ceph", &ceph_dir_s])
        .await;
    let admin_s = admin_keyring_path(root).to_string_lossy().into_owned();
    let _ = host.run_cmd("chown", &["ceph:ceph", &admin_s]).await;
    let _ = std::fs::remove_file(&tmp_mon);
    let _ = std::fs::remove_file(&monmap);
    Ok(())
}

async fn run_ok<H: Host>(host: &H, bin: &str, args: &[&str]) -> Result<()> {
    let out = host.run_cmd(bin, args).await?;
    if !out.success {
        bail!("{bin} {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(())
}

pub async fn run<H: Host>(host: &H, root: &Path, node: &str, args: &BootstrapArgs) -> Result<()> {
    if mon_dir(root, node).join("keyring").exists() {
        tracing::info!("{node}: already a member of this cluster");
        return Ok(());
    }

    if args.join_seed_addr.is_empty() {
        create_cluster(host, root, node, &args.mon_addr, &args.fsid).await?;
    } else {
        let token = read_account_token(&args.config_path);
        if token.is_empty() {
            bail!(
                "no tunnel.account_token in {} — cannot authenticate to [{}]",
                args.config_path,
                args.join_seed_addr
            );
        }
        let bundle = fetch_join_bundle(&args.join_seed_addr, &token).await?;
        validate_join_fsid(&bundle.fsid, &args.fsid, &args.join_seed_addr)
            .map_err(|e| anyhow!(e))?;
        join_cluster(host, root, node, &bundle).await?;
    }

    finish_mkfs(host, root, node).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{fake::FakeHost, CommandOutput};
    use std::future::Future;

    const FSID: &str = "11111111-2222-3333-4444-555555555555";

    /// Wraps a `FakeHost` and, for the handful of binaries this file shells
    /// out to that write their result to a path named in argv (`ceph-authtool
    /// --create-keyring <path>`, `monmaptool ... <path>`, `ceph mon getmap -o
    /// <path>`), actually writes a placeholder file there. `FakeHost` alone
    /// only fakes a command's exit status/stdout, which is right for most
    /// commands but wrong for these three — their entire job, from this
    /// file's point of view, IS the file they leave behind, and the retry
    /// loop around `mon getmap` specifically re-checks that file on disk.
    /// Confined to tests: production always shells out to the real binaries.
    #[derive(Clone)]
    struct FileWritingHost {
        inner: FakeHost,
    }

    impl FileWritingHost {
        fn new(inner: FakeHost) -> Self {
            Self { inner }
        }
    }

    fn simulate_file_write(bin: &str, args: &[&str]) {
        let path = if bin == "ceph-authtool" {
            args.iter()
                .position(|a| *a == "--create-keyring")
                .and_then(|i| args.get(i + 1))
        } else if bin == "monmaptool" {
            args.last()
        } else if bin == "ceph" && args.contains(&"getmap") {
            args.iter()
                .position(|a| *a == "-o")
                .and_then(|i| args.get(i + 1))
        } else {
            None
        };
        if let Some(path) = path {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, "fake-bytes-from-a-real-binary");
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for FileWritingHost {
        fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.ceph(args)
        }
        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<serde_json::Value>> + Send + 'a {
            self.inner.ceph_json(args)
        }
        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.ceph_volume(args)
        }
        fn kubectl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            self.inner.kubectl(args)
        }
        fn kubectl_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<serde_json::Value>> + Send + 'a {
            self.inner.kubectl_json(args)
        }
        fn kubectl_apply<'a>(
            &self,
            manifest: &'a str,
        ) -> impl Future<Output = Result<()>> + Send + 'a {
            self.inner.kubectl_apply(manifest)
        }
        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            self.inner.systemctl(args)
        }
        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let out = me.inner.run_cmd(bin, args).await?;
                if out.success {
                    simulate_file_write(bin, args);
                }
                Ok(out)
            }
        }
    }

    fn bundle(fsid: &str) -> CephJoinBundle {
        CephJoinBundle {
            fsid: fsid.to_string(),
            mon_keyring: "[mon.]\nkey = AAA==\n".to_string(),
            admin_keyring: "[client.admin]\nkey = BBB==\n".to_string(),
            bootstrap_osd_keyring: "[client.bootstrap-osd]\nkey = CCC==\n".to_string(),
            mon_addrs: vec!["fd00:cafe::1".to_string()],
        }
    }

    // ── validate_join_fsid: the one check that must never be bypassed ────────

    #[test]
    fn matching_fsids_are_accepted() {
        assert!(validate_join_fsid(FSID, FSID, "fd00:cafe::1").is_ok());
    }

    #[test]
    fn a_mismatched_fsid_is_refused() {
        let err = validate_join_fsid("other-cluster-fsid", FSID, "fd00:cafe::1").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refusing to join"));
        assert!(msg.contains("other-cluster-fsid"));
        assert!(msg.contains(FSID));
    }

    #[test]
    fn an_empty_bundle_fsid_is_refused_not_treated_as_a_match() {
        assert!(validate_join_fsid("", FSID, "fd00:cafe::1").is_err());
    }

    // ── create_cluster + finish_mkfs: the happy path, against a tempdir root ──

    #[tokio::test]
    async fn create_path_produces_a_keyring_and_never_touches_the_network() {
        let host = FileWritingHost::new(
            FakeHost::new()
                .ok("ceph-authtool", "")
                .ok("monmaptool", "")
                .ok("ceph-mon --mkfs", "")
                .ok("chown", ""),
        );
        let dir = tempfile::tempdir().unwrap();
        let args = BootstrapArgs {
            fsid: FSID.to_string(),
            mon_addr: "fd00:cafe::1".to_string(),
            join_seed_addr: String::new(),
            config_path: "/nonexistent/config.toml".to_string(),
        };

        run(&host, dir.path(), "yolab-n1", &args).await.unwrap();

        assert!(mon_dir(dir.path(), "yolab-n1").join("keyring").exists());
        assert!(host.inner.ran("monmaptool"));
        assert!(
            !host.inner.ran("mon getmap"),
            "the create path must never fetch a monmap over the network"
        );
    }

    #[tokio::test]
    async fn create_path_is_idempotent_once_the_keyring_exists() {
        let host = FakeHost::new(); // no calls scripted — none should happen
        let dir = tempfile::tempdir().unwrap();
        let mon = mon_dir(dir.path(), "yolab-n1");
        std::fs::create_dir_all(&mon).unwrap();
        std::fs::write(mon.join("keyring"), "already here").unwrap();
        let args = BootstrapArgs {
            fsid: FSID.to_string(),
            mon_addr: "fd00:cafe::1".to_string(),
            join_seed_addr: String::new(),
            config_path: "/nonexistent/config.toml".to_string(),
        };

        run(&host, dir.path(), "yolab-n1", &args).await.unwrap();

        assert!(host.calls().is_empty());
    }

    // ── join_cluster: the credential + monmap handoff ─────────────────────────

    #[tokio::test]
    async fn join_writes_the_bundles_keyrings_and_fetches_a_live_monmap() {
        let host = FileWritingHost::new(
            FakeHost::new()
                .ok("chown", "")
                .ok("ceph --connect-timeout 10 mon getmap", "")
                .ok("ceph-mon --mkfs", ""),
        );
        let dir = tempfile::tempdir().unwrap();
        let b = bundle(FSID);

        join_cluster(&host, dir.path(), "yolab-n2", &b)
            .await
            .unwrap();
        finish_mkfs(&host, dir.path(), "yolab-n2").await.unwrap();

        assert_eq!(
            std::fs::read_to_string(admin_keyring_path(dir.path())).unwrap(),
            b.admin_keyring
        );
        assert!(mon_dir(dir.path(), "yolab-n2").join("keyring").exists());
    }

    // 30 attempts x 2s between them — paused time so this resolves instantly.
    #[tokio::test(start_paused = true)]
    async fn join_gives_up_after_thirty_failed_monmap_fetches() {
        let host = FakeHost::new()
            .ok("chown", "")
            .fail("ceph --connect-timeout 10 mon getmap", "mon unreachable");
        let dir = tempfile::tempdir().unwrap();
        let b = bundle(FSID);

        let err = join_cluster(&host, dir.path(), "yolab-n2", &b)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not fetch a monmap"));
        // The keyrings were still written — they are needed for every retry —
        // but mkfs must never have been reached without a real monmap.
        assert!(admin_keyring_path(dir.path()).exists());
        assert!(!host.ran("ceph-mon --mkfs"));
    }
}
