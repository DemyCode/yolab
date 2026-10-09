use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::error::Result;
use crate::folders::{self, Folder, Removal};
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
    Ok(Json(folders::list(&client).await?))
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
        return (StatusCode::BAD_REQUEST, format!("{name:?} is not a folder name")).into_response();
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
