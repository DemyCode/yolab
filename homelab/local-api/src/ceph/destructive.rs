//! The only way to run a Ceph or LVM command that destroys data.
//!
//! WHY THIS IS A MODULE WITH A DOOR
//!
//! Every destructive incident in this project came from a command that was
//! correct in the place it was written and wrong in a state its author did not
//! picture: `pg ls-by-osd` said 0 for a DOWN OSD and the disk holding the only
//! copy was wiped; an unplugged disk marked `out` after 600s satisfied every
//! purge condition; `images_recover` and `storage_heal` both force-created the
//! same placement groups on their own clocks.
//!
//! The commands themselves were just strings passed to `ceph()`, so nothing
//! distinguished "list pools" from "delete pool" at the type level, and no
//! review could find every place that could destroy something by grepping for
//! one name.
//!
//! Now:
//!
//!   - `ceph_cli::ceph` / `ceph_volume` REFUSE any command `is_destructive`
//!     recognises, with `CmdError::Forbidden`, before spawning anything.
//!   - The door-holding variants take a `Door`, which only this module can
//!     construct.
//!   - Each function here that opens the door demands a proof value naming WHY
//!     the destruction is allowed — `SafeToDestroy` (Ceph itself confirmed it),
//!     `RecoveryMandate` (the owner pressed "recover from backup"), a
//!     `ZapWarrant`, or a `DisposablePg` — and those proofs can only be obtained
//!     by performing the check they stand for.
//!
//! A future bug can still pass the wrong proof. It can no longer forget to
//! have one.

use std::collections::BTreeSet;

use crate::ceph::model::{OsdDump, SafeToDestroyReport};
use crate::exec::{CmdError, Failure};
use crate::host::Host;

/// Permission to run a destructive command. The private field is the point:
/// nothing outside this module can build one.
pub struct Door(());

/// Pools holding nothing a machine cannot fetch or regenerate again. The only
/// pools whose placement groups may ever be rebuilt empty without a person
/// asking.
pub const DISPOSABLE_POOLS: &[&str] = &[".mgr", "images"];

/// The CephFS pools a recovery is allowed to delete and recreate, and nothing
/// else.
pub const RECOVERABLE_FS: &str = "yolab-fs";
pub const RECOVERABLE_FS_POOLS: &[&str] = &["yolab-fs-metadata", "yolab-fs-data0"];

/// Whether `bin args` destroys data. Matched on the command words, skipping
/// leading `--option value` pairs such as `--connect-timeout 10`.
pub fn is_destructive(bin: &str, args: &[&str]) -> bool {
    let words = command_words(args);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match bin {
        "ceph" => matches!(
            w.as_slice(),
            ["osd", "purge", ..]
                | ["osd", "destroy", ..]
                | ["osd", "lost", ..]
                | ["osd", "rm", ..]
                | ["osd", "force-create-pg", ..]
                | ["osd", "pool", "delete", ..]
                | ["osd", "pool", "rm", ..]
                | ["fs", "rm", ..]
                | ["fs", "fail", ..]
                | ["mon", "remove", ..]
                | ["mon", "rm", ..]
                | ["auth", "del", ..]
                | ["auth", "rm", ..]
                | ["pg", _, "mark_unfound_lost", ..]
                | ["config", "set", "mon", "mon_allow_pool_delete", "true", ..]
        ),
        "ceph-volume" => matches!(w.as_slice(), ["lvm", "zap", ..]),
        _ => false,
    }
}

