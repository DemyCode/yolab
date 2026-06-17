use axum::Json;
use serde::Serialize;

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

    match level {
        HealthLevel::Ok => ClusterHealth {
            level: HealthLevel::Ok,
            title: "All systems healthy".into(),
            message: "Your storage cluster is running normally.".into(),
            issues,
        },
        HealthLevel::Warn => ClusterHealth {
            level: HealthLevel::Warn,
            title: "Storage has warnings".into(),
            message: "Your cluster is operational but has non-critical issues.".into(),
            issues,
        },
        HealthLevel::Error => ClusterHealth {
            level: HealthLevel::Error,
            title: "Storage cluster has critical errors".into(),
            message: "One or more critical problems affect your storage. Apps may be unable to read or write data.".into(),
            issues,
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
