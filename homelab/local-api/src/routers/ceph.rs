use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::kubectl;

#[derive(Serialize)]
pub struct CephStatus {
    pub available: bool,
    pub health: String,
    pub osd_count: u32,
    pub osd_up: u32,
    pub total_bytes: u64,
    pub used_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Human-readable cluster health ─────────────────────────────────────────────

#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum HealthLevel {
    Ok,
    Warn,
    Error,
}

#[derive(Serialize)]
pub struct HealthIssue {
    pub level: HealthLevel,
    pub title: String,
    pub description: String,
}

#[derive(Serialize)]
pub struct ClusterHealth {
    pub level: HealthLevel,
    pub title: String,
    pub message: String,
    pub issues: Vec<HealthIssue>,
    /// Some PGs have zero accessible copies — reads/writes to affected PVCs block.
    pub pg_unavailable: bool,
    /// Ceph API (and likely MON quorum) is reachable.
    pub mon_quorum_ok: bool,
    /// A disk or pool is full — new writes are blocked, recovery may be stalled.
    pub osd_full: bool,
    /// Storage is still warming up after a node restart — not an error, just needs time.
    pub starting: bool,
    /// A new disk is being prepared as an OSD — storage will grow once ready.
    pub provisioning: bool,
    /// Data is unreadable AND cannot be rebuilt (the pools holding it keep one copy
    /// and the disk is gone). The Backups page keys the one-click recovery off this
    /// rather than matching on a message, so the wording can change without silently
    /// turning the recovery path off.
    pub storage_unrecoverable: bool,
}

fn system_uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(u64::MAX)
}

/// True while a disk is actively being turned into an OSD.
///
/// Rook signalled this with `rook-ceph-osd-prepare` Pods. There are none now:
/// the reconciler runs `ceph-volume` in-process. A newly created OSD is briefly
/// `in` but not yet `up`, so a gap between the two counts is the equivalent
/// signal — it keeps the UI showing "provisioning" instead of "degraded" while
/// a disk is being added.
/// Whether a disk is in the middle of being ADDED.
///
/// `in > up` alone does not mean that, and reading it that way is how a dead disk got
/// announced as routine setup. An OSD that is `in` but not `up` is the definition of a
/// disk that has stopped answering — a new disk coming up and an old disk dying look
/// identical from this counter, and only one of them is good news.
///
/// Observed live: a disk was pulled from a size=1 cluster, 63 of 81 placement groups
/// went stale, and the home page said "Preparing a new disk — you can keep using
/// everything while this finishes", because provisioning is checked before severity.
///
/// So the counter is still the signal, but it only means provisioning when nothing is
/// actually unreachable. If data cannot be read, whatever else is true, this is not
/// setup.
async fn osd_provisioning_active(data_unavailable: bool) -> bool {
    if data_unavailable {
        return false;
    }
    crate::ceph_cli::ceph_json(&["osd", "stat"])
        .await
        .ok()
        .map(|v| {
            let up = v["num_up_osds"].as_u64().unwrap_or(0);
            let in_ = v["num_in_osds"].as_u64().unwrap_or(0);
            in_ > up
        })
        .unwrap_or(false)
}

pub async fn cluster_health() -> Json<ClusterHealth> {
    Json(compute_cluster_health().await)
}

async fn compute_cluster_health() -> ClusterHealth {
    // Deliberately not computed yet: it depends on whether anything is unreachable,
    // which is only known once the health details are parsed below.
    // Straight from the mon: Ceph runs as host daemons, so there is no Rook CR
    // status to read. Formatted as "<HEALTH_X>\n<details-json>" to keep the
    // existing parser below unchanged.
    let raw = match ceph_health_and_details().await {
        Ok(s) => s,
        Err(_) => {
            let starting = system_uptime_secs() < 900;
            return ClusterHealth {
                level: if starting {
                    HealthLevel::Warn
                } else {
                    HealthLevel::Error
                },
                title: if starting {
                    "Storage is warming up".into()
                } else {
                    "Storage cluster unreachable".into()
                },
                message: if starting {
                    "Your storage is starting after a restart. Apps will be available in a few minutes.".into()
                } else {
                    "Cannot connect to the storage control plane. Check that Rook is running."
                        .into()
                },
                issues: vec![],
                pg_unavailable: false,
                mon_quorum_ok: false,
                osd_full: false,
                starting,
                // Not known from here, and "preparing a disk" is the wrong guess when
                // the control plane cannot be reached at all.
                provisioning: false,
                storage_unrecoverable: false,
            };
        }
    };

    let mut lines = raw.lines();
    let health_str = lines.next().unwrap_or("").trim();
    let details_str = lines.next().unwrap_or("{}");
    let details: serde_json::Value = serde_json::from_str(details_str).unwrap_or_default();

    let mut issues: Vec<HealthIssue> = vec![];

    // How many distinct places a copy could go, so the no-redundancy message can
    // tell this owner what to actually do rather than guess. Unreadable topology
    // yields 0, which takes the conservative branch (recommend backups) instead of
    // promising disks that may not exist.
    let places = match crate::topology::observe().await {
        Some(t) if t.osd_hosts > 1 => t.osd_hosts,
        Some(t) => t.osds,
        None => 0,
    };

    // Only when Ceph says something is unreadable — two extra queries, and this
    // endpoint is polled.
    let loss = if details
        .as_object()
        .is_some_and(|o| o.contains_key("PG_AVAILABILITY") || o.contains_key("PG_DOWN"))
    {
        assess_pg_loss().await
    } else {
        None
    };

    if let Some(obj) = details.as_object() {
        for (code, detail) in obj {
            if let Some(issue) = translate_health_check(code, detail, places, loss.as_ref()) {
                issues.push(issue);
            }
        }
    }

    // Sort: errors first, then warns
    issues.sort_by_key(|i| {
        if i.level == HealthLevel::Error {
            0u8
        } else {
            1
        }
    });

    let level = if health_str == "HEALTH_OK" {
        HealthLevel::Ok
    } else if health_str == "HEALTH_ERR" || issues.iter().any(|i| i.level == HealthLevel::Error) {
        HealthLevel::Error
    } else {
        HealthLevel::Warn
    };

    // Derive machine-readable flags from the active issue codes.
    let pg_unavailable = details
        .as_object()
        .is_some_and(|obj| obj.contains_key("PG_AVAILABILITY") || obj.contains_key("PG_DOWN"));
    let osd_full = details.as_object().is_some_and(|obj| {
        obj.contains_key("OSD_FULL") || obj.contains_key("NOSPC") || obj.contains_key("POOL_FULL")
    });
    // "Starting" when reachable but PGs are still peering/recovering and system just booted.
    let starting = pg_unavailable && system_uptime_secs() < 900;
    // Only now: a disk being added and a disk having died produce the same counters,
    // and unreachable data is what tells them apart.
    let provisioning = osd_provisioning_active(pg_unavailable).await;
    let storage_unrecoverable = loss
        .as_ref()
        .is_some_and(|l| l.unrecoverable && l.stuck > 0);

    match level {
        HealthLevel::Ok => ClusterHealth {
            level: HealthLevel::Ok,
            title: "All systems healthy".into(),
            message: "Your storage cluster is running normally.".into(),
            issues,
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
            starting: false,
            provisioning,
            storage_unrecoverable,
        },
        HealthLevel::Warn => ClusterHealth {
            level: HealthLevel::Warn,
            title: if starting {
                "Storage is warming up".into()
            } else {
                "Storage has warnings".into()
            },
            message: if starting {
                "Your storage is recovering after a restart. Apps will be available shortly.".into()
            } else {
                "Your cluster is operational but has non-critical issues.".into()
            },
            issues,
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
            starting,
            provisioning,
            storage_unrecoverable,
        },
        HealthLevel::Error => ClusterHealth {
            level: if starting {
                HealthLevel::Warn
            } else {
                HealthLevel::Error
            },
            title: if starting {
                "Storage is warming up".into()
            } else {
                "Storage cluster has critical errors".into()
            },
            message: if starting {
                "Your storage is recovering after a restart. Apps will be available shortly.".into()
            } else {
                "One or more critical problems affect your storage. Apps may be unable to read or write data.".into()
            },
            issues: if starting { vec![] } else { issues },
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
            starting,
            provisioning,
            storage_unrecoverable,
        },
    }
}

