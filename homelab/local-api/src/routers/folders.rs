use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::error::Result;
use crate::folders::{self, Folder, FolderBackup, Removal};
use crate::AppState;

const DEFAULT_SIZE_GIB: u64 = 1024;

#[derive(Deserialize)]
pub struct CreateFolder {
    pub title: String,
    #[serde(default)]
    pub size_gib: Option<u64>,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<Folder>>> {
    let client = state.kube.client().await?;
    let shared = folders::list(&client)
        .await?
        .into_iter()
        .filter(|folder| folder.kind != folders::KIND_STATE)
        .collect();
    Ok(Json(shared))
}

pub async fn create(State(state): State<AppState>, Json(body): Json<CreateFolder>) -> Response {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    let size = body.size_gib.unwrap_or(DEFAULT_SIZE_GIB);
    if folders::name_from_title(&body.title).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "a folder needs a name with at least one letter or number",
        )
            .into_response();
    }
    if !(1..=folders::MAX_SIZE_GIB).contains(&size) {
        return (
            StatusCode::BAD_REQUEST,
            format!("a folder holds between 1 and {} GiB", folders::MAX_SIZE_GIB),
        )
            .into_response();
    }
    match folders::create(&client, &body.title, size).await {
        Ok(name) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "name": name })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub async fn remove(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !folders::is_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            format!("{name:?} is not a folder name"),
        )
            .into_response();
    }
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    match folders::remove(&client, &name).await {
        Ok(Removal::Removed) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Removal::InUse(apps)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": in_use_message(&apps),
                "used_by": apps,
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

type Backend = crate::routers::backup_common::Backend;
type Refusal = (StatusCode, String);

async fn backend(state: &AppState) -> std::result::Result<Backend, Refusal> {
    state
        .backend()
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))
}

async fn must_exist(client: &kube::Client, name: &str) -> std::result::Result<(), Refusal> {
    if !folders::is_name(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{name:?} is not a folder name"),
        ));
    }
    folders::owner_claim_of(client, name)
        .await
        .map(|_| ())
        .map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))
}

#[derive(Deserialize)]
pub struct SetFolderBackup {
    pub enabled: bool,
    pub schedule: String,
}

pub async fn set_backup(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<SetFolderBackup>,
) -> Response {
    let client = match state.kube.client().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    };
    if let Err((code, why)) = must_exist(&client, &name).await {
        return (code, why).into_response();
    }
    if let Err(e) = crate::cron::Cron::parse(&body.schedule) {
        return (
            StatusCode::BAD_REQUEST,
            format!("that schedule is not valid: {e}"),
        )
            .into_response();
    }
    let backup = FolderBackup {
        enabled: body.enabled,
        schedule: body.schedule,
    };
    match folders::set_backup(&client, &name, &backup).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub async fn run_now(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let b = match backend(&state).await {
        Ok(b) => b,
        Err((code, why)) => return (code, why).into_response(),
    };
    if let Err((code, why)) = must_exist(&b.kube, &name).await {
        return (code, why).into_response();
    }
    match crate::routers::backup::start_folder(&b, &name, "manual").await {
        Ok(id) => {
            Json(serde_json::json!({ "ok": true, "started": true, "name": id })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub async fn points(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let b = match backend(&state).await {
        Ok(b) => b,
        Err((code, why)) => return (code, why).into_response(),
    };
    if let Err((code, why)) = must_exist(&b.kube, &name).await {
        return (code, why).into_response();
    }
    match crate::routers::restore::folder_points(&b, &name).await {
        Ok(points) => Json(serde_json::json!({ "points": points })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct RestoreFolder {
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

pub async fn restore(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RestoreFolder>,
) -> Response {
    let b = match backend(&state).await {
        Ok(b) => b,
        Err((code, why)) => return (code, why).into_response(),
    };
    if let Err((code, why)) = must_exist(&b.kube, &name).await {
        return (code, why).into_response();
    }
    let point = match &body.snapshot_id {
        Some(id) => match crate::routers::restore::folder_points(&b, &name).await {
            Ok(points) => points.into_iter().find(|p| &p.snapshot_id == id),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        },
        None => None,
    };
    if body.snapshot_id.is_some() && point.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "that backup is no longer available".to_string(),
        )
            .into_response();
    }
    let when = point
        .as_ref()
        .map(|p| date_of(&p.time))
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    let dest = match folders::create_restored(&b.kube, &name, &when).await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    let snapshot = body.snapshot_id;
    let source = name;
    let target = dest.clone();
    tokio::spawn(async move {
        if let Err(e) =
            crate::routers::restore::restore_folder(&b, &source, &target, snapshot.as_deref()).await
        {
            tracing::warn!("restore folder {source} into {target}: {e:#}");
        }
    });
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "ok": true, "name": dest, "restoring": true })),
    )
        .into_response()
}

fn date_of(time: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(time)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| time.to_string())
}

fn in_use_message(apps: &[String]) -> String {
    format!(
        "{} still {} this folder — change {} settings or remove {} first",
        apps.join(", "),
        if apps.len() == 1 { "uses" } else { "use" },
        if apps.len() == 1 { "its" } else { "their" },
        if apps.len() == 1 { "it" } else { "them" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_in_use_says_which_apps_to_change_first() {
        assert_eq!(
            in_use_message(&["jellyfin".into()]),
            "jellyfin still uses this folder — change its settings or remove it first"
        );
        assert_eq!(
            in_use_message(&["jellyfin".into(), "sonarr".into()]),
            "jellyfin, sonarr still use this folder — change their settings or remove them first"
        );
    }
}
