use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Sse},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{config::Config, proc::KillOnDrop, AppState};

#[derive(Serialize, Deserialize, Clone)]
pub struct Channel {
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

impl Default for Channel {
    fn default() -> Self {
        Self { remote: "origin".into(), ref_: "main".into() }
    }
}

#[derive(Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct ChannelInfo {
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub remotes: Vec<RemoteEntry>,
}

#[derive(Deserialize)]
pub struct RemoteBody {
    pub name: String,
    pub url: String,
}

fn read_channel(cfg: &Config) -> Channel {
    std::fs::read_to_string(&cfg.channel_file)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            Some(Channel {
                remote: v["remote"].as_str()?.to_string(),
                ref_: v["ref"].as_str()?.to_string(),
            })
        })
        .unwrap_or_default()
}

fn write_channel(cfg: &Config, ch: &Channel) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.built_dir)?;
    let v = serde_json::json!({"remote": ch.remote, "ref": ch.ref_});
    std::fs::write(&cfg.channel_file, v.to_string())?;
    Ok(())
}

fn list_remotes(cfg: &Config) -> Vec<RemoteEntry> {
    let Ok(out) = std::process::Command::new("git")
        .args(["-C", &cfg.repo_path, "remote", "-v"])
        .output()
    else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut seen = std::collections::HashSet::new();
    text.lines().filter_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && line.contains("(fetch)") {
            let name = parts[0].to_string();
            if seen.insert(name.clone()) {
                return Some(RemoteEntry { name, url: parts[1].to_string() });
            }
        }
        None
    }).collect()
}

pub async fn get_channel(State(state): State<AppState>) -> Json<ChannelInfo> {
    let ch = read_channel(&state.config);
    Json(ChannelInfo { remote: ch.remote, ref_: ch.ref_, remotes: list_remotes(&state.config) })
}

pub async fn set_channel(
    State(state): State<AppState>,
    Json(ch): Json<Channel>,
) -> impl IntoResponse {
    match write_channel(&state.config, &ch) {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"remote": ch.remote, "ref": ch.ref_}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn add_remote(
    State(state): State<AppState>,
    Json(body): Json<RemoteBody>,
) -> impl IntoResponse {
    let out = std::process::Command::new("git")
        .args(["-C", &state.config.repo_path, "remote", "add", &body.name, &body.url])
        .output();
    match out {
        Ok(o) if o.status.success() =>
            (StatusCode::OK, Json(serde_json::json!({"name": body.name, "url": body.url}))).into_response(),
        Ok(o) =>
            (StatusCode::BAD_REQUEST, String::from_utf8_lossy(&o.stderr).to_string()).into_response(),
        Err(e) =>
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn remove_remote(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<serde_json::Value> {
    let _ = std::process::Command::new("git")
        .args(["-C", &state.config.repo_path, "remote", "remove", &name])
        .output();
    Json(serde_json::json!({"ok": true}))
}

pub async fn update(
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, Infallible>>> {
    let cfg = state.config;
    let stream = async_stream::stream! {
        let ch = read_channel(&cfg);

        // Fetch
        let fetch_args = vec![
            "-C".to_string(), cfg.repo_path.clone(),
            "fetch".to_string(), ch.remote.clone(), "--tags".to_string(),
        ];
        yield Ok(Event::default().data(format!("$ git {}", fetch_args.join(" "))));

        let fetch_rc = {
            let args: Vec<&str> = fetch_args.iter().map(|s| s.as_str()).collect();
            let child = tokio::process::Command::new("git")
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();
            match child {
                Err(e) => {
                    yield Ok(Event::default().data(format!("[ERROR] {e}")));
                    return;
                }
                Ok(c) => {
                    let mut guard = KillOnDrop(c);
                    use tokio::io::AsyncBufReadExt;
                    if let Some(stdout) = guard.0.stdout.take() {
                        let mut lines = tokio::io::BufReader::new(stdout).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            yield Ok(Event::default().data(l));
                        }
                    }
                    guard.0.wait().await.map(|s| s.code().unwrap_or(1)).unwrap_or(1)
                }
            }
        };

        if fetch_rc != 0 {
            yield Ok(Event::default().data(format!("[ERROR] fetch failed (exit {fetch_rc})")));
            return;
        }

        // Resolve ref: try remote/ref first (branch), fall back to bare ref (tag/commit)
        let remote_ref = format!("{}/{}", ch.remote, ch.ref_);
        let has_remote_ref = std::process::Command::new("git")
            .args(["-C", &cfg.repo_path, "rev-parse", "--verify", &remote_ref])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let reset_target = if has_remote_ref { remote_ref } else { ch.ref_.clone() };

        // Reset
        yield Ok(Event::default().data(format!("$ git -C {} reset --hard {reset_target}", cfg.repo_path)));
        let reset_rc = {
            let child = tokio::process::Command::new("git")
                .args(["-C", &cfg.repo_path, "reset", "--hard", &reset_target])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();
            match child {
                Err(e) => { yield Ok(Event::default().data(format!("[ERROR] {e}"))); return; }
                Ok(c) => {
                    let mut guard = KillOnDrop(c);
                    use tokio::io::AsyncBufReadExt;
                    if let Some(stdout) = guard.0.stdout.take() {
                        let mut lines = tokio::io::BufReader::new(stdout).lines();
                        while let Ok(Some(l)) = lines.next_line().await { yield Ok(Event::default().data(l)); }
                    }
                    guard.0.wait().await.map(|s| s.code().unwrap_or(1)).unwrap_or(1)
                }
            }
        };
        if reset_rc != 0 {
            yield Ok(Event::default().data(format!("[ERROR] reset failed (exit {reset_rc})")));
            return;
        }

        // nixos-rebuild
        let flake = format!("path:{}#{}", cfg.repo_path, cfg.flake_target);
        yield Ok(Event::default().data(format!("$ nixos-rebuild switch --flake {flake} --print-build-logs")));
        yield Ok(Event::default().data("[INFO] nixos-rebuild launched — service will restart shortly"));

        let _ = std::fs::create_dir_all(cfg.rebuild_log.parent().unwrap_or(std::path::Path::new("/")));
        if let Ok(log_file) = std::fs::File::create(&cfg.rebuild_log) {
            let log2 = log_file.try_clone().unwrap_or_else(|_| std::fs::File::create(&cfg.rebuild_log).unwrap());
            if let Ok(child) = std::process::Command::new("nixos-rebuild")
                .args(["switch", "--flake", &flake, "--no-update-lock-file", "--print-build-logs"])
                .stdin(std::process::Stdio::null())
                .stdout(log_file)
                .stderr(log2)
                .spawn()
            {
                let _ = std::fs::write(&cfg.rebuild_pid, child.id().to_string());
            }
        }
    };

    Sse::new(stream)
}