/// Whether unreachable data can come back on its own, and how much of it there is.
///
/// This distinction is the whole point. Ceph reports both cases the same way — a
/// PG_AVAILABILITY warning — because from its side they look identical: some
/// placement groups cannot be read right now. But:
///
///   size >= 2, a disk down   the other copies are fine, Ceph is already rebuilding,
///                            and the honest advice is to wait.
///   size == 1, a disk down   there is no other copy. Nothing is rebuilding, nothing
///                            will, and waiting accomplishes nothing at all.
///
/// The old message assumed the first case for both. Observed live during a deliberate
/// disk pull at size=1: 63 of 81 placement groups went stale — 78% of the file data
/// and 81% of the filesystem metadata — and the page said "Some data TEMPORARILY
/// unavailable … apps will hang UNTIL RECOVERY". Red, and reassuring, during permanent
/// loss. That is worse than saying nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PgLoss {
    /// PGs that cannot be read right now.
    pub stuck: u32,
    pub total: u32,
    /// True when at least one affected PG belongs to a pool keeping a single copy, so
    /// no amount of waiting brings it back.
    pub unrecoverable: bool,
}

/// PG states that mean "the OSD holding this is not answering", as opposed to
/// `degraded`/`undersized`/`peering`, which mean "a copy is missing but another is
/// serving and recovery is under way".
fn is_stuck_state(state: &str) -> bool {
    state
        .split('+')
        .any(|s| matches!(s, "stale" | "down" | "incomplete" | "unknown"))
}

/// Reads pool replica counts and PG placement to decide which case this is.
///
/// Only called when Ceph has actually raised PG_AVAILABILITY or PG_DOWN — it is two
/// extra queries, and the endpoint they sit in is polled.
///
/// Per pool, not cluster-wide: a pool may legitimately sit at one copy — a
/// fresh cluster with a single OSD does — and reading that as "any pool keeps
/// one copy" would mark every transient blip unrecoverable.
pub(crate) async fn assess_pg_loss() -> Option<PgLoss> {
    let dump = crate::ceph_cli::ceph_json(&["osd", "dump"]).await.ok()?;
    let sizes: std::collections::HashMap<i64, u64> = dump["pools"]
        .as_array()?
        .iter()
        .filter_map(|p| Some((p["pool"].as_i64()?, p["size"].as_u64()?)))
        .collect();

    let pgs = crate::ceph_cli::ceph_json(&["pg", "dump", "pgs_brief"])
        .await
        .ok()?;
    // `ceph pg dump pgs_brief -f json` returns the array directly on some versions and
    // under `pg_stats` on others.
    let items = pgs["pg_stats"]
        .as_array()
        .or_else(|| pgs.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return None;
    }

    let mut stuck = 0u32;
    let mut unrecoverable = false;
    for pg in &items {
        let state = pg["state"].as_str().unwrap_or("");
        if !is_stuck_state(state) {
            continue;
        }
        stuck += 1;
        let pool_id = pg["pgid"]
            .as_str()
            .and_then(|id| id.split('.').next())
            .and_then(|p| p.parse::<i64>().ok());
        if let Some(size) = pool_id.and_then(|id| sizes.get(&id)) {
            if *size <= 1 {
                unrecoverable = true;
            }
        }
    }

    Some(PgLoss {
        stuck,
        total: items.len() as u32,
        unrecoverable,
    })
}

/// What to say about data that cannot be read.
pub(crate) fn unavailable_message(loss: Option<&PgLoss>) -> (String, String) {
    match loss {
        Some(l) if l.unrecoverable => {
            let share = if l.total > 0 {
                format!("{} of {} groups of your files", l.stuck, l.total)
            } else {
                "Some of your files".to_string()
            };
            (
                "Your files are unreachable and cannot be rebuilt".into(),
                format!(
                    "{share} were stored in one place only, on a disk that is no longer \
                     responding — so there is no second copy to rebuild them from and \
                     nothing is being repaired. If that disk still works, reconnecting it \
                     brings everything back. If it does not, only a backup can. Apps that \
                     use these files will not start or will hang."
                ),
            )
        }
        _ => (
            "Some files are unreachable right now".into(),
            "A disk is not responding. Other copies exist, so this repairs itself — apps \
             touching the affected files may hang until it finishes."
                .into(),
        ),
    }
}

/// Why the no-redundancy message needs to know how many disks there are.
///
/// One string cannot serve both cases, and the old one served neither: it said
/// "this is expected with a single-disk setup" unconditionally, so a cluster with
/// three disks and one copy of everything was told its situation was expected. It
/// was not expected, it was one dialog away from being fixed, and the sentence
/// talked the owner out of fixing it.
///
/// `places` is how many distinct locations a second copy could go — OSDs, or
/// machines when the failure domain is host. Zero or one means the machine
/// genuinely cannot replicate and the honest advice is backups; more than one
/// means it can, today, and the advice is to say so.
fn no_redundancy_message(places: u32) -> String {
    if places <= 1 {
        "Everything is stored once, on the only disk this machine has. If that disk fails, \
         that data is gone — there is no second copy to rebuild from. Turn on backups so a \
         copy lives somewhere else, or add another disk and ask for 2 copies."
            .into()
    } else {
        format!(
            "Everything is stored once, even though this cluster has {places} places to put \
             copies. If a disk fails, whatever lived on it is gone. Raise the number of copies \
             to 2 on this page and YoLab spreads a second copy across them — your files stay \
             available the whole time."
        )
    }
}

fn translate_health_check(
    code: &str,
    detail: &serde_json::Value,
    places: u32,
    loss: Option<&PgLoss>,
) -> Option<HealthIssue> {
    let severity = detail["severity"].as_str().unwrap_or("HEALTH_WARN");
    let level = if severity == "HEALTH_ERR" {
        HealthLevel::Error
    } else {
        HealthLevel::Warn
    };

    let (title, description) = match code {
        "POOL_NO_REDUNDANCY" => (
            "No second copy of your data".into(),
            no_redundancy_message(places),
        ),
        "MDS_ALL_DOWN" => (
            "File system offline".into(),
            "The metadata server (MDS) that manages your file system is down. Apps using file storage are stuck until it recovers.".into(),
        ),
        "MDS_DAMAGE" => (
            "File system damaged".into(),
            "The file system metadata is corrupted. Apps using file storage cannot function. Auto-recovery is in progress.".into(),
        ),
        "MDS_SLOW_METADATA_IO" | "MDS_SLOW_REQUEST" => (
            "File system running slowly".into(),
            "File system operations are taking longer than usual. Apps may be slow.".into(),
        ),
        "OSD_DOWN" => (
            "A storage disk is down".into(),
            "One or more storage disks are offline. Data may be temporarily unavailable if no redundancy exists.".into(),
        ),
        "OSD_NEARFULL" => (
            "A disk is nearly full".into(),
            "One or more disks are over 75% full. Add more storage soon to avoid data loss.".into(),
        ),
        "OSD_FULL" | "NOSPC" => (
            "A disk is full".into(),
            "A disk has run out of space. New writes are blocked and apps may crash. Free space immediately.".into(),
        ),
        "MON_DOWN" => (
            "Control node offline".into(),
            "A monitor node is offline. Storage decisions may be delayed or impossible.".into(),
        ),
        "MON_DISK_LOW" => (
            "Monitor disk is low on space".into(),
            "The disk used by the monitor process is nearly full. Free up space on the system drive to keep the cluster healthy.".into(),
        ),
        "MON_DISK_CRIT" => (
            "Monitor disk critically low".into(),
            "The monitor disk is critically full. Storage decisions are at risk. Free up space on the system drive immediately.".into(),
        ),
        "MON_DISK_BIG" => (
            "Monitor data growing large".into(),
            "Monitor storage is consuming more disk than expected. Consider trimming old snapshots or logs.".into(),
        ),
        "MON_CLOCK_SKEW" => (
            "Machine clocks out of sync".into(),
            "The clocks on your machines differ by too much. This can cause storage failures.".into(),
        ),
        // Ceph raises both of these for the same situation from the owner's
        // point of view: fewer copies exist than were asked for. It is now a
        // NORMAL, indefinite state rather than a transient one — asking for
        // more copies than there are disks is allowed, and means "make the rest
        // when I add some". So this must not read as a fault or claim recovery
        // is under way, and it must say the thing the owner needs to know
        // first: everything still works.
        //
        // That last part is only true because min_size is 1 (see
        // topology::MIN_SIZE). Under the old min_size = size - 1 this same
        // state could mean every app had stopped, and saying "everything works"
        // would have been a lie.
        "PG_DEGRADED" | "PG_UNDERSIZED" => (
            "Fewer copies than you asked for".into(),
            "Some of your files have fewer copies than your storage settings ask for — \
             there are not enough disks to hold them all right now. Everything still \
             works and nothing has been lost. Add a disk and the missing copies are \
             made automatically."
                .into(),
        ),
        "PG_DOWN" | "PG_AVAILABILITY" => {
            // Always critical regardless of Ceph's own severity. Every check Ceph
            // raised during a live size=1 disk pull was HEALTH_WARN — including the
            // one saying 78% of the data had gone — because from its side a down OSD
            // is recoverable-in-principle until proven otherwise.
            let (title, description) = unavailable_message(loss);
            return Some(HealthIssue {
                level: HealthLevel::Error,
                title,
                description,
            });
        }
        "SLOW_OPS" => (
            "Storage operations are slow".into(),
            "Some storage operations are taking longer than expected. Apps may respond slowly.".into(),
        ),
        "OBJECT_UNFOUND" => (
            "Missing data objects".into(),
            "Some data objects cannot be found on any disk. This is a sign of past data loss.".into(),
        ),
        "PG_PEERING" | "PG_NOT_SCRUBBED" | "PG_NOT_DEEP_SCRUBBED" | "PG_NOT_SCRUBBED_SINCE" => {
            // Normal transient states after startup — suppress them to avoid alarm.
            return None;
        }
        "RECENT_CRASH" => (
            "A storage process recently crashed".into(),
            "One of your storage daemons crashed and restarted. It may be a sign of a hardware issue if this happens repeatedly.".into(),
        ),
        "POOL_TOTAL_SIZE_MIN_SIZE_REACHED" => (
            "No second copy of your data".into(),
            no_redundancy_message(places),
        ),
        _ => {
            // Unknown code: surface it but don't translate
            let summary = detail["summary"]["message"].as_str().unwrap_or(code).to_string();
            (format!("Storage issue: {}", summary.split(':').next().unwrap_or(code)), summary)
        }
    };

    Some(HealthIssue {
        level,
        title,
        description,
    })
}

