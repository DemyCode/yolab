use axum::{extract::Path, http::StatusCode, Json};
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
async fn osd_provisioning_active() -> bool {
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
    let provisioning = osd_provisioning_active().await;
    // Straight from the mon: Ceph runs as host daemons, so there is no Rook CR
    // status to read. Formatted as "<HEALTH_X>\n<details-json>" to keep the
    // existing parser below unchanged.
    let raw = match ceph_health_and_details().await {
        Ok(s) => s,
        Err(_) => {
            let starting = system_uptime_secs() < 900;
            return ClusterHealth {
                level: if starting { HealthLevel::Warn } else { HealthLevel::Error },
                title: if starting {
                    "Storage is warming up".into()
                } else {
                    "Storage cluster unreachable".into()
                },
                message: if starting {
                    "Your storage is starting after a restart. Apps will be available in a few minutes.".into()
                } else {
                    "Cannot connect to the storage control plane. Check that Rook is running.".into()
                },
                issues: vec![],
                pg_unavailable: false,
                mon_quorum_ok: false,
                osd_full: false,
                starting,
                provisioning,
            };
        }
    };

    let mut lines = raw.lines();
    let health_str = lines.next().unwrap_or("").trim();
    let details_str = lines.next().unwrap_or("{}");
    let details: serde_json::Value = serde_json::from_str(details_str).unwrap_or_default();

    let mut issues: Vec<HealthIssue> = vec![];

    if let Some(obj) = details.as_object() {
        for (code, detail) in obj {
            if let Some(issue) = translate_health_check(code, detail) {
                issues.push(issue);
            }
        }
    }

    // Sort: errors first, then warns
    issues.sort_by_key(|i| if i.level == HealthLevel::Error { 0u8 } else { 1 });

    let level = if health_str == "HEALTH_OK" {
        HealthLevel::Ok
    } else if health_str == "HEALTH_ERR" || issues.iter().any(|i| i.level == HealthLevel::Error) {
        HealthLevel::Error
    } else {
        HealthLevel::Warn
    };

    // Derive machine-readable flags from the active issue codes.
    let pg_unavailable = details.as_object().map_or(false, |obj| {
        obj.contains_key("PG_AVAILABILITY") || obj.contains_key("PG_DOWN")
    });
    let osd_full = details.as_object().map_or(false, |obj| {
        obj.contains_key("OSD_FULL") || obj.contains_key("NOSPC") || obj.contains_key("POOL_FULL")
    });
    // "Starting" when reachable but PGs are still peering/recovering and system just booted.
    let starting = pg_unavailable && system_uptime_secs() < 900;

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
        },
        HealthLevel::Warn => ClusterHealth {
            level: if starting { HealthLevel::Warn } else { HealthLevel::Warn },
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
        },
        HealthLevel::Error => ClusterHealth {
            level: if starting { HealthLevel::Warn } else { HealthLevel::Error },
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
        },
    }
}

