use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;


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
    pub pg_unavailable: bool,
    pub mon_quorum_ok: bool,
    pub osd_full: bool,
    pub starting: bool,
    pub provisioning: bool,
    pub storage_unrecoverable: bool,
}

fn system_uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(u64::MAX)
}

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

    let places = match crate::topology::observe().await {
        Some(t) if t.osd_hosts > 1 => t.osd_hosts,
        Some(t) => t.osds,
        None => 0,
    };

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

    let pg_unavailable = details
        .as_object()
        .is_some_and(|obj| obj.contains_key("PG_AVAILABILITY") || obj.contains_key("PG_DOWN"));
    let osd_full = details.as_object().is_some_and(|obj| {
        obj.contains_key("OSD_FULL") || obj.contains_key("NOSPC") || obj.contains_key("POOL_FULL")
    });
    let starting = pg_unavailable && system_uptime_secs() < 900;
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PgLoss {
    pub stuck: u32,
    pub total: u32,
    pub unrecoverable: bool,
    pub unrecoverable_pools: Vec<String>,
    pub confirmed_lost: bool,
    pub confirmed_lost_pools: Vec<String>,
}

fn is_stuck_state(state: &str) -> bool {
    state
        .split('+')
        .any(|s| matches!(s, "stale" | "down" | "incomplete" | "unknown"))
}

fn is_incomplete_state(state: &str) -> bool {
    state.split('+').any(|s| s == "incomplete")
}

fn osds_still_in(dump: &Value) -> std::collections::HashSet<i64> {
    dump["osds"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|o| o["in"].as_i64().unwrap_or(0) == 1)
                .filter_map(|o| o["osd"].as_i64())
                .collect()
        })
        .unwrap_or_default()
}

fn pg_is_confirmed_lost(pg: &Value, still_in: &std::collections::HashSet<i64>) -> bool {
    let state = pg["state"].as_str().unwrap_or("");
    if is_incomplete_state(state) {
        return true;
    }
    let acting = pg["acting"]
        .as_array()
        .filter(|a| !a.is_empty())
        .or_else(|| pg["up"].as_array().filter(|a| !a.is_empty()));
    match acting {
        Some(ids) => !ids
            .iter()
            .filter_map(|v| v.as_i64())
            .any(|id| still_in.contains(&id)),
        None => true,
    }
}

pub(crate) async fn assess_pg_loss() -> Option<PgLoss> {
    let dump = crate::ceph_cli::ceph_json(&["osd", "dump"]).await.ok()?;
    let pgs = crate::ceph_cli::ceph_json(&["pg", "dump", "pgs_brief"])
        .await
        .ok()?;
    compute_pg_loss(&dump, &pgs)
}

pub(crate) async fn assess_pg_loss_via<H: crate::host::Host>(host: &H) -> Option<PgLoss> {
    let dump = host.ceph_json(&["osd", "dump"]).await.ok()?;
    let pgs = host.ceph_json(&["pg", "dump", "pgs_brief"]).await.ok()?;
    compute_pg_loss(&dump, &pgs)
}

fn compute_pg_loss(dump: &Value, pgs: &Value) -> Option<PgLoss> {
    let pools: std::collections::HashMap<i64, (String, u64)> = dump["pools"]
        .as_array()?
        .iter()
        .filter_map(|p| {
            Some((
                p["pool"].as_i64()?,
                (
                    p["pool_name"].as_str().unwrap_or("").to_string(),
                    p["size"].as_u64()?,
                ),
            ))
        })
        .collect();

    let items = pgs["pg_stats"]
        .as_array()
        .or_else(|| pgs.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return None;
    }

    let still_in = osds_still_in(dump);
    let mut stuck = 0u32;
    let mut unrecoverable = false;
    let mut unrecoverable_pools: Vec<String> = Vec::new();
    let mut confirmed_lost = false;
    let mut confirmed_lost_pools: Vec<String> = Vec::new();
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
        let pool = pool_id.and_then(|id| pools.get(&id));
        let single_copy = pool.is_some_and(|(_, size)| *size <= 1);
        if single_copy || is_incomplete_state(state) {
            unrecoverable = true;
            if let Some((name, _)) = pool {
                if !name.is_empty() && !unrecoverable_pools.iter().any(|p| p == name) {
                    unrecoverable_pools.push(name.clone());
                }
            }
            if pg_is_confirmed_lost(pg, &still_in) {
                confirmed_lost = true;
                if let Some((name, _)) = pool {
                    if !name.is_empty() && !confirmed_lost_pools.iter().any(|p| p == name) {
                        confirmed_lost_pools.push(name.clone());
                    }
                }
            }
        }
    }

    Some(PgLoss {
        stuck,
        total: items.len() as u32,
        unrecoverable,
        unrecoverable_pools,
        confirmed_lost,
        confirmed_lost_pools,
    })
}