pub async fn ceph_status() -> Json<CephStatus> {
    match cluster_status_from_k8s().await {
        Ok((status, osd_total, osd_ready)) => {
            let cap = status
                .get("ceph")
                .and_then(|c| c.get("capacity"))
                .cloned()
                .unwrap_or_default();
            Json(CephStatus {
                available: status.get("phase").and_then(|p| p.as_str()) == Some("Ready"),
                health: status
                    .get("ceph")
                    .and_then(|c| c.get("health"))
                    .and_then(|h| h.as_str())
                    .unwrap_or("HEALTH_UNKNOWN")
                    .to_string(),
                osd_count: osd_total,
                osd_up: osd_ready,
                total_bytes: cap.get("bytesTotal").and_then(|v| v.as_u64()).unwrap_or(0),
                used_bytes: cap.get("bytesUsed").and_then(|v| v.as_u64()).unwrap_or(0),
                error: None,
            })
        }
        Err(e) => Json(CephStatus {
            available: false,
            health: "HEALTH_UNKNOWN".into(),
            osd_count: 0,
            osd_up: 0,
            total_bytes: 0,
            used_bytes: 0,
            error: Some(e.to_string()),
        }),
    }
}

/// Cluster status plus OSD totals.
///
/// Both used to be assembled from Rook: `.status` off the CephCluster CR and a
/// count of `app=rook-ceph-osd` Deployments. Neither exists now, and counting
/// Deployments was always a proxy anyway — it reported how many OSD *pods* Rook
/// had scheduled, not how many OSDs Ceph actually had up. This asks Ceph.
///
/// Name kept so callers are untouched; nothing about it is k8s any more.
pub async fn cluster_status_from_k8s(
) -> anyhow::Result<(serde_json::Map<String, serde_json::Value>, u32, u32)> {
    let stat = crate::ceph_cli::ceph_json(&["osd", "stat"]).await?;
    let osd_total = stat["num_osds"].as_u64().unwrap_or(0) as u32;
    let osd_ready = stat["num_up_osds"].as_u64().unwrap_or(0) as u32;

    // Shaped like the old CR status so the UI contract does not change.
    let health = crate::ceph_cli::ceph_json(&["health"])
        .await
        .unwrap_or_default();
    let mut status = serde_json::Map::new();
    status.insert(
        "ceph".into(),
        serde_json::json!({
            "health": health["status"].as_str().unwrap_or(""),
            "details": health.get("checks").cloned().unwrap_or(serde_json::json!({})),
        }),
    );

    Ok((status, osd_total, osd_ready))
}

/// "<HEALTH_X>\n<details-json>", the shape compute_cluster_health parses.
async fn ceph_health_and_details() -> anyhow::Result<String> {
    let h = crate::ceph_cli::ceph_json(&["health", "detail"]).await?;
    let status = h["status"].as_str().unwrap_or("");
    let checks = h.get("checks").cloned().unwrap_or(serde_json::json!({}));
    Ok(format!("{status}\n{checks}"))
}

// ── Storage detail ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct OsdInfo {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub class: String,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub utilization: f64,
    pub var: f64,
    pub pgs: u64,
    pub status: String,
    /// CRUSH weight (0.0 = inactive/not yet activated, >0 = participating).
    pub crush_weight: f64,
    /// OSD reweight (0.0 = explicitly out/draining, 1.0 = in).
    pub reweight: f64,
    /// True when `ceph osd safe-to-destroy` confirms no data remains on this OSD.
    /// Combined with crush_weight=0 + reweight=0, this means the disk can be unplugged.
    pub safe_to_destroy: bool,
    /// True when `ceph osd ok-to-stop` confirms losing this OSD won't block any I/O
    /// (all its PGs still meet min_size on remaining OSDs). The disk can be lost without
    /// service disruption — data degrades but stays accessible.
    pub ok_to_stop: bool,
}

#[derive(Serialize)]
pub struct PoolInfo {
    pub id: u64,
    pub name: String,
    pub size: u32,
    pub min_size: u32,
    pub crush_rule_name: String,
    pub failure_domain: String,
    pub stored_bytes: u64,
    pub used_bytes: u64,
    pub max_avail_bytes: u64,
}

#[derive(Serialize)]
pub struct StorageDetail {
    pub osds: Vec<OsdInfo>,
    pub pools: Vec<PoolInfo>,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Deserialize)]
pub struct SetReplicationReq {
    pub size: u32,
    pub min_size: u32,
    pub failure_domain: String,
}

/// Everything the Storage page needs, in one shot.
///
/// This used to be a ~40-line shell script piped into a Rook pod, hand-building
/// JSON with `echo` and juggling per-call temp keyring paths so concurrent
/// polls would not clobber each other. With Ceph on the host it is just a
/// handful of subprocess calls, so all of that is gone: no pod discovery, no
/// keyring copying, no /tmp collisions, and no shell quoting to get wrong.
async fn fetch_storage_raw() -> anyhow::Result<serde_json::Value> {
    use crate::ceph_cli::ceph_json;

    let osd_df = ceph_json(&["osd", "df", "tree"]).await.unwrap_or_default();
    let pool_detail = ceph_json(&["osd", "pool", "ls", "detail"])
        .await
        .unwrap_or_default();
    let ceph_df = ceph_json(&["df"]).await.unwrap_or_default();
    let crush_rules = ceph_json(&["osd", "crush", "rule", "dump"])
        .await
        .unwrap_or_default();

    // Ask per OSD, never in bulk. `safe-to-destroy osd.a osd.b` answers "can all
    // of these go at once?", which is nearly always false and would mark every
    // disk unremovable.
    let ids: Vec<i64> = ceph_json(&["osd", "ls"])
        .await
        .ok()
        .and_then(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        })
        .unwrap_or_default();

    let mut safe_to_destroy = Vec::new();
    let mut ok_to_stop = Vec::new();
    for id in ids {
        if crate::ceph_cli::osd_safe_to_destroy(id).await {
            safe_to_destroy.push(id);
        }
        // ok-to-stop exits 0 when losing this OSD would not block I/O.
        if crate::ceph_cli::ceph(&["osd", "ok-to-stop", &format!("osd.{id}")])
            .await
            .is_ok()
        {
            ok_to_stop.push(id);
        }
    }

    Ok(serde_json::json!({
        "osd_df": osd_df,
        "pool_detail": pool_detail,
        "ceph_df": ceph_df,
        "crush_rules": crush_rules,
        "safe_to_destroy": { "safe_to_destroy": safe_to_destroy },
        "ok_to_stop": { "ok_to_stop": ok_to_stop },
    }))
}

