use axum::{extract::State, Json};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct RebuildLog {
    pub running: bool,
    pub log: Vec<String>,
}

fn pid_is_running(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Err(_) => false,
        Ok(s) => !s
            .lines()
            .find(|l| l.starts_with("State:"))
            .map(|l| l.contains('Z'))
            .unwrap_or(false),
    }
}

pub async fn rebuild_log(State(state): State<AppState>) -> Json<RebuildLog> {
    let cfg = &state.config;
    let running = cfg.rebuild_pid.exists() && {
        std::fs::read_to_string(&cfg.rebuild_pid)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(pid_is_running)
            .unwrap_or(false)
    };
    let log = std::fs::read_to_string(&cfg.rebuild_log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    Json(RebuildLog { running, log })
}