fn command_words(args: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if let Some(opt) = a.strip_prefix("--") {
            // `--opt=value` is one word; `--opt value` is two, except for the
            // bare flags the destructive commands themselves carry.
            if !opt.contains('=')
                && !opt.starts_with("yes-i-really")
                && opt != "destroy"
                && i + 1 < args.len()
                && out.is_empty()
            {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        out.push(a.to_string());
        i += 1;
    }
    out
}

// ── Proof: Ceph says destroying this OSD loses nothing ───────────────────────

/// Obtained only from `safe_to_destroy`, which asked Ceph.
///
/// `ceph osd safe-to-destroy` is the one trusted signal — never PG counts, never
/// reweight. Inferring it from `pg ls-by-osd` once wiped the only copy of 686
/// objects, because a DOWN OSD has no PGs *mapped* while still holding the data.
#[derive(Debug)]
pub struct SafeToDestroy {
    osd: i64,
}

/// `Ok(Some)` when Ceph confirms, `Ok(None)` when Ceph says the OSD still holds
/// data (EBUSY), `Err` when Ceph did not answer. The caller cannot confuse the
/// last two: "could not ask" is not "not safe yet", and neither is "safe".
pub async fn safe_to_destroy<H: Host>(
    host: &H,
    osd: i64,
) -> Result<Option<SafeToDestroy>, CmdError> {
    let id = format!("osd.{osd}");
    let raw = match host
        .ceph(&["osd", "safe-to-destroy", &id, "-f", "json"])
        .await
    {
        Ok(raw) => raw,
        Err(e) if e.failure() == Some(Failure::Busy) => return Ok(None),
        Err(e) => return Err(e),
    };
    let report: SafeToDestroyReport = crate::exec::parse_json("ceph osd safe-to-destroy", &raw)?;
    Ok(report
        .safe_to_destroy
        .contains(&osd)
        .then_some(SafeToDestroy { osd }))
}

/// The receipt for a purge Ceph confirmed. Required to zap the disk the OSD
/// lived on.
#[derive(Debug)]
pub struct Purged {
    osd: i64,
}

/// Purges an OSD Ceph has confirmed is safe to destroy, then confirms it is gone
/// from `osd ls`. `Ok(None)` when the purge reported success but the OSD is
/// still listed: never hand out a `Purged` for something that is not.
pub async fn purge_safe<H: Host>(
    host: &H,
    proof: SafeToDestroy,
) -> Result<Option<Purged>, CmdError> {
    purge(host, proof.osd).await
}

async fn purge<H: Host>(host: &H, osd: i64) -> Result<Option<Purged>, CmdError> {
    let door = Door(());
    let id = format!("osd.{osd}");
    host.ceph_destructive(&door, &["osd", "purge", &id, "--yes-i-really-mean-it"])
        .await?;
    let still = host.osd_ids().await?;
    Ok((!still.contains(&osd)).then_some(Purged { osd }))
}

// ── Proof: the owner asked for a recovery from backup ────────────────────────

/// Issued from a persisted recovery record that a person started with the
/// "Recover health from backup" button. The recovery is a reset: it purges the
/// lost disks and replaces the filesystem, so its mandate is what allows those
/// two things and nothing else.
#[derive(Debug, Clone)]
pub struct RecoveryMandate {
    started_at: u64,
    osds: BTreeSet<i64>,
}

impl RecoveryMandate {
    /// `osds` are the OSDs recorded lost when the owner started the recovery.
    /// Only a running, persisted recovery may call this — see
    /// `storage_heal::continue_recovery`.
    pub fn from_persisted_recovery(started_at: u64, osds: BTreeSet<i64>) -> Self {
        Self { started_at, osds }
    }
}

/// Purges one OSD named in the mandate — and refuses if it is up. A lost disk
/// that came back is not lost, whatever the record says.
pub async fn purge_lost<H: Host>(
    host: &H,
    mandate: &RecoveryMandate,
    osd: i64,
) -> Result<(), CmdError> {
    if !mandate.osds.contains(&osd) {
        return Err(CmdError::Forbidden {
            cmd: format!("ceph osd purge osd.{osd} (not in the recovery's lost set)"),
        });
    }
    let dump = host.osd_dump().await?;
    if dump.up().contains(&osd) {
        return Err(CmdError::Forbidden {
            cmd: format!("ceph osd purge osd.{osd} (it is up)"),
        });
    }
    purge(host, osd).await.map(|_| ())
}

/// Fails and removes the app filesystem and deletes its two pools. The
/// `mon_allow_pool_delete` switch is turned on for exactly this and off again
/// whatever happened in between — pool deletion must stay impossible everywhere
/// else, or every app's data is within reach of any bug.
pub async fn delete_app_filesystem<H: Host>(
    host: &H,
    mandate: &RecoveryMandate,
    fs_exists: bool,
    existing_pools: &[String],
) -> Result<(), CmdError> {
    let door = Door(());
    tracing::warn!(
        "recovery started at {}: deleting the app filesystem",
        mandate.started_at
    );
    if fs_exists {
        host.ceph_destructive(&door, &["fs", "fail", RECOVERABLE_FS])
            .await?;
        host.ceph_destructive(
            &door,
            &["fs", "rm", RECOVERABLE_FS, "--yes-i-really-mean-it"],
        )
        .await?;
    }
    let doomed: Vec<&str> = RECOVERABLE_FS_POOLS
        .iter()
        .copied()
        .filter(|p| existing_pools.iter().any(|e| e == p))
        .collect();
    if doomed.is_empty() {
        return Ok(());
    }
    host.ceph_destructive(
        &door,
        &["config", "set", "mon", "mon_allow_pool_delete", "true"],
    )
    .await?;
    let mut result = Ok(());
    for pool in doomed {
        if let Err(e) = host
            .ceph_destructive(
                &door,
                &[
                    "osd",
                    "pool",
                    "delete",
                    pool,
                    pool,
                    "--yes-i-really-really-mean-it",
                ],
            )
            .await
        {
            result = Err(e);
            break;
        }
    }
    let off = host
        .ceph(&["config", "set", "mon", "mon_allow_pool_delete", "false"])
        .await;
    result?;
    off.map(|_| ())
}

// ── Proof: a placement group in a pool a machine can refill ─────────────────

/// A placement group that belongs to a disposable pool. Constructed only when
/// the pgid's pool prefix matches that pool's id, so a change to Ceph's filtering
/// can never widen a rebuild onto an app-data pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisposablePg {
    pool: String,
    pgid: String,
}