fn failure_domain_from_rule(rule: &serde_json::Value) -> String {
    rule["steps"]
        .as_array()
        .and_then(|steps| {
            steps.iter().find_map(|s| {
                let op = s["op"].as_str().unwrap_or("");
                if op.contains("choose") {
                    s["type"].as_str().map(str::to_string)
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "host".into())
}

fn parse_storage_detail(v: &serde_json::Value) -> StorageDetail {
    // ── safe-to-destroy set ────────────────────────────────────────────────────
    // ceph osd safe-to-destroy returns {"safe_to_destroy": [id, ...], "active": [...], ...}
    let safe_ids: std::collections::HashSet<i64> = v["safe_to_destroy"]["safe_to_destroy"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    // ── ok-to-stop set ────────────────────────────────────────────────────────
    // ok-to-stop exit 0 = losing this OSD won't block I/O (PGs still meet min_size)
    let ok_to_stop_ids: std::collections::HashSet<i64> = v["ok_to_stop"]["ok_to_stop"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    // ── OSD tree ───────────────────────────────────────────────────────────────
    let nodes = v["osd_df"]["nodes"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let mut osd_host: HashMap<i64, String> = HashMap::new();
    for n in nodes {
        if n["type"].as_str() == Some("host") {
            let host = n["name"].as_str().unwrap_or("unknown").to_string();
            if let Some(children) = n["children"].as_array() {
                for c in children {
                    if let Some(id) = c.as_i64() {
                        osd_host.insert(id, host.clone());
                    }
                }
            }
        }
    }

    let mut osds: Vec<OsdInfo> = nodes
        .iter()
        .filter(|n| n["type"].as_str() == Some("osd"))
        .map(|n| {
            let id = n["id"].as_i64().unwrap_or(0);
            let kb = n["kb"].as_u64().unwrap_or(0);
            let kb_used = n["kb_used"].as_u64().unwrap_or(0);
            let kb_avail = n["kb_avail"].as_u64().unwrap_or(0);
            OsdInfo {
                id,
                name: n["name"].as_str().unwrap_or("").to_string(),
                host: osd_host
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into()),
                class: n["class"]
                    .as_str()
                    .or_else(|| n["device_class"].as_str())
                    .unwrap_or("")
                    .to_string(),
                size_bytes: kb * 1024,
                used_bytes: kb_used * 1024,
                avail_bytes: kb_avail * 1024,
                utilization: n["utilization"].as_f64().unwrap_or(0.0),
                var: n["var"].as_f64().unwrap_or(1.0),
                pgs: n["pgs"].as_u64().unwrap_or(0),
                status: n["status"].as_str().unwrap_or("unknown").to_string(),
                crush_weight: n["crush_weight"].as_f64().unwrap_or(0.0),
                reweight: n["reweight"].as_f64().unwrap_or(1.0),
                safe_to_destroy: safe_ids.contains(&id),
                ok_to_stop: ok_to_stop_ids.contains(&id),
            }
        })
        .collect();
    osds.sort_by(|a, b| a.host.cmp(&b.host).then(a.id.cmp(&b.id)));

    // ── CRUSH rules → failure domain map ──────────────────────────────────────
    let crush_rules = v["crush_rules"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let rule_fd: HashMap<u64, String> = crush_rules
        .iter()
        .filter_map(|r| {
            r["rule_id"]
                .as_u64()
                .map(|id| (id, failure_domain_from_rule(r)))
        })
        .collect();
    let rule_names: HashMap<u64, String> = crush_rules
        .iter()
        .filter_map(|r| {
            r["rule_id"]
                .as_u64()
                .map(|id| (id, r["rule_name"].as_str().unwrap_or("").to_string()))
        })
        .collect();

    // ── Pool df (max_avail, stored, used) ─────────────────────────────────────
    let df_pools = v["ceph_df"]["pools"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let df_by_id: HashMap<u64, &serde_json::Value> = df_pools
        .iter()
        .filter_map(|p| p["id"].as_u64().map(|id| (id, p)))
        .collect();

    // ── Pool detail ────────────────────────────────────────────────────────────
    let pool_detail = v["pool_detail"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let pools: Vec<PoolInfo> = pool_detail
        .iter()
        .map(|pd| {
            let pool_id = pd["pool"]
                .as_u64()
                .or_else(|| pd["pool_id"].as_u64())
                .unwrap_or(0);
            let crush_rule_id = pd["crush_rule"].as_u64().unwrap_or(0);
            let df = df_by_id.get(&pool_id);
            PoolInfo {
                id: pool_id,
                name: pd["pool_name"].as_str().unwrap_or("").to_string(),
                size: pd["size"].as_u64().unwrap_or(1) as u32,
                min_size: pd["min_size"].as_u64().unwrap_or(1) as u32,
                crush_rule_name: rule_names
                    .get(&crush_rule_id)
                    .cloned()
                    .unwrap_or_else(|| format!("rule-{}", crush_rule_id)),
                failure_domain: rule_fd
                    .get(&crush_rule_id)
                    .cloned()
                    .unwrap_or_else(|| "host".into()),
                stored_bytes: df.and_then(|p| p["stats"]["stored"].as_u64()).unwrap_or(0),
                used_bytes: df
                    .and_then(|p| p["stats"]["bytes_used"].as_u64())
                    .unwrap_or(0),
                max_avail_bytes: df
                    .and_then(|p| p["stats"]["max_avail"].as_u64())
                    .unwrap_or(0),
            }
        })
        .collect();

    let stats = &v["ceph_df"]["stats"];
    StorageDetail {
        osds,
        pools,
        total_bytes: stats["total_bytes"].as_u64().unwrap_or(0),
        avail_bytes: stats["total_avail_bytes"].as_u64().unwrap_or(0),
        used_bytes: stats["total_used_raw_bytes"].as_u64().unwrap_or(0),
    }
}

pub async fn storage_detail() -> Json<serde_json::Value> {
    match fetch_storage_raw().await {
        Ok(raw) => Json(serde_json::json!({ "ok": true, "data": parse_storage_detail(&raw) })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

pub async fn set_replication(
    Json(req): Json<SetReplicationReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    if req.size < 1 || req.size > 3 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "size must be 1–3"})),
        );
    }
    if req.min_size < 1 || req.min_size > req.size {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "min_size must be ≥1 and ≤size"})),
        );
    }
    if req.failure_domain != "osd" && req.failure_domain != "host" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "failure_domain must be osd or host"})),
        );
    }

    let rule_name = if req.failure_domain == "osd" {
        "replicated_osd"
    } else {
        "replicated_rule"
    };
    let fd = &req.failure_domain;
    let size = req.size;
    let min_size = req.min_size;

    // Create the OSD-level CRUSH rule if it does not exist yet
    // (replicated_rule already covers the host domain).
    let have_rules = crate::ceph_cli::ceph(&["osd", "crush", "rule", "ls"])
        .await
        .unwrap_or_default();
    if !have_rules.lines().any(|l| l.trim() == rule_name) {
        if let Err(e) = crate::ceph_cli::ceph(&[
            "osd",
            "crush",
            "rule",
            "create-replicated",
            rule_name,
            "default",
            fd,
        ])
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("create crush rule: {e}")})),
            );
        }
    }

    let pools_raw = match crate::ceph_cli::ceph(&["osd", "pool", "ls"]).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    };

    let size_s = size.to_string();
    let min_size_s = min_size.to_string();
    let mut output = String::new();

    for pool in pools_raw.lines().map(str::trim).filter(|p| !p.is_empty()) {
        // .nfs and .rgw.* carry stricter placement requirements and are left alone.
        if pool.starts_with(".nfs") || pool.starts_with(".rgw") {
            continue;
        }
        // The images pool is deliberately size=1 and must stay that way: every
        // node holds its own copy of every container image, so replicating them
        // costs 3x for data that is re-downloadable from a registry. Sweeping it
        // up with the app-data pools would silently triple image storage.
        if pool == "images" {
            output.push_str("Skipped pool images (kept at size 1 by design)\n");
            continue;
        }

        let _ = crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "crush_rule", rule_name]).await;

        // size=1 requires --yes-i-really-mean-it; harmless for size>1.
        let r = if size == 1 {
            crate::ceph_cli::ceph(&[
                "osd",
                "pool",
                "set",
                pool,
                "size",
                &size_s,
                "--yes-i-really-mean-it",
            ])
            .await
        } else {
            crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "size", &size_s]).await
        };
        if let Err(e) = r {
            output.push_str(&format!("Pool {pool}: size failed: {e}\n"));
            continue;
        }

        let _ = crate::ceph_cli::ceph(&["osd", "pool", "set", pool, "min_size", &min_size_s]).await;
        output.push_str(&format!("Updated pool {pool}\n"));
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "output": output})),
    )
}

