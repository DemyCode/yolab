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
//!     `HealMandate` (the owner pressed FORCE HEAL), a `ZapWarrant`, or a
//!     `Purged` receipt — and those proofs can only be obtained by performing the
//!     check they stand for.
//!
//! A future bug can still pass the wrong proof. It can no longer forget to
//! have one.

use std::collections::BTreeSet;

use crate::ceph::model::SafeToDestroyReport;
use crate::exec::{CmdError, Failure};
use crate::host::Host;

/// Permission to run a destructive command. The private field is the point:
/// nothing outside this module can build one.
pub struct Door(());

/// The app filesystem, which a heal fails before deleting its pools.
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

// ── Proof: the owner pressed FORCE HEAL ──────────────────────────────────────

/// Issued from a persisted heal record that a person started with FORCE HEAL,
/// after confirming by name every machine that stopped answering. A heal
/// rebuilds the cluster without what did not answer, so its mandate allows
/// exactly that: forgetting those machines, purging disks that are down,
/// deleting every pool, and resetting k3s to this machine — nothing aimed at a
/// machine or a disk that still answers.
#[derive(Debug, Clone)]
pub struct HealMandate {
    id: String,
    dead_machines: BTreeSet<String>,
}

impl HealMandate {
    /// Only a running, persisted heal may call this — see `heal::Heal::mandate`.
    pub fn from_persisted_heal(id: &str, dead_machines: BTreeSet<String>) -> Self {
        Self {
            id: id.to_string(),
            dead_machines,
        }
    }

    fn refuse_live(&self, machine: &str, what: &str) -> Result<(), CmdError> {
        if self.dead_machines.contains(machine) {
            return Ok(());
        }
        Err(CmdError::Forbidden {
            cmd: format!("{what} ({machine} was not confirmed gone)"),
        })
    }
}

/// Purges an OSD that is down — and refuses one that is up. A disk that
/// answers is not unresponsive, whatever was decided earlier.
pub async fn purge_down<H: Host>(host: &H, mandate: &HealMandate, osd: i64) -> Result<(), CmdError> {
    let dump = host.osd_dump().await?;
    if dump.up().contains(&osd) {
        return Err(CmdError::Forbidden {
            cmd: format!("ceph osd purge osd.{osd} (it is up)"),
        });
    }
    tracing::warn!("heal {}: purging osd.{osd}", mandate.id);
    purge(host, osd).await.map(|_| ())
}

/// Removes a confirmed-gone machine's mon from a monmap that still has quorum.
pub async fn remove_mon<H: Host>(host: &H, mandate: &HealMandate, machine: &str) -> Result<(), CmdError> {
    mandate.refuse_live(machine, &format!("ceph mon remove {machine}"))?;
    let door = Door(());
    host.ceph_destructive(&door, &["mon", "remove", machine])
        .await
        .map(|_| ())
}

/// Removes confirmed-gone machines from THIS machine's monmap while there is no
/// quorum to ask — the only way back to a quorum once a majority of mons is
/// gone. The local mon is stopped, its map edited in its own store, and started
/// again; the result is a monmap in which the remaining mons are a majority.
///
/// Every step is attempted in order and the mon is started again whatever
/// happened, so a failure leaves the machine as it was rather than monless.
pub async fn remove_mons_offline<H: Host>(
    host: &H,
    mandate: &HealMandate,
    me: &str,
    gone: &[String],
    monmap_path: &str,
) -> Result<(), CmdError> {
    if gone.is_empty() {
        return Ok(());
    }
    for machine in gone {
        mandate.refuse_live(machine, &format!("monmaptool --rm {machine}"))?;
    }
    if gone.iter().any(|g| g == me) {
        return Err(CmdError::Forbidden {
            cmd: format!("monmaptool --rm {me} (this machine's own mon)"),
        });
    }
    tracing::warn!("heal {}: removing {gone:?} from {me}'s monmap offline", mandate.id);
    let unit = format!("ceph-mon-{me}.service");
    checked(host.systemctl(&["stop", &unit]).await, "systemctl stop")?;
    let edited: Result<(), CmdError> = async {
        checked(
            host.run_cmd("ceph-mon", &["-i", me, "--extract-monmap", monmap_path]).await,
            "ceph-mon --extract-monmap",
        )?;
        for machine in gone {
            checked(
                host.run_cmd("monmaptool", &[monmap_path, "--rm", machine]).await,
                "monmaptool --rm",
            )?;
        }
        checked(
            host.run_cmd(
                "ceph-mon",
                &[
                    "-i",
                    me,
                    "--inject-monmap",
                    monmap_path,
                    "--setuser",
                    "ceph",
                    "--setgroup",
                    "ceph",
                ],
            )
            .await,
            "ceph-mon --inject-monmap",
        )
    }
    .await;
    let started = checked(host.systemctl(&["start", &unit]).await, "systemctl start");
    edited?;
    started
}

