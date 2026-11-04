use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::host::RealHost;
use crate::notify::Tunnel;
use crate::storage::settings;
use crate::vdisk::{platform, record, record::VdiskRecord};
use crate::AppState;

pub const BOX_TYPES: [&str; 4] = ["bx11", "bx21", "bx31", "bx41"];

#[derive(Serialize, Debug, PartialEq)]
pub struct VdiskInfo {
    pub id: i64,
    pub disk_id: String,
    pub node: String,
    pub box_type: String,
    pub size_gb: u64,
    pub ready: bool,
    pub removing: bool,
    pub state: &'static str,
    pub status: String,
}

#[derive(Deserialize)]
pub struct AddVdisk {
    pub node: String,
    pub box_type: String,
}

fn info(rec: &VdiskRecord, status: Option<&String>) -> VdiskInfo {
    VdiskInfo {
        id: rec.id,
        disk_id: rec.disk_id(),
        node: rec.node.clone(),
        box_type: rec.box_type.clone(),
        size_gb: rec.size_gb,
        ready: rec.is_provisioned(),
        removing: rec.removing,
        state: state_of(rec, status.map(String::as_str)),
        status: status.cloned().unwrap_or_default(),
    }
}

pub(crate) fn state_of(rec: &VdiskRecord, status: Option<&str>) -> &'static str {
    use crate::vdisk::{CONNECTED, DRAINING, OFFLINE, PREPARING};
    match status {
        _ if rec.removing => "removing",
        _ if !rec.is_provisioned() => "provisioning",
        Some(s) if s == OFFLINE => "offline",
        Some(s) if s == CONNECTED => "connected",
        Some(s) if s == PREPARING => "provisioning",
        Some(s) if s == DRAINING => "removing",
        Some(s) if !s.is_empty() => "error",
        _ => "connecting",
    }
}

pub(crate) fn refuse_add(box_type: &str, node: &str, known_nodes: &[String]) -> Option<String> {
    if !BOX_TYPES.contains(&box_type) {
        return Some(format!("box_type must be one of {}", BOX_TYPES.join(", ")));
    }
    if !known_nodes.iter().any(|n| n == node) {
        return Some(format!(
            "{node:?} is not a machine that stores data in this cluster"
        ));
    }
    None
}

pub async fn list_vdisks(State(_s): State<AppState>) -> Result<Json<Vec<VdiskInfo>>> {
    let host = RealHost;
    let records = record::list(&host).await?;
    let statuses = record::statuses(&host).await?;
    let mut out: Vec<VdiskInfo> = records
        .iter()
        .map(|r| info(r, statuses.get(&r.id.to_string())))
        .collect();
    out.sort_by_key(|v| v.id);
    Ok(Json(out))
}

pub async fn add_vdisk(
    State(s): State<AppState>,
    Json(body): Json<AddVdisk>,
) -> std::result::Result<Json<VdiskInfo>, (StatusCode, Json<serde_json::Value>)> {
    let bad = |code: StatusCode, msg: String| (code, Json(serde_json::json!({ "error": msg })));
    let known: Vec<String> = settings::dump(&RealHost, settings::DISK_STATUS)
        .await
        .map_err(|e| {
            bad(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("the storage cluster did not answer: {e}"),
            )
        })?
        .into_keys()
        .collect();
    if let Some(why) = refuse_add(&body.box_type, &body.node, &known) {
        return Err(bad(StatusCode::BAD_REQUEST, why));
    }
    let tunnel = Tunnel::read(&s.config.config_path)
        .map_err(|e| bad(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let volume = platform::create(&crate::http::client(), &tunnel, &body.box_type)
        .await
        .map_err(|e| bad(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let rec = VdiskRecord {
        id: volume.id,
        node: body.node.clone(),
        box_type: volume.box_type.clone(),
        size_gb: volume.size_gb,
        host: if volume.is_ready() {
            volume.host.clone()
        } else {
            String::new()
        },
        username: if volume.is_ready() {
            volume.username.clone()
        } else {
            String::new()
        },
        password: if volume.is_ready() {
            volume.password.clone()
        } else {
            String::new()
        },
        removing: false,
        osd_id: None,
    };
    record::save(&RealHost, &rec).await.map_err(|e| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the Storage Box {} was bought but could not be recorded in the cluster ({e}) — \
                 delete it from the YoLab console so it is not billed",
                volume.id
            ),
        )
    })?;
    crate::routers::disks::record_switch(&rec.node, &rec.disk_id(), "ON")
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    crate::runtime::wake(crate::vdisk::NAME);
    Ok(Json(info(&rec, None)))
}

pub async fn remove_vdisk(
    State(_s): State<AppState>,
    Path(id): Path<i64>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let bad = |code: StatusCode, msg: String| (code, Json(serde_json::json!({ "error": msg })));
    let host = RealHost;
    let mut rec = record::get(&host, id)
        .await
        .map_err(|e| bad(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, format!("no virtual disk {id}")))?;
    rec.removing = true;
    record::save(&host, &rec)
        .await
        .map_err(|e| bad(StatusCode::SERVICE_UNAVAILABLE, e.to_string()))?;
    crate::routers::disks::record_switch(&rec.node, &rec.disk_id(), "OFF")
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    crate::runtime::wake(crate::vdisk::NAME);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_sizes_on_known_machines_can_be_bought() {
        let nodes = vec!["node1".to_string()];
        assert_eq!(refuse_add("bx11", "node1", &nodes), None);
        assert!(refuse_add("bx99", "node1", &nodes).is_some());
        assert!(refuse_add("bx11", "node7", &nodes).is_some());
    }

    #[test]
    fn the_page_gets_a_state_from_the_controllers_own_messages() {
        let mut rec = VdiskRecord {
            id: 2,
            node: "node1".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: String::new(),
            username: String::new(),
            password: String::new(),
            removing: false,
            osd_id: None,
        };
        assert_eq!(state_of(&rec, None), "provisioning");
        rec.host = "h".into();
        rec.username = "u".into();
        rec.password = "p".into();
        assert_eq!(state_of(&rec, None), "connecting");
        assert_eq!(state_of(&rec, Some(crate::vdisk::OFFLINE)), "offline");
        assert_eq!(state_of(&rec, Some(crate::vdisk::CONNECTED)), "connected");
        assert_eq!(
            state_of(&rec, Some("Could not connect yet, retrying: x")),
            "error"
        );
        rec.removing = true;
        assert_eq!(state_of(&rec, Some(crate::vdisk::OFFLINE)), "removing");
    }

    #[test]
    fn the_listing_never_carries_the_box_password() {
        let rec = VdiskRecord {
            id: 2,
            node: "node1".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: "u2.your-storagebox.de".into(),
            username: "u2".into(),
            password: "hunter2".into(),
            removing: false,
            osd_id: None,
        };
        let json = serde_json::to_string(&info(&rec, Some(&"Connected".to_string()))).unwrap();
        assert!(!json.contains("hunter2"));
        assert!(!json.contains("u2.your-storagebox.de"));
        assert!(json.contains("\"disk_id\":\"vdisk-2\""));
    }
}