// ── OSD lifecycle ──────────────────────────────────────────────────────────────
//
// There is exactly one source of truth for whether a disk is in the cluster: the
// `yolab-disk-config` ConfigMap (DISK → USING|OFF). Both the main toggle and these
// Advanced buttons write that map; the disk reconciler (disks_reconciler.rs) is the
// single actuator that drives crush weight + in/out to match. So "Re-add" / "Remove
// safely" here simply set desired ON/OFF for the disk backing this OSD.

/// Re-add the disk backing this OSD: set its desired state to USING.
pub async fn osd_mark_in(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    set_desired_by_osd(id, "USING").await
}

/// Remove the disk backing this OSD safely: set its desired state to OFF. The
/// reconciler drains it (osd out); it's fine if draining takes a long time.
pub async fn osd_mark_out(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    set_desired_by_osd(id, "OFF").await
}

/// Resolve an OSD id to its `{node}--{disk_id}` config key (via the disk-status
/// ConfigMap) and set that key's desired state. Keeps the Advanced buttons on the
/// same source of truth as the main toggle.
async fn set_desired_by_osd(id: i64, desired: &str) -> (StatusCode, Json<serde_json::Value>) {
    let status = kubectl::get_json(&[
        "get",
        "configmap",
        "yolab-disk-status",
        "-n",
        "rook-ceph",
        "-o",
        "jsonpath={.data}",
    ])
    .await
    .ok();

    let mut found: Option<(String, String)> = None;
    if let Some(map) = status.as_ref().and_then(|v| v.as_object()) {
        for (node, payload) in map {
            let Some(s) = payload.as_str() else { continue };
            let Ok(p) = serde_json::from_str::<serde_json::Value>(s) else {
                continue;
            };
            if let Some(disks) = p["disks"].as_object() {
                for (disk_id, meta) in disks {
                    if meta["osd_id"].as_i64() == Some(id) {
                        found = Some((node.clone(), disk_id.clone()));
                    }
                }
            }
        }
    }

    let Some((node, disk_id)) = found else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "ok": false, "error": format!("osd.{id} not found in disk inventory")
            })),
        );
    };

    let key = format!("{node}--{disk_id}");
    let patch = serde_json::json!({"data": {key: desired}}).to_string();
    if kubectl::run(&[
        "patch",
        "configmap",
        "yolab-disk-config",
        "-n",
        "rook-ceph",
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await
    .is_err()
    {
        let _ = kubectl::run(&[
            "create",
            "configmap",
            "yolab-disk-config",
            "-n",
            "rook-ceph",
        ])
        .await;
        if kubectl::run(&[
            "patch",
            "configmap",
            "yolab-disk-config",
            "-n",
            "rook-ceph",
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        .is_err()
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false, "error": "failed to save disk config"
                })),
            );
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

/// Where homelab/nixos/ceph/dashboard.nix writes the generated password.
const DASHBOARD_PASSWORD_FILE: &str = "/var/lib/ceph/dashboard-password";

/// The credentials shown next to the dashboard link.
///
/// These used to come from the `rook-ceph-dashboard-password` Secret. Ceph left
/// Kubernetes and the Secret left with it, so this returned an empty string that
/// the page rendered as a row of dots — credentials that looked real and could
/// not work. The password now comes from the same file the mgr was configured
/// with, so the two cannot drift apart.
pub async fn dashboard_creds() -> Json<serde_json::Value> {
    let password = std::fs::read_to_string(DASHBOARD_PASSWORD_FILE)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    Json(serde_json::json!({
        "username": "admin",
        "password": password,
        // The page can say "not ready yet" instead of showing empty dots.
        "ready": !password.is_empty(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── translate_health_check ────────────────────────────────────────────────
    //
    // These strings are the entire storage vocabulary a non-technical user ever
    // sees, so what matters is that a code maps to plain language, that severity
    // survives, and that nothing leaks Ceph jargon unfiltered.

    fn warn() -> serde_json::Value {
        json!({"severity": "HEALTH_WARN", "summary": {"message": "some detail"}})
    }

    fn err() -> serde_json::Value {
        json!({"severity": "HEALTH_ERR", "summary": {"message": "some detail"}})
    }

    #[test]
    fn health_check_translates_a_known_code_to_plain_language() {
        let issue = translate_health_check("POOL_NO_REDUNDANCY", &warn(), 3, None).unwrap();
        assert_eq!(issue.title, "No second copy of your data");
        assert!(issue.description.contains("stored once"));
        assert_eq!(issue.level, HealthLevel::Warn);
    }

    // ── Recoverable vs gone ───────────────────────────────────────────────────
    //
    // From a real disk pull at size=1: 63 of 81 PGs went `stale+active+clean`, every
    // Ceph check said HEALTH_WARN, and the page said "temporarily unavailable … until
    // recovery". Nothing was recovering. These pin the difference.

    fn loss(stuck: u32, total: u32, unrecoverable: bool) -> PgLoss {
        PgLoss {
            stuck,
            total,
            unrecoverable,
        }
    }

    #[test]
    fn a_stale_pg_at_one_copy_is_never_called_temporary() {
        let (title, body) = unavailable_message(Some(&loss(63, 81, true)));
        assert!(title.contains("cannot be rebuilt"), "{title}");
        assert!(!title.to_lowercase().contains("temporar"), "{title}");
        assert!(!body.to_lowercase().contains("until recovery"), "{body}");
        // The two things a person can actually do.
        assert!(body.contains("reconnecting it"), "{body}");
        assert!(body.contains("backup"), "{body}");
        // And what it means for them right now.
        assert!(body.contains("63 of 81"), "{body}");
        assert!(body.contains("will not start or will hang"), "{body}");
    }

    /// With copies elsewhere it really is temporary, and saying so is right — this is
    /// the case the old wording was written for.
    #[test]
    fn a_replicated_cluster_is_still_told_to_wait() {
        let (title, body) = unavailable_message(Some(&loss(5, 81, false)));
        assert!(body.contains("repairs itself"), "{body}");
        assert!(!body.contains("backup"), "{body}");
        assert!(!title.contains("cannot be rebuilt"), "{title}");
    }

    /// When the assessment cannot be made, do not claim data is lost.
    #[test]
    fn an_unreadable_assessment_takes_the_cautious_branch() {
        let (title, body) = unavailable_message(None);
        assert!(!title.contains("cannot be rebuilt"), "{title}");
        assert!(body.contains("repairs itself"), "{body}");
    }

    /// `stale` and `down` mean the holder is not answering. `degraded` and
    /// `undersized` mean a copy is missing while another serves — Ceph is already
    /// fixing those, and calling them lost would cry wolf on every reboot.
    #[test]
    fn only_states_meaning_nobody_is_answering_count_as_stuck() {
        for gone in [
            "stale+active+clean",
            "down+peering",
            "incomplete",
            "unknown",
            "stale+peering",
        ] {
            assert!(is_stuck_state(gone), "{gone} means unreachable");
        }
        for fine in [
            "active+clean",
            "active+undersized+degraded",
            "active+clean+remapped",
            "peering",
            "active+recovering+degraded",
            "active+clean+scrubbing+deep",
        ] {
            assert!(!is_stuck_state(fine), "{fine} is not data loss");
        }
    }

    /// The whole issue, as the page receives it: level is Error even though every Ceph
    /// check was only a warning.
    #[test]
    fn the_page_is_told_this_is_an_error_not_a_warning() {
        let issue =
            translate_health_check("PG_AVAILABILITY", &warn(), 3, Some(&loss(63, 81, true)))
                .unwrap();
        assert_eq!(issue.level, HealthLevel::Error);
        assert!(issue.title.contains("cannot be rebuilt"), "{}", issue.title);
    }

    // ── no_redundancy_message ─────────────────────────────────────────────────
    //
    // This is the only sentence that ever tells someone their data is unprotected,
    // and until now it could not be shown at all: the mon check that raises
    // POOL_NO_REDUNDANCY was disabled cluster-wide in nixos/ceph/default.nix, so a
    // three-disk cluster keeping one copy of everything reported HEALTH_OK. These
    // pin both halves of the repair — that it fires, and that it says something
    // the reader can act on.

    /// A machine that genuinely cannot replicate must be pointed at backups, not
    /// told to add copies it has nowhere to put.
    #[test]
    fn with_one_disk_the_advice_is_backups() {
        for places in [0, 1] {
            let m = no_redundancy_message(places);
            assert!(m.contains("Turn on backups"), "places={places}: {m}");
            assert!(
                !m.contains("Raise the number of copies"),
                "places={places}: {m}"
            );
        }
    }

    /// A machine that CAN replicate must be told so. The old text said "this is
    /// expected with a single-disk setup" whatever the disk count, which talked
    /// the owner out of the one action that would have protected them.
    #[test]
    fn with_several_disks_the_advice_is_to_raise_the_copy_count() {
        let m = no_redundancy_message(3);
        assert!(m.contains("Raise the number of copies"), "{m}");
        assert!(
            m.contains('3'),
            "the count the owner can see must appear: {m}"
        );
        assert!(!m.contains("expected"), "must not call this normal: {m}");
    }

    /// Both codes Ceph can raise for this condition say the same thing — they are
    /// the same fact and used to be two separately-maintained strings.
    #[test]
    fn both_no_redundancy_codes_give_the_same_advice() {
        for code in ["POOL_NO_REDUNDANCY", "POOL_TOTAL_SIZE_MIN_SIZE_REACHED"] {
            let issue = translate_health_check(code, &warn(), 2, None).unwrap();
            assert_eq!(issue.title, "No second copy of your data");
            assert_eq!(issue.description, no_redundancy_message(2));
        }
    }

    #[test]
    fn health_check_carries_cephs_severity_through() {
        assert_eq!(
            translate_health_check("OSD_DOWN", &err(), 3, None)
                .unwrap()
                .level,
            HealthLevel::Error
        );
        assert_eq!(
            translate_health_check("OSD_DOWN", &warn(), 3, None)
                .unwrap()
                .level,
            HealthLevel::Warn
        );
    }

    #[test]
    fn health_check_defaults_to_warn_when_severity_is_missing() {
        let issue = translate_health_check("OSD_DOWN", &json!({}), 3, None).unwrap();
        assert_eq!(issue.level, HealthLevel::Warn);
    }

    /// Ceph reports unavailable placement groups as HEALTH_WARN, but apps reading
    /// or writing affected files hang outright. Presenting that as a yellow
    /// warning would tell the user "minor issue" while their apps are frozen.
    #[test]
    fn unavailable_data_is_always_an_error_even_when_ceph_calls_it_a_warning() {
        for code in ["PG_DOWN", "PG_AVAILABILITY"] {
            let issue = translate_health_check(code, &warn(), 3, None).unwrap();
            assert_eq!(issue.level, HealthLevel::Error, "{code}");
            assert_eq!(issue.title, "Some files are unreachable right now");
        }
    }

    /// Transient post-startup states. Surfacing them would mean a freshly booted
    /// cluster always looks broken, training users to ignore the health panel.
    #[test]
    fn routine_transient_states_are_suppressed_entirely() {
        for code in [
            "PG_PEERING",
            "PG_NOT_SCRUBBED",
            "PG_NOT_DEEP_SCRUBBED",
            "PG_NOT_SCRUBBED_SINCE",
        ] {
            assert!(
                translate_health_check(code, &warn(), 3, None).is_none(),
                "{code} should not be shown to the user"
            );
            // Not even when Ceph escalates them.
            assert!(
                translate_health_check(code, &err(), 3, None).is_none(),
                "{code} (err)"
            );
        }
    }

    #[test]
    fn an_unknown_code_falls_back_to_cephs_own_summary() {
        let detail = json!({
            "severity": "HEALTH_WARN",
            "summary": {"message": "BLUEFS_SPILLOVER: 1 OSD(s) experiencing spillover"},
        });
        let issue = translate_health_check("BLUEFS_SPILLOVER", &detail, 3, None).unwrap();
        // Title takes the part before the first colon; the body keeps the whole line.
        assert_eq!(issue.title, "Storage issue: BLUEFS_SPILLOVER");
        assert_eq!(
            issue.description,
            "BLUEFS_SPILLOVER: 1 OSD(s) experiencing spillover"
        );
    }

    #[test]
    fn an_unknown_code_with_no_summary_still_names_itself() {
        let issue = translate_health_check("SOMETHING_NEW", &json!({}), 3, None).unwrap();
        assert_eq!(issue.title, "Storage issue: SOMETHING_NEW");
        assert_eq!(issue.description, "SOMETHING_NEW");
    }

    /// A blank title renders as an empty row in the UI — worse than raw jargon,
    /// because it looks like a rendering bug rather than a storage problem.
    #[test]
    fn every_translated_code_produces_non_empty_text() {
        let codes = [
            "POOL_NO_REDUNDANCY",
            "MDS_ALL_DOWN",
            "MDS_DAMAGE",
            "MDS_SLOW_METADATA_IO",
            "MDS_SLOW_REQUEST",
            "OSD_DOWN",
            "OSD_NEARFULL",
            "OSD_FULL",
            "NOSPC",
            "MON_DOWN",
            "MON_DISK_LOW",
            "MON_DISK_CRIT",
            "MON_DISK_BIG",
            "MON_CLOCK_SKEW",
            "PG_DEGRADED",
            "PG_DOWN",
            "PG_AVAILABILITY",
            "SLOW_OPS",
            "OBJECT_UNFOUND",
            "RECENT_CRASH",
            "POOL_TOTAL_SIZE_MIN_SIZE_REACHED",
        ];
        for code in codes {
            let issue = translate_health_check(code, &warn(), 3, None)
                .unwrap_or_else(|| panic!("{code} should be surfaced, not suppressed"));
            assert!(!issue.title.trim().is_empty(), "{code} has a blank title");
            assert!(
                !issue.description.trim().is_empty(),
                "{code} has a blank description"
            );
            assert!(
                !issue.title.contains(code),
                "{code} fell through to the untranslated branch"
            );
        }
    }

    #[test]
    fn a_full_disk_reads_the_same_whichever_code_ceph_uses() {
        let a = translate_health_check("OSD_FULL", &err(), 3, None).unwrap();
        let b = translate_health_check("NOSPC", &err(), 3, None).unwrap();
        assert_eq!(a.title, b.title);
        assert_eq!(a.description, b.description);
    }

    // ── failure_domain_from_rule ──────────────────────────────────────────────

    #[test]
    fn failure_domain_comes_from_the_choose_step() {
        let rule = json!({"steps": [
            {"op": "take", "item_name": "default"},
            {"op": "chooseleaf_firstn", "num": 0, "type": "host"},
            {"op": "emit"},
        ]});
        assert_eq!(failure_domain_from_rule(&rule), "host");
    }

    #[test]
    fn failure_domain_reads_osd_for_a_single_node_rule() {
        let rule = json!({"steps": [
            {"op": "take", "item_name": "default"},
            {"op": "choose_firstn", "num": 0, "type": "osd"},
        ]});
        assert_eq!(failure_domain_from_rule(&rule), "osd");
    }

    #[test]
    fn failure_domain_uses_the_first_choose_step_it_finds() {
        let rule = json!({"steps": [
            {"op": "chooseleaf_firstn", "type": "rack"},
            {"op": "chooseleaf_firstn", "type": "host"},
        ]});
        assert_eq!(failure_domain_from_rule(&rule), "rack");
    }

    /// `host` is the safe default: it never claims more independence between
    /// copies than actually exists.
    #[test]
    fn failure_domain_defaults_to_host_when_it_cannot_be_determined() {
        assert_eq!(failure_domain_from_rule(&json!({})), "host");
        assert_eq!(failure_domain_from_rule(&json!({"steps": []})), "host");
        assert_eq!(
            failure_domain_from_rule(&json!({"steps": [{"op": "emit"}]})),
            "host"
        );
        // A choose step with no type at all.
        assert_eq!(
            failure_domain_from_rule(&json!({"steps": [{"op": "chooseleaf_firstn"}]})),
            "host"
        );
    }

    // ── parse_storage_detail ──────────────────────────────────────────────────

    fn sample_raw() -> serde_json::Value {
        json!({
            "safe_to_destroy": {"safe_to_destroy": [1]},
            "ok_to_stop": {"ok_to_stop": [0, 1]},
            "osd_df": {
                "nodes": [
                    {"type": "host", "name": "node2", "children": [1]},
                    {"type": "host", "name": "node1", "children": [0]},
                    {
                        "type": "osd", "id": 0, "name": "osd.0", "device_class": "ssd",
                        "kb": 2_000_000, "kb_used": 500_000, "kb_avail": 1_500_000,
                        "utilization": 25.0, "var": 1.1, "pgs": 32, "status": "up",
                        "crush_weight": 1.9, "reweight": 1.0
                    },
                    {
                        "type": "osd", "id": 1, "name": "osd.1", "class": "hdd",
                        "kb": 1_000_000, "kb_used": 100_000, "kb_avail": 900_000,
                        "utilization": 10.0, "var": 0.4, "pgs": 16, "status": "down",
                        "crush_weight": 0.0, "reweight": 0.0
                    },
                ],
                "stray": []
            },
            "crush_rules": [
                {"rule_id": 0, "rule_name": "replicated_rule",
                 "steps": [{"op": "chooseleaf_firstn", "type": "host"}]},
                {"rule_id": 1, "rule_name": "single_node",
                 "steps": [{"op": "chooseleaf_firstn", "type": "osd"}]},
            ],
            "ceph_df": {
                "stats": {
                    "total_bytes": 3_000_000_000u64,
                    "total_avail_bytes": 2_400_000_000u64,
                    "total_used_raw_bytes": 600_000_000u64
                },
                "pools": [
                    {"id": 3, "stats": {"stored": 111, "bytes_used": 333, "max_avail": 999}},
                ]
            },
            "pool_detail": [
                {"pool": 3, "pool_name": "yolab-blockpool", "size": 2, "min_size": 1, "crush_rule": 0},
                {"pool": 4, "pool_name": "orphan-pool", "size": 1, "min_size": 1, "crush_rule": 7},
            ]
        })
    }

    #[test]
    fn storage_detail_converts_cephs_kilobytes_to_bytes() {
        let d = parse_storage_detail(&sample_raw());
        let osd0 = d.osds.iter().find(|o| o.id == 0).unwrap();
        assert_eq!(osd0.size_bytes, 2_000_000 * 1024);
        assert_eq!(osd0.used_bytes, 500_000 * 1024);
        assert_eq!(osd0.avail_bytes, 1_500_000 * 1024);
    }

    #[test]
    fn storage_detail_attributes_each_osd_to_its_host() {
        let d = parse_storage_detail(&sample_raw());
        assert_eq!(d.osds.iter().find(|o| o.id == 0).unwrap().host, "node1");
        assert_eq!(d.osds.iter().find(|o| o.id == 1).unwrap().host, "node2");
    }

    #[test]
    fn storage_detail_accepts_either_class_spelling() {
        // `osd df tree` says `device_class`; some Ceph versions emit `class`.
        let d = parse_storage_detail(&sample_raw());
        assert_eq!(d.osds.iter().find(|o| o.id == 0).unwrap().class, "ssd");
        assert_eq!(d.osds.iter().find(|o| o.id == 1).unwrap().class, "hdd");
    }

    /// These two flags are what the UI turns into "safe to unplug". Getting the
    /// set membership backwards would tell someone to pull a disk that still
    /// holds the only copy of their data.
    #[test]
    fn storage_detail_marks_only_the_osds_ceph_cleared() {
        let d = parse_storage_detail(&sample_raw());
        let osd0 = d.osds.iter().find(|o| o.id == 0).unwrap();
        let osd1 = d.osds.iter().find(|o| o.id == 1).unwrap();
        assert!(!osd0.safe_to_destroy);
        assert!(osd1.safe_to_destroy);
        assert!(osd0.ok_to_stop);
        assert!(osd1.ok_to_stop);
    }

    /// Absent lists must read as "nothing is cleared", never "everything is".
    #[test]
    fn storage_detail_clears_nothing_when_ceph_returned_no_verdict() {
        let mut raw = sample_raw();
        raw["safe_to_destroy"] = json!({});
        raw["ok_to_stop"] = json!({});
        let d = parse_storage_detail(&raw);
        assert!(d.osds.iter().all(|o| !o.safe_to_destroy && !o.ok_to_stop));
    }

    #[test]
    fn storage_detail_sorts_osds_by_host_then_id() {
        let d = parse_storage_detail(&sample_raw());
        let order: Vec<(&str, i64)> = d.osds.iter().map(|o| (o.host.as_str(), o.id)).collect();
        assert_eq!(order, vec![("node1", 0), ("node2", 1)]);
    }

    #[test]
    fn storage_detail_resolves_pool_rules_to_names_and_failure_domains() {
        let d = parse_storage_detail(&sample_raw());
        let pool = d
            .pools
            .iter()
            .find(|p| p.name == "yolab-blockpool")
            .unwrap();
        assert_eq!(pool.crush_rule_name, "replicated_rule");
        assert_eq!(pool.failure_domain, "host");
        assert_eq!(pool.size, 2);
        assert_eq!(pool.min_size, 1);
    }

    #[test]
    fn storage_detail_joins_pool_usage_by_id() {
        let d = parse_storage_detail(&sample_raw());
        let pool = d.pools.iter().find(|p| p.id == 3).unwrap();
        assert_eq!(pool.stored_bytes, 111);
        assert_eq!(pool.used_bytes, 333);
        assert_eq!(pool.max_avail_bytes, 999);

        // Pool 4 has no ceph df entry — usage reads as zero, not as pool 3's numbers.
        let orphan = d.pools.iter().find(|p| p.id == 4).unwrap();
        assert_eq!(orphan.stored_bytes, 0);
        assert_eq!(orphan.max_avail_bytes, 0);
    }

    #[test]
    fn storage_detail_names_an_unresolvable_crush_rule_by_id() {
        let d = parse_storage_detail(&sample_raw());
        let orphan = d.pools.iter().find(|p| p.id == 4).unwrap();
        assert_eq!(orphan.crush_rule_name, "rule-7");
        assert_eq!(orphan.failure_domain, "host");
    }

    #[test]
    fn storage_detail_reads_cluster_totals() {
        let d = parse_storage_detail(&sample_raw());
        assert_eq!(d.total_bytes, 3_000_000_000);
        assert_eq!(d.avail_bytes, 2_400_000_000);
        assert_eq!(d.used_bytes, 600_000_000);
    }

    /// The Ceph exec can return `{}`, an error object, or a partial document when
    /// the cluster is mid-outage — precisely when the storage page is being looked
    /// at. Every one of those has to render as empty, not panic.
    #[test]
    fn storage_detail_survives_empty_and_malformed_input() {
        for raw in [
            json!({}),
            json!({"osd_df": null, "ceph_df": null, "pool_detail": null}),
            json!({"osd_df": {"nodes": "not-an-array"}}),
            json!({"pool_detail": [{}]}),
            json!({"osd_df": {"nodes": [{"type": "osd"}]}}),
        ] {
            let d = parse_storage_detail(&raw);
            assert_eq!(d.total_bytes, 0);
            assert!(d.osds.len() <= 1);
        }
    }

    /// An OSD Ceph lists but no host claims still has to appear — a disk missing
    /// from the UI is a disk nobody knows to replace.
    #[test]
    fn storage_detail_keeps_an_osd_with_no_parent_host() {
        let raw = json!({
            "osd_df": {"nodes": [{"type": "osd", "id": 5, "name": "osd.5"}]}
        });
        let d = parse_storage_detail(&raw);
        assert_eq!(d.osds.len(), 1);
        assert_eq!(d.osds[0].host, "unknown");
        assert_eq!(d.osds[0].status, "unknown");
        // reweight defaults to 1.0 (in), not 0.0 (draining).
        assert_eq!(d.osds[0].reweight, 1.0);
    }
}

// ── Dashboard proxy ───────────────────────────────────────────────────────────

/// The active mgr's dashboard base URL, e.g. "http://[fd00:cafe::30]:7000".
///
/// `ceph mgr services` reports the URL of the mgr that is CURRENTLY ACTIVE, and
/// that is the whole reason this proxy exists. Every node runs a mgr, only one
/// of them serves the dashboard, and the others answer with a redirect naming
/// an address on the WireGuard mesh — which a browser on the internet cannot
/// reach. Proxying straight to the local mgr therefore worked or 502'd
/// depending on where the active mgr happened to be.
///
/// The returned URL already carries the configured url_prefix, which is not
/// wanted here: the caller appends the full incoming path, prefix included.
async fn active_dashboard_origin() -> Option<String> {
    let services = crate::ceph_cli::ceph_json(&["mgr", "services"])
        .await
        .ok()?;
    dashboard_origin_from(&services)
}

/// Split from the call above so the URL handling can be tested without a
/// cluster. It is small and entirely made of details that are wrong by default:
/// IPv6 literals lose their brackets, the port is optional, and the url_prefix
/// the mgr reports has to be dropped rather than kept.
pub(crate) fn dashboard_origin_from(services: &serde_json::Value) -> Option<String> {
    let url = services["dashboard"].as_str().filter(|u| !u.is_empty())?;
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port().unwrap_or(7000);
    // Url::host_str strips the brackets from an IPv6 literal, and every address
    // in this cluster is IPv6 — putting it back unbracketed produces a URL
    // where the last ":xxxx" of the address reads as the port.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(format!("{}://{}:{}", parsed.scheme(), host, port))
}

/// Reverse-proxy one request to the active mgr's dashboard.
///
/// Deliberately dumb: method, path, query, headers and body straight through,
/// and the response straight back. The dashboard is a single-page app that
/// fetches its own assets and API under the same prefix, so anything clever
/// here — rewriting bodies, following redirects — breaks it.
pub async fn dashboard_proxy(req: axum::extract::Request) -> Response {
    let Some(origin) = active_dashboard_origin().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The storage dashboard is not available right now. It runs on whichever \
             machine currently manages the cluster, and none is answering.",
        )
            .into_response();
    };

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let url = format!("{origin}{path_and_query}");

    // Redirects are PASSED THROUGH, not followed. The dashboard issues them for
    // its own login flow, and following one here would return the redirect
    // target's body under the original URL — the browser would never update its
    // address and the app would wedge.
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("dashboard proxy: could not build client: {e}");
            return (StatusCode::BAD_GATEWAY, "dashboard unavailable").into_response();
        }
    };

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "request body too large").into_response(),
    };

    let mut upstream = client.request(parts.method.clone(), &url).body(body_bytes);
    for (name, value) in parts.headers.iter() {
        // Host must be reset to the upstream, and the hop-by-hop headers
        // describe THIS connection rather than the proxied one.
        let n = name.as_str().to_ascii_lowercase();
        if matches!(
            n.as_str(),
            "host" | "connection" | "transfer-encoding" | "upgrade" | "keep-alive"
        ) {
            continue;
        }
        upstream = upstream.header(name, value);
    }

    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("dashboard proxy: {url} failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                "Could not reach the storage dashboard.",
            )
                .into_response();
        }
    };

    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("dashboard proxy: reading {url} failed: {e}");
            return (StatusCode::BAD_GATEWAY, "dashboard unavailable").into_response();
        }
    };

    let mut out = Response::builder().status(status);
    for (name, value) in headers.iter() {
        let n = name.as_str().to_ascii_lowercase();
        // content-length is recomputed from the body we actually send, and the
        // hop-by-hop headers belong to the upstream connection.
        if matches!(
            n.as_str(),
            "connection" | "transfer-encoding" | "content-length" | "keep-alive" | "upgrade"
        ) {
            continue;
        }
        out = out.header(name, value);
    }
    out.body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "dashboard unavailable").into_response())
}