fn translate_health_check(code: &str, detail: &serde_json::Value) -> Option<HealthIssue> {
    let severity = detail["severity"].as_str().unwrap_or("HEALTH_WARN");
    let level = if severity == "HEALTH_ERR" { HealthLevel::Error } else { HealthLevel::Warn };

    let (title, description) = match code {
        "POOL_NO_REDUNDANCY" => (
            "No disk redundancy".into(),
            "Your data has no backup copy. If a disk fails, data is lost. This is expected with a single-disk setup.".into(),
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
        "PG_DEGRADED" => (
            "Data redundancy reduced".into(),
            "Some data chunks are stored on fewer disks than configured. Your cluster is recovering.".into(),
        ),
        "PG_DOWN" | "PG_AVAILABILITY" => {
            // Always critical regardless of Ceph's own severity: apps actively hang
            // on reads/writes to unavailable PGs, even when Ceph reports HEALTH_WARN.
            return Some(HealthIssue {
                level: HealthLevel::Error,
                title: "Some data temporarily unavailable".into(),
                description: "Certain data is unreachable right now. Apps reading or writing to affected files will hang until recovery.".into(),
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
            "No disk redundancy".into(),
            "Your data has no backup copy. If a disk fails, data is lost. This is expected with a single-disk setup.".into(),
        ),
        _ => {
            // Unknown code: surface it but don't translate
            let summary = detail["summary"]["message"].as_str().unwrap_or(code).to_string();
            (format!("Storage issue: {}", summary.split(':').next().unwrap_or(code)), summary)
        }
    };

    Some(HealthIssue { level, title, description })
}

pub async fn ceph_status() -> Json<CephStatus> {
    match cluster_status_from_k8s().await {
        Ok((status, osd_total, osd_ready)) => {
            let cap = status.get("ceph")
                .and_then(|c| c.get("capacity"))
                .cloned()
                .unwrap_or_default();
            Json(CephStatus {
                available: status.get("phase").and_then(|p| p.as_str()) == Some("Ready"),
                health: status.get("ceph")
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
pub async fn cluster_status_from_k8s()
-> anyhow::Result<(serde_json::Map<String, serde_json::Value>, u32, u32)> {
    let stat = crate::ceph_cli::ceph_json(&["osd", "stat"]).await?;
    let osd_total = stat["num_osds"].as_u64().unwrap_or(0) as u32;
    let osd_ready = stat["num_up_osds"].as_u64().unwrap_or(0) as u32;

    // Shaped like the old CR status so the UI contract does not change.
    let health = crate::ceph_cli::ceph_json(&["health"]).await.unwrap_or_default();
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
    let pool_detail = ceph_json(&["osd", "pool", "ls", "detail"]).await.unwrap_or_default();
    let ceph_df = ceph_json(&["df"]).await.unwrap_or_default();
    let crush_rules = ceph_json(&["osd", "crush", "rule", "dump"]).await.unwrap_or_default();

    // Ask per OSD, never in bulk. `safe-to-destroy osd.a osd.b` answers "can all
    // of these go at once?", which is nearly always false and would mark every
    // disk unremovable.
    let ids: Vec<i64> = ceph_json(&["osd", "ls"])
        .await
        .ok()
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()))
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
    rule["steps"].as_array().and_then(|steps| {
        steps.iter().find_map(|s| {
            let op = s["op"].as_str().unwrap_or("");
            if op.contains("choose") { s["type"].as_str().map(str::to_string) } else { None }
        })
    }).unwrap_or_else(|| "host".into())
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
    let nodes = v["osd_df"]["nodes"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);

    let mut osd_host: HashMap<i64, String> = HashMap::new();
    for n in nodes {
        if n["type"].as_str() == Some("host") {
            let host = n["name"].as_str().unwrap_or("unknown").to_string();
            if let Some(children) = n["children"].as_array() {
                for c in children {
                    if let Some(id) = c.as_i64() { osd_host.insert(id, host.clone()); }
                }
            }
        }
    }

    let mut osds: Vec<OsdInfo> = nodes.iter()
        .filter(|n| n["type"].as_str() == Some("osd"))
        .map(|n| {
            let id = n["id"].as_i64().unwrap_or(0);
            let kb       = n["kb"].as_u64().unwrap_or(0);
            let kb_used  = n["kb_used"].as_u64().unwrap_or(0);
            let kb_avail = n["kb_avail"].as_u64().unwrap_or(0);
            OsdInfo {
                id,
                name: n["name"].as_str().unwrap_or("").to_string(),
                host: osd_host.get(&id).cloned().unwrap_or_else(|| "unknown".into()),
                class: n["class"].as_str()
                    .or_else(|| n["device_class"].as_str())
                    .unwrap_or("").to_string(),
                size_bytes:  kb       * 1024,
                used_bytes:  kb_used  * 1024,
                avail_bytes: kb_avail * 1024,
                utilization: n["utilization"].as_f64().unwrap_or(0.0),
                var:         n["var"].as_f64().unwrap_or(1.0),
                pgs:         n["pgs"].as_u64().unwrap_or(0),
                status:      n["status"].as_str().unwrap_or("unknown").to_string(),
                crush_weight: n["crush_weight"].as_f64().unwrap_or(0.0),
                reweight:     n["reweight"].as_f64().unwrap_or(1.0),
                safe_to_destroy: safe_ids.contains(&id),
                ok_to_stop: ok_to_stop_ids.contains(&id),
            }
        })
        .collect();
    osds.sort_by(|a, b| a.host.cmp(&b.host).then(a.id.cmp(&b.id)));

    // ── CRUSH rules → failure domain map ──────────────────────────────────────
    let crush_rules = v["crush_rules"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let rule_fd: HashMap<u64, String> = crush_rules.iter()
        .filter_map(|r| r["rule_id"].as_u64().map(|id| (id, failure_domain_from_rule(r))))
        .collect();
    let rule_names: HashMap<u64, String> = crush_rules.iter()
        .filter_map(|r| r["rule_id"].as_u64().map(|id| (id, r["rule_name"].as_str().unwrap_or("").to_string())))
        .collect();

    // ── Pool df (max_avail, stored, used) ─────────────────────────────────────
    let df_pools = v["ceph_df"]["pools"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let df_by_id: HashMap<u64, &serde_json::Value> = df_pools.iter()
        .filter_map(|p| p["id"].as_u64().map(|id| (id, p)))
        .collect();

    // ── Pool detail ────────────────────────────────────────────────────────────
    let pool_detail = v["pool_detail"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let pools: Vec<PoolInfo> = pool_detail.iter().map(|pd| {
        let pool_id = pd["pool"].as_u64().or_else(|| pd["pool_id"].as_u64()).unwrap_or(0);
        let crush_rule_id = pd["crush_rule"].as_u64().unwrap_or(0);
        let df = df_by_id.get(&pool_id);
        PoolInfo {
            id: pool_id,
            name: pd["pool_name"].as_str().unwrap_or("").to_string(),
            size:     pd["size"].as_u64().unwrap_or(1) as u32,
            min_size: pd["min_size"].as_u64().unwrap_or(1) as u32,
            crush_rule_name: rule_names.get(&crush_rule_id).cloned()
                .unwrap_or_else(|| format!("rule-{}", crush_rule_id)),
            failure_domain: rule_fd.get(&crush_rule_id).cloned().unwrap_or_else(|| "host".into()),
            stored_bytes:   df.and_then(|p| p["stats"]["stored"].as_u64()).unwrap_or(0),
            used_bytes:     df.and_then(|p| p["stats"]["bytes_used"].as_u64()).unwrap_or(0),
            max_avail_bytes: df.and_then(|p| p["stats"]["max_avail"].as_u64()).unwrap_or(0),
        }
    }).collect();

    let stats = &v["ceph_df"]["stats"];
    StorageDetail {
        osds,
        pools,
        total_bytes: stats["total_bytes"].as_u64().unwrap_or(0),
        avail_bytes: stats["total_avail_bytes"].as_u64().unwrap_or(0),
        used_bytes:  stats["total_used_raw_bytes"].as_u64().unwrap_or(0),
    }
}

pub async fn storage_detail() -> Json<serde_json::Value> {
    match fetch_storage_raw().await {
        Ok(raw) => Json(serde_json::json!({ "ok": true, "data": parse_storage_detail(&raw) })),
        Err(e)  => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

pub async fn set_replication(
    Json(req): Json<SetReplicationReq>,
) -> (StatusCode, Json<serde_json::Value>) {
    if req.size < 1 || req.size > 3 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "size must be 1–3"})));
    }
    if req.min_size < 1 || req.min_size > req.size {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "min_size must be ≥1 and ≤size"})));
    }
    if req.failure_domain != "osd" && req.failure_domain != "host" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "failure_domain must be osd or host"})));
    }

    let rule_name = if req.failure_domain == "osd" { "replicated_osd" } else { "replicated_rule" };
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
            "osd", "crush", "rule", "create-replicated", rule_name, "default", fd,
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
                "osd", "pool", "set", pool, "size", &size_s, "--yes-i-really-mean-it",
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

    (StatusCode::OK, Json(serde_json::json!({"ok": true, "output": output})))
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
        "get", "configmap", "yolab-disk-status", "-n", "rook-ceph",
        "-o", "jsonpath={.data}",
    ])
    .await
    .ok();

    let mut found: Option<(String, String)> = None;
    if let Some(map) = status.as_ref().and_then(|v| v.as_object()) {
        for (node, payload) in map {
            let Some(s) = payload.as_str() else { continue };
            let Ok(p) = serde_json::from_str::<serde_json::Value>(s) else { continue };
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
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "ok": false, "error": format!("osd.{id} not found in disk inventory")
        })));
    };

    let key = format!("{node}--{disk_id}");
    let patch = serde_json::json!({"data": {key: desired}}).to_string();
    if kubectl::run(&[
        "patch", "configmap", "yolab-disk-config", "-n", "rook-ceph", "--type", "merge", "-p", &patch,
    ]).await.is_err()
    {
        let _ = kubectl::run(&["create", "configmap", "yolab-disk-config", "-n", "rook-ceph"]).await;
        if kubectl::run(&[
            "patch", "configmap", "yolab-disk-config", "-n", "rook-ceph", "--type", "merge", "-p", &patch,
        ]).await.is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "ok": false, "error": "failed to save disk config"
            })));
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