impl DisposablePg {
    pub fn new(dump: &OsdDump, pool: &str, pgid: &str) -> Option<Self> {
        if !DISPOSABLE_POOLS.contains(&pool) {
            return None;
        }
        let id = dump.pool_id(pool)?;
        pgid.starts_with(&format!("{id}.")).then(|| Self {
            pool: pool.to_string(),
            pgid: pgid.to_string(),
        })
    }

    pub fn pgid(&self) -> &str {
        &self.pgid
    }

    pub fn pool(&self) -> &str {
        &self.pool
    }
}

pub async fn force_create_pg<H: Host>(host: &H, pg: &DisposablePg) -> Result<(), CmdError> {
    let door = Door(());
    host.ceph_destructive(
        &door,
        &["osd", "force-create-pg", &pg.pgid, "--yes-i-really-mean-it"],
    )
    .await
    .map(|_| ())
}

// ── Proof: this disk may be zapped ───────────────────────────────────────────

/// Why a disk may be returned to blank.
#[derive(Debug)]
pub enum ZapWarrant {
    /// Our own OSD on it was just purged, confirmed gone.
    AfterPurge(Purged),
    /// The owner switched it ON and it carries an OSD from ANOTHER cluster —
    /// read from its LVM tags against our fsid, which was known.
    ForeignCluster { osd: i64 },
    /// The owner switched it ON, no OSD of ours is on it, and `ceph-volume lvm
    /// create` refused it for a stale signature.
    StaleSignature,
}

impl ZapWarrant {
    /// Only issued when the create error really is the stale-signature refusal.
    pub fn stale_signature(create_error: &CmdError) -> Option<Self> {
        let e = create_error.to_string().to_ascii_lowercase();
        (e.contains("bluestore signature") || e.contains("has a filesystem signature"))
            .then_some(ZapWarrant::StaleSignature)
    }
}