#[cfg(test)]
mod dashboard_tests {
    use super::*;
    use serde_json::json;

    /// The shape `ceph mgr services` actually returns: an IPv6 mesh address,
    /// and the url_prefix already appended.
    #[test]
    fn an_ipv6_mgr_url_keeps_its_brackets() {
        let v = json!({"dashboard": "http://[fd00:cafe::30]:7000/ceph-dashboard"});
        assert_eq!(
            dashboard_origin_from(&v).as_deref(),
            Some("http://[fd00:cafe::30]:7000")
        );
    }

    /// The prefix belongs to the incoming request, which already carries it.
    /// Keeping it here would produce /ceph-dashboard/ceph-dashboard/...
    #[test]
    fn the_url_prefix_is_dropped_from_the_origin() {
        let v = json!({"dashboard": "http://[fd00:cafe::30]:7000/ceph-dashboard"});
        let origin = dashboard_origin_from(&v).unwrap();
        assert!(!origin.contains("ceph-dashboard"), "{origin}");
    }

    #[test]
    fn a_hostname_mgr_url_works_too() {
        let v = json!({"dashboard": "http://node3:7000/"});
        assert_eq!(
            dashboard_origin_from(&v).as_deref(),
            Some("http://node3:7000")
        );
    }

    /// https is what the mgr reports when ssl is left on. The scheme has to be
    /// carried through rather than assumed, or the proxy talks plaintext to a
    /// TLS port and the dashboard appears to hang.
    #[test]
    fn the_scheme_is_preserved() {
        let v = json!({"dashboard": "https://[fd00:cafe::30]:8443/"});
        assert_eq!(
            dashboard_origin_from(&v).as_deref(),
            Some("https://[fd00:cafe::30]:8443")
        );
    }