pub(crate) fn unavailable_message(loss: Option<&PgLoss>) -> (String, String) {
    let share = |l: &PgLoss| {
        if l.total > 0 {
            format!("{} of {} groups of your files", l.stuck, l.total)
        } else {
            "Some of your files".to_string()
        }
    };
    match loss {
        Some(l) if l.confirmed_lost => (
            "Your files are unreachable and cannot be rebuilt".into(),
            format!(
                "{} were stored in one place only, on a disk Ceph has now given up on \
                 — so there is no second copy to rebuild them from and nothing is being \
                 repaired. If that disk still works, reconnecting it brings everything \
                 back. If it does not, only a backup can. Apps that use these files will \
                 not start or will hang.",
                share(l)
            ),
        ),
        Some(l) if l.unrecoverable => (
            "Some files are unreachable right now".into(),
            format!(
                "{} are on a disk whose storage service is not running. The disk is still \
                 part of the cluster and the data on it is intact, so this is not lost — \
                 it comes back when that service starts again. Apps that use these files \
                 may hang until then. There is no second copy, so nothing can serve them \
                 in the meantime.",
                share(l)
            ),
        ),
        _ => (
            "Some files are unreachable right now".into(),
            "A disk is not responding. Other copies exist, so this repairs itself — apps \
             touching the affected files may hang until it finishes."
                .into(),
        ),
    }
}

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
        "PG_DEGRADED" | "PG_UNDERSIZED" => (
            "Fewer copies than you asked for".into(),
            "Some of your files have fewer copies than your storage settings ask for — \
             there are not enough disks to hold them all right now. Everything still \
             works and nothing has been lost. Add a disk and the missing copies are \
             made automatically."
                .into(),
        ),
        "PG_DOWN" | "PG_AVAILABILITY" => {
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

async fn ceph_health_and_details() -> anyhow::Result<String> {
    let h = crate::ceph_cli::ceph_json(&["health", "detail"]).await?;
    let status = h["status"].as_str().unwrap_or("");
    let checks = h.get("checks").cloned().unwrap_or(serde_json::json!({}));
    Ok(format!("{status}\n{checks}"))
}


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
    pub crush_weight: f64,
    pub reweight: f64,
    pub safe_to_destroy: bool,
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
        if crate::ceph::destructive::safe_to_destroy(&crate::host::RealHost, id)
            .await
            .is_ok_and(|p| p.is_some())
        {
            safe_to_destroy.push(id);
        }
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
    let safe_ids: std::collections::HashSet<i64> = v["safe_to_destroy"]["safe_to_destroy"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    let ok_to_stop_ids: std::collections::HashSet<i64> = v["ok_to_stop"]["ok_to_stop"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

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

    let df_pools = v["ceph_df"]["pools"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let df_by_id: HashMap<u64, &serde_json::Value> = df_pools
        .iter()
        .filter_map(|p| p["id"].as_u64().map(|id| (id, p)))
        .collect();

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


pub async fn osd_mark_in(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    set_desired_by_osd(id, "ON").await
}

pub async fn osd_mark_out(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    set_desired_by_osd(id, "OFF").await
}

fn disk_of_osd(
    published: &std::collections::BTreeMap<String, String>,
    osd_id: i64,
) -> Option<(String, String)> {
    published.iter().find_map(|(node, raw)| {
        let payload: Value = serde_json::from_str(raw).ok()?;
        payload["disks"]
            .as_object()?
            .iter()
            .find(|(_, meta)| meta["osd_id"].as_i64() == Some(osd_id))
            .map(|(disk_id, _)| (node.clone(), disk_id.clone()))
    })
}

async fn set_desired_by_osd(id: i64, desired: &str) -> (StatusCode, Json<serde_json::Value>) {
    use crate::storage::settings;
    let published = match settings::dump(&crate::host::RealHost, settings::DISK_STATUS).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    serde_json::json!({"ok": false, "error": format!("cannot read the disk inventory: {e}")}),
                ),
            )
        }
    };
    let Some((node, disk_id)) = disk_of_osd(&published, id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"ok": false, "error": format!("osd.{id} is not in any node's disk inventory")}),
            ),
        );
    };
    match crate::routers::disks::record_switch(&node, &disk_id, desired).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": error})),
        ),
    }
}