pub async fn dashboard_creds() -> Json<serde_json::Value> {
    let password = kubectl::run(&[
        "get", "secret", "-n", "rook-ceph", "rook-ceph-dashboard-password",
        "-o", "go-template={{.data.password | base64decode}}",
    ]).await.unwrap_or_default();

    let username = kubectl::run(&[
        "get", "secret", "-n", "rook-ceph", "rook-ceph-dashboard-password",
        "-o", "go-template={{.data.username | base64decode}}",
    ]).await.unwrap_or_else(|_| "admin".into());

    let username = if username.trim().is_empty() { "admin".into() } else { username.trim().to_string() };

    Json(serde_json::json!({
        "username": username,
        "password": password.trim(),
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
        let issue = translate_health_check("POOL_NO_REDUNDANCY", &warn()).unwrap();
        assert_eq!(issue.title, "No disk redundancy");
        assert!(issue.description.contains("no backup copy"));
        assert_eq!(issue.level, HealthLevel::Warn);
    }

    #[test]
    fn health_check_carries_cephs_severity_through() {
        assert_eq!(
            translate_health_check("OSD_DOWN", &err()).unwrap().level,
            HealthLevel::Error
        );
        assert_eq!(
            translate_health_check("OSD_DOWN", &warn()).unwrap().level,
            HealthLevel::Warn
        );
    }

    #[test]
    fn health_check_defaults_to_warn_when_severity_is_missing() {
        let issue = translate_health_check("OSD_DOWN", &json!({})).unwrap();
        assert_eq!(issue.level, HealthLevel::Warn);
    }

    /// Ceph reports unavailable placement groups as HEALTH_WARN, but apps reading
    /// or writing affected files hang outright. Presenting that as a yellow
    /// warning would tell the user "minor issue" while their apps are frozen.
    #[test]
    fn unavailable_data_is_always_an_error_even_when_ceph_calls_it_a_warning() {
        for code in ["PG_DOWN", "PG_AVAILABILITY"] {
            let issue = translate_health_check(code, &warn()).unwrap();
            assert_eq!(issue.level, HealthLevel::Error, "{code}");
            assert_eq!(issue.title, "Some data temporarily unavailable");
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
                translate_health_check(code, &warn()).is_none(),
                "{code} should not be shown to the user"
            );
            // Not even when Ceph escalates them.
            assert!(translate_health_check(code, &err()).is_none(), "{code} (err)");
        }
    }

    #[test]
    fn an_unknown_code_falls_back_to_cephs_own_summary() {
        let detail = json!({
            "severity": "HEALTH_WARN",
            "summary": {"message": "BLUEFS_SPILLOVER: 1 OSD(s) experiencing spillover"},
        });
        let issue = translate_health_check("BLUEFS_SPILLOVER", &detail).unwrap();
        // Title takes the part before the first colon; the body keeps the whole line.
        assert_eq!(issue.title, "Storage issue: BLUEFS_SPILLOVER");
        assert_eq!(issue.description, "BLUEFS_SPILLOVER: 1 OSD(s) experiencing spillover");
    }

    #[test]
    fn an_unknown_code_with_no_summary_still_names_itself() {
        let issue = translate_health_check("SOMETHING_NEW", &json!({})).unwrap();
        assert_eq!(issue.title, "Storage issue: SOMETHING_NEW");
        assert_eq!(issue.description, "SOMETHING_NEW");
    }

    /// A blank title renders as an empty row in the UI — worse than raw jargon,
    /// because it looks like a rendering bug rather than a storage problem.
    #[test]
    fn every_translated_code_produces_non_empty_text() {
        let codes = [
            "POOL_NO_REDUNDANCY", "MDS_ALL_DOWN", "MDS_DAMAGE", "MDS_SLOW_METADATA_IO",
            "MDS_SLOW_REQUEST", "OSD_DOWN", "OSD_NEARFULL", "OSD_FULL", "NOSPC",
            "MON_DOWN", "MON_DISK_LOW", "MON_DISK_CRIT", "MON_DISK_BIG", "MON_CLOCK_SKEW",
            "PG_DEGRADED", "PG_DOWN", "PG_AVAILABILITY", "SLOW_OPS", "OBJECT_UNFOUND",
            "RECENT_CRASH", "POOL_TOTAL_SIZE_MIN_SIZE_REACHED",
        ];
        for code in codes {
            let issue = translate_health_check(code, &warn())
                .unwrap_or_else(|| panic!("{code} should be surfaced, not suppressed"));
            assert!(!issue.title.trim().is_empty(), "{code} has a blank title");
            assert!(!issue.description.trim().is_empty(), "{code} has a blank description");
            assert!(
                !issue.title.contains(code),
                "{code} fell through to the untranslated branch"
            );
        }
    }

    #[test]
    fn a_full_disk_reads_the_same_whichever_code_ceph_uses() {
        let a = translate_health_check("OSD_FULL", &err()).unwrap();
        let b = translate_health_check("NOSPC", &err()).unwrap();
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
        let pool = d.pools.iter().find(|p| p.name == "yolab-blockpool").unwrap();
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
