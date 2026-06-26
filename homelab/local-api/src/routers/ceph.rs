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
}

pub async fn cluster_health() -> Json<ClusterHealth> {
    Json(compute_cluster_health().await)
}

async fn compute_cluster_health() -> ClusterHealth {
    let raw = match kubectl::run(&[
        "get", "cephcluster", "-n", "rook-ceph", "rook-ceph",
        "-o", "jsonpath={.status.ceph.health}{\"\\n\"}{.status.ceph.details}",
    ]).await {
        Ok(s) => s,
        Err(_) => {
            return ClusterHealth {
                level: HealthLevel::Error,
                title: "Storage cluster unreachable".into(),
                message: "Cannot connect to the storage control plane. If a machine just restarted, wait 2–3 minutes.".into(),
                issues: vec![],
                pg_unavailable: false,
                mon_quorum_ok: false,
                osd_full: false,
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

    match level {
        HealthLevel::Ok => ClusterHealth {
            level: HealthLevel::Ok,
            title: "All systems healthy".into(),
            message: "Your storage cluster is running normally.".into(),
            issues,
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
        },
        HealthLevel::Warn => ClusterHealth {
            level: HealthLevel::Warn,
            title: "Storage has warnings".into(),
            message: "Your cluster is operational but has non-critical issues.".into(),
            issues,
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
        },
        HealthLevel::Error => ClusterHealth {
            level: HealthLevel::Error,
            title: "Storage cluster has critical errors".into(),
            message: "One or more critical problems affect your storage. Apps may be unable to read or write data.".into(),
            issues,
            pg_unavailable,
            mon_quorum_ok: true,
            osd_full,
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
        "MON_CLOCK_SKEW" => (
            "Machine clocks out of sync".into(),
            "The clocks on your machines differ by too much. This can cause storage failures.".into(),
        ),
        "PG_DEGRADED" => (
            "Data redundancy reduced".into(),
            "Some data chunks are stored on fewer disks than configured. Your cluster is recovering.".into(),
        ),
        "PG_DOWN" | "PG_AVAILABILITY" => (
            "Some data temporarily unavailable".into(),
            "Certain data is unreachable right now. Apps reading or writing to affected files will hang until recovery.".into(),
        ),
        "SLOW_OPS" => (
            "Storage operations are slow".into(),
            "Some storage operations are taking longer than expected. Apps may respond slowly.".into(),
        ),
        "OBJECT_UNFOUND" => (
            "Missing data objects".into(),
            "Some data objects cannot be found on any disk. This is a sign of past data loss.".into(),
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

pub async fn cluster_status_from_k8s()
-> anyhow::Result<(serde_json::Map<String, serde_json::Value>, u32, u32)> {
    let status_str = kubectl::run(&[
        "get", "cephcluster", "-n", "rook-ceph", "rook-ceph", "-o", "jsonpath={.status}",
    ]).await?;
    let status: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&status_str).unwrap_or_default();

    let ready_str = kubectl::run(&[
        "get", "deploy", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "-o", "jsonpath={.items[*].status.readyReplicas}",
    ]).await.unwrap_or_default();

    let osd_ready: u32 = ready_str.split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .sum();

    let items_str = kubectl::run(&[
        "get", "deploy", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "-o", "jsonpath={.items}",
    ]).await.unwrap_or_default();

    let osd_total = serde_json::from_str::<serde_json::Value>(&items_str)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.len() as u32))
        .unwrap_or(0);

    Ok((status, osd_total, osd_ready))
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

async fn fetch_storage_raw() -> anyhow::Result<serde_json::Value> {
    let pod = kubectl::run(&[
        "get", "pod", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "--field-selector=status.phase=Running",
        "-o", "jsonpath={.items[0].metadata.name}",
    ]).await?;
    let pod = pod.trim().to_string();
    anyhow::ensure!(!pod.is_empty(), "no running OSD pod found");

    let mon_raw = kubectl::run(&[
        "get", "cm", "-n", "rook-ceph", "rook-ceph-mon-endpoints",
        "-o", "jsonpath={.data.data}",
    ]).await.unwrap_or_default();
    let mon_host = mon_raw.split('=').nth(1).unwrap_or("").trim().to_string();
    anyhow::ensure!(!mon_host.is_empty(), "cannot find mon endpoint");

    let kb64 = kubectl::run(&[
        "get", "secret", "-n", "rook-ceph", "rook-ceph-admin-keyring",
        "-o", "jsonpath={.data.keyring}",
    ]).await.unwrap_or_default();
    let kb64 = kb64.trim().replace('\n', "");
    anyhow::ensure!(!kb64.is_empty(), "cannot read admin keyring");

    let script = [
        format!("echo '{}' | base64 -d > /tmp/.ck 2>/dev/null", kb64),
        format!("printf '[global]\\nmon_host = {}\\n' > /tmp/.cc", mon_host),
        "CEPH='ceph -c /tmp/.cc --keyring /tmp/.ck -n client.admin'".into(),
        r#"echo '{"osd_df":'  "#.into(),
        "$CEPH osd df tree -f json 2>/dev/null || echo '{}'".into(),
        r#"echo ',"pool_detail":'"#.into(),
        "$CEPH osd pool ls detail -f json 2>/dev/null || echo '[]'".into(),
        r#"echo ',"ceph_df":'"#.into(),
        "$CEPH df -f json 2>/dev/null || echo '{}'".into(),
        r#"echo ',"crush_rules":'"#.into(),
        "$CEPH osd crush rule dump -f json 2>/dev/null || echo '[]'".into(),
        "echo '}'".into(),
        "rm -f /tmp/.ck /tmp/.cc".into(),
    ].join("\n");

    let raw = kubectl::run(&["exec", &pod, "-n", "rook-ceph", "--", "bash", "-c", &script]).await?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("JSON parse: {e}\nRaw: {}", &raw[..raw.len().min(500)]))
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

    let pod = match kubectl::run(&[
        "get", "pod", "-n", "rook-ceph", "-l", "app=rook-ceph-osd",
        "--field-selector=status.phase=Running",
        "-o", "jsonpath={.items[0].metadata.name}",
    ]).await {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "no running OSD pod"}))),
    };

    let mon_raw = kubectl::run(&[
        "get", "cm", "-n", "rook-ceph", "rook-ceph-mon-endpoints",
        "-o", "jsonpath={.data.data}",
    ]).await.unwrap_or_default();
    let mon_host = mon_raw.split('=').nth(1).unwrap_or("").trim().to_string();

    let kb64 = kubectl::run(&[
        "get", "secret", "-n", "rook-ceph", "rook-ceph-admin-keyring",
        "-o", "jsonpath={.data.keyring}",
    ]).await.unwrap_or_default();
    let kb64 = kb64.trim().replace('\n', "");

    // size=1 requires --yes-i-really-mean-it; the flag is harmless for size>1
    let really = if size == 1 { " --yes-i-really-mean-it" } else { "" };

    let script = [
        format!("echo '{}' | base64 -d > /tmp/.ck 2>/dev/null", kb64),
        format!("printf '[global]\\nmon_host = {}\\n' > /tmp/.cc", mon_host),
        "CEPH='ceph -c /tmp/.cc --keyring /tmp/.ck -n client.admin'".into(),
        "set -e".into(),
        // Create OSD-level rule if needed (replicated_rule already exists for host)
        format!("if ! $CEPH osd crush rule ls 2>/dev/null | grep -qx {rule}; then $CEPH osd crush rule create-replicated {rule} default {fd}; fi", rule = rule_name, fd = fd),
        // Apply to all non-internal pools
        format!("for POOL in $($CEPH osd pool ls 2>/dev/null | grep -v '^[.]'); do"),
        format!("  $CEPH osd pool set $POOL crush_rule {rule}", rule = rule_name),
        format!("  $CEPH osd pool set $POOL size {size}{really}", size = size, really = really),
        format!("  $CEPH osd pool set $POOL min_size {min_size}", min_size = min_size),
        "  echo \"Updated pool $POOL\"".into(),
        "done".into(),
        "rm -f /tmp/.ck /tmp/.cc".into(),
    ].join("\n");

    match kubectl::run(&["exec", &pod, "-n", "rook-ceph", "--", "bash", "-c", &script]).await {
        Ok(out) => (StatusCode::OK, Json(serde_json::json!({"ok": true, "output": out}))),
        Err(e)  => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))),
    }
}

