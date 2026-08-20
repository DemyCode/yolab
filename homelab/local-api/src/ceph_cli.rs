//! Ceph access via the local host binaries.
//!
//! This replaces `kubectl::ceph_exec`, which found a Rook pod and ran `kubectl
//! exec` into it. That approach had two problems this one does not:
//!
//!   1. It required a healthy k3s API server to answer *any* storage question.
//!      During the 20-hour kubelet crash-loop outage, the Storage page could not
//!      report so much as free capacity, because every path to Ceph ran through
//!      the very thing that was broken.
//!   2. It ran the `ceph` CLI inside a daemon's own memory cgroup. The mgr has a
//!      512Mi limit, and invoking the CLI there OOM-killed it — reproduced live,
//!      and the likely source of the "N daemons have recently crashed" warnings.
//!
//! Ceph now runs as host daemons (see homelab/nixos/ceph/), so this is a plain
//! subprocess against /etc/ceph/ceph.conf and the admin keyring. No pod
//! discovery, no keyring copying, no 30s exec timeout, and it keeps working when
//! Kubernetes does not.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;

/// Ceph commands are fast, but a wedged mon can make them hang forever. Every
/// call is bounded so a storage hiccup degrades the UI instead of blocking the
/// whole local-api task pool.
const TIMEOUT_SECS: u64 = 30;

async fn run_bin(bin: &str, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        Command::new(bin).args(args).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{bin} timed out after {TIMEOUT_SECS}s"))?
    .with_context(|| format!("spawn {bin}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Ceph writes routine cephx negotiation chatter to stderr even on
        // success paths; surface the last line, which is the actual error.
        let msg = stderr.lines().last().unwrap_or("unknown error").to_string();
        bail!("{bin} {}: {msg}", args.join(" "))
    }
}

/// `ceph <args>`.
pub async fn ceph(args: &[&str]) -> Result<String> {
    run_bin("ceph", args).await
}

/// `ceph <args> -f json`, parsed.
pub async fn ceph_json(args: &[&str]) -> Result<Value> {
    let mut a = args.to_vec();
    a.extend_from_slice(&["-f", "json"]);
    let raw = ceph(&a).await?;
    serde_json::from_str(&raw).with_context(|| format!("parse json from `ceph {}`", args.join(" ")))
}

/// `rbd <args>`.
///
/// Unused from Rust today: the images-store systemd units in
/// homelab/nixos/ceph/images-store.nix drive rbd directly, because they must run
/// before k3s and therefore before local-api exists. Kept because surfacing
/// image-store usage on the Storage page is the obvious next consumer.
#[allow(dead_code)]
pub async fn rbd(args: &[&str]) -> Result<String> {
    run_bin("rbd", args).await
}

/// `ceph-volume <args>`. Creating an OSD is the one operation whose runtime is
/// unbounded in practice (it wipes labels, creates LVs, and mkfs's BlueStore),
/// so it gets its own generous limit rather than the shared 30s.
pub async fn ceph_volume(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        Command::new("ceph-volume").args(args).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ceph-volume timed out after 600s"))?
    .context("spawn ceph-volume")?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        bail!(
            "ceph-volume {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// True when the cluster answers at all. Used to gate anything that would
/// otherwise interpret "Ceph unreachable" as "Ceph says no".
pub async fn reachable() -> bool {
    ceph(&["-s"]).await.is_ok()
}

/// The cluster's own fsid. Callers compare it against the fsid in a disk's
/// BlueStore superblock to tell our disks from a stranger's — so an unreachable
/// mon must yield None, never a default, or every foreign disk starts looking
/// like ours. See the `is_uuid_rejects_the_empty_string` test in
/// disks_reconciler.rs for why that distinction is load-bearing.
pub async fn cluster_fsid() -> Option<String> {
    // `ceph fsid -f json` returns {"fsid": "..."} on some releases and a bare
    // UUID on others. Accept either rather than depending on which one ships,
    // because returning None here makes every labelled disk look foreign.
    if let Ok(v) = ceph_json(&["fsid"]).await {
        if let Some(f) = v["fsid"].as_str().filter(|s| !s.is_empty()) {
            return Some(f.to_string());
        }
    }
    ceph(&["fsid"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ceph's own confirmation that destroying this OSD loses no data.
///
/// The only signal trusted for that question — never infer it from reweight or
/// PG counts. Inferring it from `pg ls-by-osd` once caused real data loss; see
/// the drain-OSD incident notes.
pub async fn osd_safe_to_destroy(osd_id: i64) -> bool {
    ceph_json(&["osd", "safe-to-destroy", &format!("osd.{osd_id}")])
        .await
        .ok()
        .and_then(|v| {
            v["safe_to_destroy"]
                .as_array()
                .map(|a| a.iter().any(|x| x.as_i64() == Some(osd_id)))
        })
        .unwrap_or(false)
}

/// Map of device path -> OSD id for OSDs on this host, from `ceph-volume lvm
/// list`. This is the authoritative local view: it reads the LVM tags
/// ceph-volume itself wrote, so it needs no mon and works even when the cluster
/// is down — unlike `ceph osd metadata`, which does.
///
/// Errors propagate. Callers must NOT treat a failure as "no OSDs": the disk
/// reconciler creates an OSD for any ON disk missing from this map, and
/// `ceph-volume lvm create` wipes the device. See `refuse_osd_creation`.
pub async fn local_osds() -> Result<Vec<(String, i64)>> {
    let raw = ceph_volume(&["lvm", "list", "--format", "json"]).await?;
    crate::disks_reconciler::parse_lvm_list(&raw)
}
