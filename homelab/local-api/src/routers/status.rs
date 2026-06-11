use axum::{extract::State, Json};
use serde::Serialize;

use crate::{error::Result, AppState};

#[derive(Serialize)]
pub struct StatusInfo {
    pub commit_hash: String,
    pub commit_message: String,
    pub commit_date: String,
    pub platform: String,
    pub flake_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn built_or_git(state: &AppState, filename: &str, args: &[&str]) -> String {
    let v = std::fs::read_to_string(state.config.built_dir.join(filename))
        .unwrap_or_default()
        .trim()
        .to_string();
    if !v.is_empty() {
        return v;
    }
    std::process::Command::new("git")
        .args(args)
        .current_dir(&state.config.repo_path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub async fn handler(State(state): State<AppState>) -> Result<Json<StatusInfo>> {
    Ok(Json(StatusInfo {
        commit_hash: built_or_git(&state, "built-hash", &["rev-parse", "HEAD"]),
        commit_message: built_or_git(&state, "built-message", &["log", "-1", "--pretty=%s"]),
        commit_date: built_or_git(&state, "built-date", &["log", "-1", "--pretty=%cI"]),
        platform: state.config.platform.clone(),
        flake_target: state.config.flake_target.clone(),
        error: None,
    }))
}