    /// No active mgr, no dashboard module, or an answer we cannot read. Each
    /// must yield None so the caller says "not available" rather than proxying
    /// to a URL it invented.
    #[test]
    fn an_unusable_answer_yields_nothing() {
        assert!(dashboard_origin_from(&json!({})).is_none());
        assert!(dashboard_origin_from(&json!({"dashboard": ""})).is_none());
        assert!(dashboard_origin_from(&json!({"dashboard": "not a url"})).is_none());
        assert!(dashboard_origin_from(&json!({"prometheus": "http://x:9283/"})).is_none());
    }
}

/// Which paths actually reach the dashboard proxy.
///
/// This exists because the migrated dashboard returned 404 — not from the
/// proxy, which answers 503 when no mgr is active and 502 when one cannot be
/// reached, but from the router, before any of that code ran.
///
/// The reason is matchit's wildcard: `/*rest` needs at least one character
/// after the slash, so `/ceph-dashboard/*rest` does NOT match
/// `/ceph-dashboard/` — and a trailing slash is exactly what a browser sends
/// for a directory-style link. `/ceph-dashboard` (no slash) is a different
/// route again. All three spellings have to be registered, and this pins that.
#[cfg(test)]
mod dashboard_route_tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::any,
        Router,
    };
    use tower::ServiceExt;

    async fn reached() -> &'static str {
        "reached the proxy"
    }

    /// The same three patterns main.rs registers.
    fn router() -> Router {
        Router::new()
            .route("/ceph-dashboard", any(reached))
            .route("/ceph-dashboard/", any(reached))
            .route("/ceph-dashboard/*rest", any(reached))
    }

    async fn status_for(path: &str) -> StatusCode {
        router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_bare_link_from_the_storage_page_reaches_the_proxy() {
        // href="/ceph-dashboard/" — the exact URL that 404'd.
        assert_eq!(status_for("/ceph-dashboard/").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_prefix_without_a_trailing_slash_reaches_the_proxy() {
        assert_eq!(status_for("/ceph-dashboard").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_dashboards_own_assets_and_api_reach_the_proxy() {
        for p in [
            "/ceph-dashboard/index.html",
            "/ceph-dashboard/api/health/minimal",
            "/ceph-dashboard/static/js/main.1234.js",
            "/ceph-dashboard/#/login",
        ] {
            assert_eq!(status_for(p).await, StatusCode::OK, "{p} must be proxied");
        }
    }

    /// Proves the wildcard alone is not enough — the shape of the original bug.
    /// If this ever starts passing, matchit changed and the explicit
    /// trailing-slash route can go.
    #[tokio::test]
    async fn a_wildcard_alone_does_not_match_a_bare_trailing_slash() {
        let only_wildcard = Router::new().route("/ceph-dashboard/*rest", any(reached));
        let status = only_wildcard
            .oneshot(
                Request::builder()
                    .uri("/ceph-dashboard/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "matchit's /*rest requires at least one character after the slash"
        );
    }
}