fn checked(out: Result<crate::host::CommandOutput, CmdError>, what: &str) -> Result<(), CmdError> {
    let out = out?;
    if out.success {
        return Ok(());
    }
    Err(CmdError::failed(what, out.stderr.trim()))
}

/// Deletes the cephx keys of a confirmed-gone machine's mgr and MDS, so the
/// machine cannot come back as those daemons without being set up again.
pub async fn forget_daemons<H: Host>(host: &H, mandate: &HealMandate, machine: &str) -> Result<(), CmdError> {
    mandate.refuse_live(machine, &format!("ceph auth del mgr.{machine}"))?;
    let door = Door(());
    for entity in [format!("mgr.{machine}"), format!("mds.{machine}")] {
        match host.ceph_destructive(&door, &["auth", "del", &entity]).await {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Resets this machine's embedded etcd to a single member — itself — keeping its
/// data, so k3s can run again after the other members are gone for good.
/// `k3s` must not be running; the caller stops it first.
pub async fn reset_kubernetes_membership<H: Host>(host: &H, mandate: &HealMandate) -> Result<(), CmdError> {
    tracing::warn!("heal {}: resetting k3s to a single member", mandate.id);
    checked(
        host.run_cmd_bounded(
            "k3s",
            &["server", "--cluster-reset"],
            std::time::Duration::from_secs(600),
        )
        .await,
        "k3s server --cluster-reset",
    )
}

/// Fails and removes the app filesystem, then deletes EVERY pool. A heal leaves
/// storage as a fresh installation has it: the mgr recreates `.mgr`, each
/// machine's boot creates the image store, and the filesystem controller the app
/// filesystem.
///
/// The `mon_allow_pool_delete` switch is turned on for exactly this and off
/// again whatever happened in between — pool deletion must stay impossible
/// everywhere else, or every app's data is within reach of any bug.
pub async fn delete_all_storage<H: Host>(
    host: &H,
    mandate: &HealMandate,
    fs_exists: bool,
    existing_pools: &[String],
) -> Result<(), CmdError> {
    let door = Door(());
    tracing::warn!("heal {}: deleting the app filesystem and every pool", mandate.id);
    if fs_exists {
        host.ceph_destructive(&door, &["fs", "fail", RECOVERABLE_FS])
            .await?;
        host.ceph_destructive(
            &door,
            &["fs", "rm", RECOVERABLE_FS, "--yes-i-really-mean-it"],
        )
        .await?;
    }
    if existing_pools.is_empty() {
        return Ok(());
    }
    host.ceph_destructive(
        &door,
        &["config", "set", "mon", "mon_allow_pool_delete", "true"],
    )
    .await?;
    let mut result = Ok(());
    for pool in existing_pools {
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

    #[tokio::test]
    async fn a_destructive_command_outside_the_door_is_refused_not_run() {
        let host = FakeHost::new()
            .ok("ceph osd purge", "")
            .ok("ceph-volume lvm zap", "");
        let purge = host
            .ceph(&["osd", "purge", "osd.3", "--yes-i-really-mean-it"])
            .await;
        assert!(matches!(purge, Err(CmdError::Forbidden { .. })));
        assert!(host.refused("osd purge osd.3"));
        assert!(!host.ran("osd purge"), "a refusal is not a run");
    }

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

    fn mandate() -> HealMandate {
        HealMandate::from_persisted_heal("h1", BTreeSet::from(["node2".to_string()]))
    }

    #[tokio::test]
    async fn a_heal_never_purges_an_osd_that_is_up() {
        let up = FakeHost::new().ok(
            "ceph osd dump",
            r#"{"osds":[{"osd":3,"up":1,"in":0}],"pools":[]}"#,
        );
        assert!(purge_down(&up, &mandate(), 3).await.is_err());
        assert!(!up.ran("osd purge"));

        let down = FakeHost::new()
            .ok("ceph osd dump", r#"{"osds":[{"osd":3,"up":0,"in":1}],"pools":[]}"#)
            .ok("ceph osd purge", "")
            .ok("ceph osd ls", "[]");
        purge_down(&down, &mandate(), 3).await.unwrap();
        assert!(down.ran("ceph osd purge osd.3 --yes-i-really-mean-it"));
    }

    #[tokio::test]
    async fn only_a_confirmed_gone_machine_loses_its_mon_and_keys() {
        let host = FakeHost::new().ok("ceph mon remove", "").ok("ceph auth del", "");
        assert!(remove_mon(&host, &mandate(), "node1").await.is_err());
        assert!(forget_daemons(&host, &mandate(), "node1").await.is_err());
        assert!(host.calls().is_empty(), "{:?}", host.calls());

        remove_mon(&host, &mandate(), "node2").await.unwrap();
        forget_daemons(&host, &mandate(), "node2").await.unwrap();
        assert!(host.ran("ceph mon remove node2"));
        assert!(host.ran("ceph auth del mgr.node2") && host.ran("ceph auth del mds.node2"));
    }

    #[tokio::test]
    async fn a_key_that_is_already_gone_is_not_an_error() {
        let host = FakeHost::new()
            .fail("ceph auth del mgr.node2", "Error ENOENT: failed to find mgr.node2 in keyring")
            .ok("ceph auth del mds.node2", "");
        forget_daemons(&host, &mandate(), "node2").await.unwrap();
    }

    fn offline_host() -> FakeHost {
        FakeHost::new()
            .ok("systemctl stop ceph-mon-node1.service", "")
            .ok("systemctl start ceph-mon-node1.service", "")
            .ok("ceph-mon -i node1", "")
            .ok("monmaptool", "")
    }

    #[tokio::test]
    async fn the_monmap_is_edited_with_the_mon_stopped_and_started_again() {
        let host = offline_host();
        let gone = vec!["node2".to_string()];
        remove_mons_offline(&host, &mandate(), "node1", &gone, "/var/lib/yolab/monmap")
            .await
            .unwrap();
        let order = [
            "systemctl stop ceph-mon-node1.service",
            "ceph-mon -i node1 --extract-monmap /var/lib/yolab/monmap",
            "monmaptool /var/lib/yolab/monmap --rm node2",
            "ceph-mon -i node1 --inject-monmap /var/lib/yolab/monmap --setuser ceph --setgroup ceph",
            "systemctl start ceph-mon-node1.service",
        ];
        for w in order.windows(2) {
            let (a, b) = (host.position(w[0]), host.position(w[1]));
            assert!(a.is_some() && a < b, "{} before {}: {:?}", w[0], w[1], host.calls());
        }
    }

    #[tokio::test]
    async fn a_failed_edit_still_starts_the_mon_again() {
        let host = offline_host().fail("monmaptool", "no such mon");
        let gone = vec!["node2".to_string()];
        assert!(remove_mons_offline(&host, &mandate(), "node1", &gone, "/m").await.is_err());
        assert!(host.ran("systemctl start ceph-mon-node1.service"));
        assert!(!host.ran("--inject-monmap"));
    }

    #[tokio::test]
    async fn the_offline_edit_refuses_live_machines_and_this_one() {
        let not_confirmed = vec!["node3".to_string()];
        let host = offline_host();
        assert!(remove_mons_offline(&host, &mandate(), "node1", &not_confirmed, "/m")
            .await
            .is_err());
        assert!(host.calls().is_empty(), "{:?}", host.calls());

        let both = HealMandate::from_persisted_heal(
            "h1",
            BTreeSet::from(["node1".to_string(), "node2".to_string()]),
        );
        let itself = vec!["node2".to_string(), "node1".to_string()];
        let host = offline_host();
        assert!(remove_mons_offline(&host, &both, "node1", &itself, "/m")
            .await
            .is_err());
        assert!(host.calls().is_empty(), "{:?}", host.calls());

        let host = FakeHost::new();
        remove_mons_offline(&host, &mandate(), "node1", &[], "/m").await.unwrap();
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn the_kubernetes_reset_reports_failure() {
        let ok = FakeHost::new().ok("k3s server --cluster-reset", "");
        reset_kubernetes_membership(&ok, &mandate()).await.unwrap();
        let bad = FakeHost::new().fail("k3s server --cluster-reset", "etcd data dir missing");
        assert!(reset_kubernetes_membership(&bad, &mandate()).await.is_err());
    }

    #[tokio::test]
    async fn every_pool_is_deleted_and_pool_deletion_is_switched_back_off() {
        let host = FakeHost::new()
            .ok("ceph fs fail", "")
            .ok("ceph fs rm", "")
            .ok("ceph config set mon mon_allow_pool_delete", "")
            .ok("ceph osd pool delete", "");
        let pools = vec![".mgr".to_string(), "images".to_string(), "yolab-fs-data0".to_string()];
        delete_all_storage(&host, &mandate(), true, &pools).await.unwrap();
        let pos = |n: &str| host.position(n).unwrap_or_else(|| panic!("{n}"));
        assert!(pos("ceph fs fail yolab-fs") < pos("ceph fs rm yolab-fs"));
        assert!(pos("mon_allow_pool_delete true") < pos("pool delete .mgr .mgr"));
        assert!(pos("pool delete yolab-fs-data0") < pos("mon_allow_pool_delete false"));
        assert!(host.ran("pool delete images images --yes-i-really-really-mean-it"));

        let failing = FakeHost::new()
            .ok("ceph config set mon mon_allow_pool_delete", "")
            .fail("ceph osd pool delete .mgr", "EBUSY");
        assert!(delete_all_storage(&failing, &mandate(), false, &pools).await.is_err());
        assert!(failing.ran("mon_allow_pool_delete false"));
        assert!(!failing.ran("pool delete images") && !failing.ran("fs fail"));

        let nothing = FakeHost::new();
        delete_all_storage(&nothing, &mandate(), false, &[]).await.unwrap();
        assert!(nothing.calls().is_empty());
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