// ── OSD lifecycle ──────────────────────────────────────────────────────────────

/// Activate a weight-0 OSD: set its CRUSH weight to the physical disk size
/// and mark it `in` so Ceph starts assigning PGs to it.
pub async fn osd_activate(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    let osd = format!("osd.{id}");

    // Derive the physical size from OSD metadata (bluestore_bdev_size in bytes).
    let meta = match kubectl::ceph_exec(&["osd", "metadata", &osd, "-f", "json"]).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("metadata: {e}")}))),
    };
    let size_bytes: u64 = serde_json::from_str::<serde_json::Value>(&meta)
        .ok()
        .and_then(|v| {
            v["bluestore_bdev_size"].as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v["bluestore_bdev_size"].as_u64())
        })
        .unwrap_or(0);
    if size_bytes == 0 {
        return (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "cannot determine device size from OSD metadata"})));
    }

    // CRUSH weight unit = 1 TiB. Round to 5 decimal places.
    let weight = format!("{:.5}", size_bytes as f64 / (1u64 << 40) as f64);

    if let Err(e) = kubectl::ceph_exec(&["osd", "in", &osd]).await {
        return (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("osd in: {e}")})));
    }
    match kubectl::ceph_exec(&["osd", "crush", "reweight", &osd, &weight]).await {
        Ok(_)  => (StatusCode::OK, Json(serde_json::json!({"ok": true, "crush_weight": weight}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("crush reweight: {e}")}))),
    }
}

