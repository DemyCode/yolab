use axum::{extract::State, Json};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct RebuildLog {
    pub running: bool,
    pub log: Vec<String>,
}

pub async fn rebuild_log(State(state): State<AppState>) -> Json<RebuildLog> {
    let cfg = &state.config;
    let running = cfg.rebuild_pid.exists() && {
        std::fs::read_to_string(&cfg.rebuild_pid)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
            .unwrap_or(false)
    };
    let log = std::fs::read_to_string(&cfg.rebuild_log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    Json(RebuildLog { running, log })
}