/// `--destroy` removes the volume group, which is right for a whole disk
/// ceph-volume built its own LVM stack on and wrong for the system LV disko owns
/// (the OS depends on the volume group around it). Device-mapper paths are
/// therefore never `--destroy`ed, whatever the warrant.
pub async fn zap<H: Host>(host: &H, dev_path: &str, warrant: ZapWarrant) -> Result<(), CmdError> {
    let door = Door(());
    let is_lv = dev_path.starts_with("/dev/mapper/") || dev_path.starts_with("/dev/dm-");
    let destroy = !is_lv && !matches!(warrant, ZapWarrant::StaleSignature);
    let mut args = vec!["lvm", "zap"];
    if destroy {
        args.push("--destroy");
    }
    args.push(dev_path);
    let why = match &warrant {
        ZapWarrant::AfterPurge(purged) => format!("after purging osd.{}", purged.osd),
        ZapWarrant::ForeignCluster { osd } => format!("foreign cluster's osd.{osd}"),
        ZapWarrant::StaleSignature => "stale signature".to_string(),
    };
    tracing::warn!("zapping {dev_path}: {why}");
    host.ceph_volume_destructive(&door, &args).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn destructive_commands_are_recognised() {
        for args in [
            vec!["osd", "purge", "osd.3", "--yes-i-really-mean-it"],
            vec!["osd", "destroy", "3"],
            vec!["osd", "lost", "3"],
            vec!["osd", "rm", "3"],
            vec!["osd", "force-create-pg", "2.1", "--yes-i-really-mean-it"],
            vec![
                "osd",
                "pool",
                "delete",
                "p",
                "p",
                "--yes-i-really-really-mean-it",
            ],
            vec!["fs", "rm", "yolab-fs", "--yes-i-really-mean-it"],
            vec!["fs", "fail", "yolab-fs"],
            vec!["mon", "remove", "n2"],
            vec!["auth", "del", "osd.3"],
            vec!["pg", "2.1", "mark_unfound_lost", "revert"],
            vec!["config", "set", "mon", "mon_allow_pool_delete", "true"],
            vec!["--connect-timeout", "10", "osd", "purge", "osd.3"],
        ] {
            assert!(is_destructive("ceph", &args), "{args:?}");
        }
        assert!(is_destructive("ceph-volume", &["lvm", "zap", "/dev/sdb"]));
        assert!(is_destructive(
            "ceph-volume",
            &["lvm", "zap", "--destroy", "/dev/sdb"]
        ));
    }

    #[test]
    fn ordinary_commands_are_not() {
        for args in [
            vec!["-s"],
            vec!["osd", "dump", "-f", "json"],
            vec!["osd", "out", "osd.3"],
            vec!["osd", "in", "osd.3"],
            vec!["osd", "safe-to-destroy", "osd.3", "-f", "json"],
            vec!["osd", "pool", "create", "images", "32", "32"],
            vec!["osd", "pool", "ls"],
            vec!["fs", "ls", "-f", "json"],
            vec!["config", "set", "mon", "mon_allow_pool_delete", "false"],
            vec!["--connect-timeout", "10", "mon", "dump", "-f", "json"],
            vec!["mon", "add", "n2", "[v2:[::1]:3300]"],
        ] {
            assert!(!is_destructive("ceph", &args), "{args:?}");
        }
        assert!(!is_destructive(
            "ceph-volume",
            &["lvm", "list", "--format", "json"]
        ));
        assert!(!is_destructive(
            "ceph-volume",
            &["lvm", "create", "--data", "/dev/sdb"]
        ));
        assert!(!is_destructive("kubectl", &["delete", "namespace", "x"]));
    }

    #[tokio::test]
    async fn busy_means_not_yet_and_an_unanswered_check_is_an_error() {
        let busy = FakeHost::new().fail(
            "ceph osd safe-to-destroy osd.3",
            "Error EBUSY: OSD(s) 3 have 25 pgs currently mapped to them.",
        );
        assert!(safe_to_destroy(&busy, 3).await.unwrap().is_none());

        let silent = FakeHost::new().fail("ceph osd safe-to-destroy osd.3", "timed out");
        assert!(safe_to_destroy(&silent, 3).await.is_err());

        let other_osd = FakeHost::new().ok(
            "ceph osd safe-to-destroy osd.3",
            r#"{"safe_to_destroy":[4]}"#,
        );
        assert!(safe_to_destroy(&other_osd, 3).await.unwrap().is_none());

        let garbage = FakeHost::new().ok("ceph osd safe-to-destroy osd.3", "{}");
        assert!(safe_to_destroy(&garbage, 3).await.is_err());
    }

    #[tokio::test]
    async fn a_confirmed_purge_yields_a_receipt_only_when_the_osd_is_gone() {
        let gone = FakeHost::new()
            .ok(
                "ceph osd safe-to-destroy osd.3",
                r#"{"safe_to_destroy":[3]}"#,
            )
            .ok("ceph osd purge osd.3", "")
            .ok("ceph osd ls", "[1,2]");
        let proof = safe_to_destroy(&gone, 3).await.unwrap().unwrap();
        let receipt = purge_safe(&gone, proof).await.unwrap();
        assert_eq!(receipt.map(|p| p.osd), Some(3));

        let lingering = FakeHost::new()
            .ok(
                "ceph osd safe-to-destroy osd.3",
                r#"{"safe_to_destroy":[3]}"#,
            )
            .ok("ceph osd purge osd.3", "")
            .ok("ceph osd ls", "[1,2,3]");
        let proof = safe_to_destroy(&lingering, 3).await.unwrap().unwrap();
        assert!(purge_safe(&lingering, proof).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_recovery_never_purges_an_osd_that_is_up_or_was_not_recorded_lost() {
        let mandate = RecoveryMandate::from_persisted_recovery(1, BTreeSet::from([3]));
        let up = FakeHost::new().ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":3,"up":1,"in":0}],"pools":[]}"#,
        );
        assert!(purge_lost(&up, &mandate, 3).await.is_err());
        assert!(!up.ran("osd purge"));

        let down = FakeHost::new().ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":4,"up":0,"in":0}],"pools":[]}"#,
        );
        assert!(purge_lost(&down, &mandate, 4).await.is_err());
        assert!(!down.ran("osd purge"));
    }

    #[test]
    fn only_disposable_pools_with_matching_ids_yield_a_rebuildable_pg() {
        let dump: OsdDump = serde_json::from_str(
            r#"{"osds":[],"pools":[{"pool":2,"pool_name":"images"},{"pool":3,"pool_name":"yolab-fs-data0"}]}"#,
        )
        .unwrap();
        assert!(DisposablePg::new(&dump, "images", "2.1f").is_some());
        assert!(DisposablePg::new(&dump, "images", "3.1f").is_none());
        assert!(DisposablePg::new(&dump, "yolab-fs-data0", "3.1").is_none());
        assert!(DisposablePg::new(&dump, "missing", "9.1").is_none());
    }

    #[tokio::test]
    async fn the_system_lv_is_never_destroyed_whatever_the_warrant() {
        let host = FakeHost::new().ok("ceph-volume lvm zap", "");
        zap(
            &host,
            "/dev/mapper/pool-ceph",
            ZapWarrant::ForeignCluster { osd: 1 },
        )
        .await
        .unwrap();
        assert!(host.ran("ceph-volume lvm zap /dev/mapper/pool-ceph"));
        assert!(!host.ran("--destroy"));
    }

    #[test]
    fn a_stale_signature_warrant_needs_the_real_refusal() {
        let refusal = CmdError::failed(
            "ceph-volume lvm create",
            "RuntimeError: device /dev/sdb has a filesystem signature",
        );
        assert!(ZapWarrant::stale_signature(&refusal).is_some());
        let other = CmdError::failed("ceph-volume lvm create", "No space left on device");
        assert!(ZapWarrant::stale_signature(&other).is_none());
    }

    #[tokio::test]
    async fn the_plain_fake_entry_point_refuses_destruction_too() {
        let host = FakeHost::new().ok("ceph osd purge", "");
        let err = host
            .ceph(&["osd", "purge", "osd.1", "--yes-i-really-mean-it"])
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Forbidden { .. }));
    }
}
