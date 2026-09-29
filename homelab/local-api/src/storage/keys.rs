use std::path::Path;

use anyhow::{bail, Result};

use crate::host::Host;

fn caps_for(daemon: &str) -> Option<Vec<&'static str>> {
    match daemon {
        "mgr" => Some(vec![
            "mon",
            "allow profile mgr",
            "osd",
            "allow *",
            "mds",
            "allow *",
        ]),
        "mds" => Some(vec![
            "mon",
            "profile mds",
            "mgr",
            "profile mds",
            "osd",
            "allow rwx",
            "mds",
            "allow *",
        ]),
        _ => None,
    }
}

fn daemon_dir(root: &Path, daemon: &str, node: &str) -> std::path::PathBuf {
    root.join(format!("var/lib/ceph/{daemon}/ceph-{node}"))
}

pub async fn mint<H: Host>(host: &H, root: &Path, node: &str, daemon: &str) -> Result<()> {
    let Some(caps) = caps_for(daemon) else {
        bail!("unknown daemon '{daemon}'");
    };
    let dir = daemon_dir(root, daemon, node)
        .to_string_lossy()
        .into_owned();
    let keyring = format!("{dir}/keyring");
    if Path::new(&keyring).exists() {
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;

    let mut reachable = false;
    for _ in 0..60 {
        if host.reachable().await {
            reachable = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !reachable {
        bail!("cluster not reachable — cannot mint the {daemon} key yet");
    }

    let name = format!("{daemon}.{node}");
    let mut args: Vec<&str> = vec!["auth", "get-or-create", name.as_str()];
    args.extend(caps.iter().copied());
    args.push("-o");
    args.push(keyring.as_str());
    host.ceph(&args).await?;

    let out = host
        .run_cmd("chown", &["-R", "ceph:ceph", dir.as_str()])
        .await?;
    if !out.success {
        bail!("chown ceph:ceph {dir}: {}", out.stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mgr_caps_grant_profile_mgr() {
        assert_eq!(
            caps_for("mgr"),
            Some(vec![
                "mon",
                "allow profile mgr",
                "osd",
                "allow *",
                "mds",
                "allow *",
            ])
        );
    }

    #[test]
    fn mds_caps_grant_profile_mds() {
        assert_eq!(
            caps_for("mds"),
            Some(vec![
                "mon",
                "profile mds",
                "mgr",
                "profile mds",
                "osd",
                "allow rwx",
                "mds",
                "allow *",
            ])
        );
    }

    #[test]
    fn unknown_daemon_has_no_caps() {
        assert_eq!(caps_for("osd"), None);
    }

    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn minting_writes_the_keyring_into_the_daemons_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph auth get-or-create", "")
            .ok("chown", "");
        mint(&host, dir.path(), "n1", "mgr").await.unwrap();
        let want = dir.path().join("var/lib/ceph/mgr/ceph-n1");
        assert!(want.is_dir());
        assert!(host.ran(&format!(
            "ceph auth get-or-create mgr.n1 mon allow profile mgr osd allow * mds allow * -o {}/keyring",
            want.display()
        )));
        assert!(host.ran(&format!("chown -R ceph:ceph {}", want.display())));
    }

    #[tokio::test]
    async fn an_existing_keyring_is_never_minted_again() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("var/lib/ceph/mds/ceph-n1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("keyring"), "[mds.n1]").unwrap();
        let host = FakeHost::new();
        mint(&host, dir.path(), "n1", "mds").await.unwrap();
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_daemon_is_refused_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new();
        let err = mint(&host, dir.path(), "n1", "osd").await.unwrap_err();
        assert!(err.to_string().contains("unknown daemon"));
        assert!(host.calls().is_empty());
        assert!(!dir.path().join("var/lib/ceph/osd").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_cluster_mints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new().fail("ceph -s", "timed out");
        let err = mint(&host, dir.path(), "n1", "mgr").await.unwrap_err();
        assert!(err.to_string().contains("not reachable"));
        assert!(!host.ran("ceph auth"));
    }

    #[tokio::test]
    async fn a_failed_chown_is_an_error_not_a_silently_root_owned_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph auth get-or-create", "")
            .fail("chown", "Operation not permitted");
        let err = mint(&host, dir.path(), "n1", "mgr").await.unwrap_err();
        assert!(err.to_string().contains("chown ceph:ceph"));
    }
}