#[cfg(test)]
mod osd_switch_tests {
    use super::*;

    #[test]
    fn an_osd_is_found_in_whichever_node_publishes_it() {
        let published = std::collections::BTreeMap::from([
            (
                "node1".to_string(),
                r#"{"disks":{"dev-sda":{"osd_id":0}}}"#.to_string(),
            ),
            (
                "node2".to_string(),
                r#"{"disks":{"serial-wwn-0x1":{"osd_id":3}}}"#.to_string(),
            ),
            ("node3".to_string(), "not json".to_string()),
        ]);
        assert_eq!(
            disk_of_osd(&published, 3),
            Some(("node2".to_string(), "serial-wwn-0x1".to_string()))
        );
        assert_eq!(disk_of_osd(&published, 9), None);
    }
}

const DASHBOARD_PASSWORD_FILE: &str = "/var/lib/ceph/dashboard-password";

pub async fn dashboard_creds() -> Json<serde_json::Value> {
    let password = std::fs::read_to_string(DASHBOARD_PASSWORD_FILE)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    Json(serde_json::json!({
        "username": "admin",
        "password": password,
        "ready": !password.is_empty(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;


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


    fn loss(stuck: u32, total: u32, unrecoverable: bool) -> PgLoss {
        PgLoss {
            stuck,
            total,
            unrecoverable,
            unrecoverable_pools: if unrecoverable {
                vec!["yolab-fs-metadata".into()]
            } else {
                vec![]
            },
            confirmed_lost: unrecoverable,
            confirmed_lost_pools: if unrecoverable {
                vec!["yolab-fs-metadata".into()]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn a_stale_pg_at_one_copy_is_never_called_temporary() {
        let (title, body) = unavailable_message(Some(&loss(63, 81, true)));
        assert!(title.contains("cannot be rebuilt"), "{title}");
        assert!(!title.to_lowercase().contains("temporar"), "{title}");
        assert!(!body.to_lowercase().contains("until recovery"), "{body}");
        assert!(body.contains("reconnecting it"), "{body}");
        assert!(body.contains("backup"), "{body}");
        assert!(body.contains("63 of 81"), "{body}");
        assert!(body.contains("will not start or will hang"), "{body}");
    }

    #[test]
    fn a_replicated_cluster_is_still_told_to_wait() {
        let (title, body) = unavailable_message(Some(&loss(5, 81, false)));
        assert!(body.contains("repairs itself"), "{body}");
        assert!(!body.contains("backup"), "{body}");
        assert!(!title.contains("cannot be rebuilt"), "{title}");
    }

    #[test]
    fn an_unreadable_assessment_takes_the_cautious_branch() {
        let (title, body) = unavailable_message(None);
        assert!(!title.contains("cannot be rebuilt"), "{title}");
        assert!(body.contains("repairs itself"), "{body}");
    }

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


    fn pg_dump(pgid: &str, state: &str) -> Value {
        json!({ "pg_stats": [{ "pgid": pgid, "state": state }] })
    }

    fn osd_dump(pools: Value) -> Value {
        json!({ "pools": pools })
    }

    fn pool(id: i64, name: &str, size: u64) -> Value {
        json!({ "pool": id, "pool_name": name, "size": size })
    }


    fn osd(id: i64, is_in: i64) -> Value {
        json!({ "osd": id, "in": is_in, "up": 0, "weight": 1.0 })
    }

    fn dump_with_osds(pools: Value, osds: Value) -> Value {
        json!({ "pools": pools, "osds": osds })
    }

    fn pg_acting(pgid: &str, state: &str, acting: Value) -> Value {
        json!({ "pg_stats": [{ "pgid": pgid, "state": state, "acting": acting, "up": acting }] })
    }

    #[test]
    fn a_stale_pg_whose_osd_is_still_in_is_not_confirmed_lost() {
        let dump = dump_with_osds(
            json!([pool(2, "yolab-fs-metadata", 1)]),
            json!([osd(0, 1), osd(1, 1), osd(2, 1)]),
        );
        let loss =
            compute_pg_loss(&dump, &pg_acting("2.f", "stale+active+clean", json!([1]))).unwrap();

        assert_eq!(loss.stuck, 1);
        assert!(
            loss.unrecoverable,
            "single copy and unreadable: the purge gate must still refuse"
        );
        assert!(
            !loss.confirmed_lost,
            "osd.1 is still in — the daemon is down, the data is not gone"
        );
        assert!(loss.confirmed_lost_pools.is_empty());
    }

    #[test]
    fn a_stale_pg_whose_osd_is_out_is_confirmed_lost() {
        let dump = dump_with_osds(
            json!([pool(2, "yolab-fs-metadata", 1)]),
            json!([osd(0, 1), osd(1, 0)]),
        );
        let loss =
            compute_pg_loss(&dump, &pg_acting("2.f", "stale+active+clean", json!([1]))).unwrap();

        assert!(loss.confirmed_lost, "osd.1 is out: Ceph has written it off");
        assert_eq!(
            loss.confirmed_lost_pools,
            vec!["yolab-fs-metadata".to_string()]
        );
    }

    #[test]
    fn an_incomplete_pg_is_confirmed_lost_even_with_osds_still_in() {
        let dump = dump_with_osds(
            json!([pool(3, "yolab-fs-data0", 2)]),
            json!([osd(0, 1), osd(1, 1)]),
        );
        let loss = compute_pg_loss(&dump, &pg_acting("3.f", "incomplete", json!([0, 1]))).unwrap();
        assert!(loss.confirmed_lost);
    }

    #[test]
    fn a_pg_with_no_acting_osds_is_confirmed_lost() {
        let dump = dump_with_osds(json!([pool(2, "yolab-fs-metadata", 1)]), json!([osd(0, 1)]));
        let loss = compute_pg_loss(&dump, &pg_acting("2.f", "stale", json!([]))).unwrap();
        assert!(loss.confirmed_lost);
    }

    #[test]
    fn the_purge_gate_still_refuses_while_a_daemon_is_only_down() {
        let dump = dump_with_osds(
            json!([pool(2, "yolab-fs-metadata", 1)]),
            json!([osd(0, 1), osd(1, 1)]),
        );
        let loss =
            compute_pg_loss(&dump, &pg_acting("2.f", "stale+active+clean", json!([1]))).unwrap();
        assert!(
            loss.unrecoverable && loss.stuck > 0,
            "this is the pair plan_purge keys on to refuse"
        );
    }

    #[test]
    fn the_message_distinguishes_gone_from_not_answering() {
        let gone = PgLoss {
            stuck: 74,
            total: 81,
            unrecoverable: true,
            unrecoverable_pools: vec!["yolab-fs-data0".into()],
            confirmed_lost: true,
            confirmed_lost_pools: vec!["yolab-fs-data0".into()],
        };
        let (title, _) = unavailable_message(Some(&gone));
        assert!(title.contains("cannot be rebuilt"));

        let waiting = PgLoss {
            confirmed_lost: false,
            confirmed_lost_pools: vec![],
            ..gone.clone()
        };
        let (title, body) = unavailable_message(Some(&waiting));
        assert!(
            !title.contains("cannot be rebuilt"),
            "intact data must never be described as unrebuildable"
        );
        assert!(
            body.contains("intact"),
            "and the owner must be told not to reach for a backup: {body}"
        );
        assert!(
            !body.contains("repairs itself"),
            "there is no second copy to repair from at size 1: {body}"
        );
    }

    #[test]
    fn a_down_pg_on_a_replicated_pool_is_not_permanent() {
        let dump = osd_dump(json!([
            pool(2, "yolab-fs-metadata", 2),
            pool(3, "yolab-fs-data0", 2),
        ]));
        let loss = compute_pg_loss(&dump, &pg_dump("3.f", "down+peering")).unwrap();
        assert_eq!(loss.stuck, 1);
        assert!(!loss.unrecoverable, "a second copy still exists");
        assert!(loss.unrecoverable_pools.is_empty());
    }

    #[test]
    fn an_incomplete_pg_is_permanent_whatever_the_replica_count() {
        let dump = osd_dump(json!([
            pool(2, "yolab-fs-metadata", 2),
            pool(3, "yolab-fs-data0", 2),
        ]));
        let loss = compute_pg_loss(&dump, &pg_dump("3.f", "incomplete")).unwrap();
        assert!(loss.unrecoverable);
        assert_eq!(loss.unrecoverable_pools, vec!["yolab-fs-data0".to_string()]);
    }

    #[test]
    fn a_down_pg_on_a_single_copy_pool_is_permanent() {
        let dump = osd_dump(json!([
            pool(2, "yolab-fs-metadata", 1),
            pool(3, "yolab-fs-data0", 1),
        ]));
        let loss = compute_pg_loss(&dump, &pg_dump("2.c", "down+peering")).unwrap();
        assert!(loss.unrecoverable);
        assert_eq!(
            loss.unrecoverable_pools,
            vec!["yolab-fs-metadata".to_string()]
        );
    }

    #[test]
    fn lost_image_pool_does_not_read_as_app_data_loss() {
        let dump = osd_dump(json!([
            pool(2, "yolab-fs-metadata", 2),
            pool(3, "yolab-fs-data0", 2),
            pool(4, "images", 2),
        ]));
        let loss = compute_pg_loss(&dump, &pg_dump("4.9", "incomplete")).unwrap();
        assert!(loss.unrecoverable);
        assert_eq!(loss.unrecoverable_pools, vec!["images".to_string()]);
    }

    #[test]
    fn the_page_is_told_this_is_an_error_not_a_warning() {
        let issue =
            translate_health_check("PG_AVAILABILITY", &warn(), 3, Some(&loss(63, 81, true)))
                .unwrap();
        assert_eq!(issue.level, HealthLevel::Error);
        assert!(issue.title.contains("cannot be rebuilt"), "{}", issue.title);
    }


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

    #[test]
    fn unavailable_data_is_always_an_error_even_when_ceph_calls_it_a_warning() {
        for code in ["PG_DOWN", "PG_AVAILABILITY"] {
            let issue = translate_health_check(code, &warn(), 3, None).unwrap();
            assert_eq!(issue.level, HealthLevel::Error, "{code}");
            assert_eq!(issue.title, "Some files are unreachable right now");
        }
    }

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

    #[test]
    fn failure_domain_defaults_to_host_when_it_cannot_be_determined() {
        assert_eq!(failure_domain_from_rule(&json!({})), "host");
        assert_eq!(failure_domain_from_rule(&json!({"steps": []})), "host");
        assert_eq!(
            failure_domain_from_rule(&json!({"steps": [{"op": "emit"}]})),
            "host"
        );
        assert_eq!(
            failure_domain_from_rule(&json!({"steps": [{"op": "chooseleaf_firstn"}]})),
            "host"
        );
    }


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
        let d = parse_storage_detail(&sample_raw());
        assert_eq!(d.osds.iter().find(|o| o.id == 0).unwrap().class, "ssd");
        assert_eq!(d.osds.iter().find(|o| o.id == 1).unwrap().class, "hdd");
    }

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

    #[test]
    fn storage_detail_keeps_an_osd_with_no_parent_host() {
        let raw = json!({
            "osd_df": {"nodes": [{"type": "osd", "id": 5, "name": "osd.5"}]}
        });
        let d = parse_storage_detail(&raw);
        assert_eq!(d.osds.len(), 1);
        assert_eq!(d.osds[0].host, "unknown");
        assert_eq!(d.osds[0].status, "unknown");
        assert_eq!(d.osds[0].reweight, 1.0);
    }
}


async fn active_dashboard_origin() -> Option<String> {
    let services = crate::ceph_cli::ceph_json(&["mgr", "services"])
        .await
        .ok()?;
    dashboard_origin_from(&services)
}

pub(crate) fn dashboard_origin_from(services: &serde_json::Value) -> Option<String> {
    let url = services["dashboard"].as_str().filter(|u| !u.is_empty())?;
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port().unwrap_or(7000);
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(format!("{}://{}:{}", parsed.scheme(), host, port))
}

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

    #[test]
    fn an_ipv6_mgr_url_keeps_its_brackets() {
        let v = json!({"dashboard": "http://[fd00:cafe::30]:7000/ceph-dashboard"});
        assert_eq!(
            dashboard_origin_from(&v).as_deref(),
            Some("http://[fd00:cafe::30]:7000")
        );
    }

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

    #[test]
    fn the_scheme_is_preserved() {
        let v = json!({"dashboard": "https://[fd00:cafe::30]:8443/"});
        assert_eq!(
            dashboard_origin_from(&v).as_deref(),
            Some("https://[fd00:cafe::30]:8443")
        );
    }

    #[test]
    fn an_unusable_answer_yields_nothing() {
        assert!(dashboard_origin_from(&json!({})).is_none());
        assert!(dashboard_origin_from(&json!({"dashboard": ""})).is_none());
        assert!(dashboard_origin_from(&json!({"dashboard": "not a url"})).is_none());
        assert!(dashboard_origin_from(&json!({"prometheus": "http://x:9283/"})).is_none());
    }
}

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