/// Deactivate an OSD: set CRUSH weight to 0 and mark it `out`.
/// Ceph will drain all PGs off it. The UI will show "Safe to unplug"
/// once `ceph osd safe-to-destroy` returns clean.
pub async fn osd_deactivate(Path(id): Path<i64>) -> (StatusCode, Json<serde_json::Value>) {
    let osd = format!("osd.{id}");
    if let Err(e) = kubectl::ceph_exec(&["osd", "crush", "reweight", &osd, "0"]).await {
        return (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("crush reweight: {e}")})));
    }
    match kubectl::ceph_exec(&["osd", "out", &osd]).await {
        Ok(_)  => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("osd out: {e}")}))),
    }
}

/// Background task: every 30 s scan for OSDs that are weight-0 + out + down +
/// safe-to-destroy and purge them automatically. This is the final step after
/// the user physically unplugs a deactivated disk.
pub async fn run_osd_removal_watcher() {
    // Give the cluster time to start before the first check.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    loop {
        if let Err(e) = osd_removal_tick().await {
            tracing::debug!("osd removal watcher: {e}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn osd_removal_tick() -> anyhow::Result<()> {
    let raw = kubectl::ceph_exec(&["osd", "df", "tree", "-f", "json"]).await?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;

    let nodes = v["nodes"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    for node in nodes {
        if node["type"].as_str() != Some("osd") { continue; }
        let id           = node["id"].as_i64().unwrap_or(-1);
        let crush_weight = node["crush_weight"].as_f64().unwrap_or(1.0);
        let reweight     = node["reweight"].as_f64().unwrap_or(1.0);
        let status       = node["status"].as_str().unwrap_or("up");

        // Only auto-purge OSDs the user explicitly deactivated (crush_weight=0,
        // reweight=0 via `osd out`) that are now down (disk unplugged).
        if crush_weight > 0.0 || reweight > 0.0 || status != "down" {
            continue;
        }

        let osd = format!("osd.{id}");
        match kubectl::ceph_exec(&["osd", "safe-to-destroy", &osd]).await {
            Ok(_) => {
                tracing::info!("auto-purging {osd}: weight=0, out, down, safe-to-destroy");
                // Delete OSD deployment first so Rook doesn't restart it mid-purge.
                let deploy = format!("rook-ceph-osd-{id}");
                let _ = kubectl::run(&[
                    "delete", "deploy", &deploy,
                    "-n", "rook-ceph", "--ignore-not-found=true",
                ]).await;
                if let Err(e) = kubectl::ceph_exec(&[
                    "osd", "purge", &osd, "--yes-i-really-mean-it",
                ]).await {
                    tracing::error!("purge {osd} failed: {e}");
                } else {
                    tracing::info!("{osd} purged");
                }
            }
            Err(_) => {} // not safe yet — leave it alone
        }
    }
    Ok(())
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
