//! Mint the mgr/mds cephx key. One shared flow because the two units differ
//! only in the daemon name, the keyring directory and the capabilities.

use std::path::Path;

use anyhow::{bail, Result};

use crate::host::Host;

use super::ceph_shared::hostname;

/// The `ceph auth get-or-create` caps for a daemon. The one bug-prone detail:
/// a typo here silently mints a key with the wrong grants, so it is pinned.
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

pub async fn mint<H: Host>(host: &H, daemon: &str) -> Result<()> {
    let Some(caps) = caps_for(daemon) else {
        bail!("unknown daemon '{daemon}'");
    };
    let node = hostname();
    let dir = format!("/var/lib/ceph/{daemon}/ceph-{node}");
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
}
