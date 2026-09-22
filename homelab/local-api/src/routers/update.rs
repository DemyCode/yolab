use std::{
    convert::Infallible,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{sse::Event, IntoResponse, Response, Sse},
    Json,
};
use tokio_stream::StreamExt;

use crate::{
    config::{Channel, Config},
    kubectl, AppState,
};

static IS_UPDATING: AtomicBool = AtomicBool::new(false);

pub(crate) struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        IS_UPDATING.store(false, Ordering::SeqCst);
    }
}

fn heal_holds(cfg: &Config) -> Option<String> {
    crate::heal::member::holds_config(&crate::heal::member::Layout::from_config(cfg)).then(|| {
        "a FORCE HEAL has prepared this machine for a new cluster — updates wait until it restarts or the heal is undone".to_string()
    })
}

pub(crate) fn exclusive() -> Option<UpdateGuard> {
    IS_UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| UpdateGuard)
}

pub async fn get_channel(State(state): State<AppState>) -> Json<Channel> {
    Json(state.config.channel())
}

pub async fn set_channel(
    State(state): State<AppState>,
    Json(ch): Json<Channel>,
) -> impl IntoResponse {
    match state.config.write_channel(&ch) {
        Ok(_) => (StatusCode::OK, Json(ch)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn emit(out: &tokio::sync::mpsc::Sender<String>, msg: impl Into<String>) {
    let _ = out.send(msg.into()).await;
}

async fn run_update(cfg: &Config, out: &tokio::sync::mpsc::Sender<String>) -> bool {
    let ch = cfg.channel();

    emit(
        out,
        format!("[INFO] building {}#{}", ch.flake(), cfg.flake_target),
    )
    .await;

    clear_stale_rebuild_unit();

    let args = rebuild_args(cfg, &ch);
    emit(out, format!("$ nixos-rebuild {}", args.join(" "))).await;
    emit(
        out,
        "[INFO] nixos-rebuild launched — this service will restart shortly",
    )
    .await;

    let (Ok(log_file), Ok(log2)) = (
        std::fs::File::create(&cfg.rebuild_log),
        std::fs::File::create(&cfg.rebuild_log),
    ) else {
        emit(out, "[ERROR] could not open the rebuild log").await;
        return false;
    };

    let child = std::process::Command::new("nixos-rebuild")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log2)
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            emit(out, format!("[ERROR] could not launch nixos-rebuild: {e}")).await;
            return false;
        }
    };

    let pid = child.id();
    let _ = std::fs::write(&cfg.rebuild_pid, pid.to_string());
    let pid_file = cfg.rebuild_pid.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        if std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            == Some(pid)
        {
            let _ = std::fs::remove_file(&pid_file);
        }
    });
    true
}

pub async fn update(State(state): State<AppState>) -> Response {
    if let Some(why) = heal_holds(&state.config) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": why })),
        )
            .into_response();
    }
    if IS_UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Update already in progress"})),
        )
            .into_response();
    }

    let cfg = state.config.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);

    tokio::spawn(async move {
        let _guard = UpdateGuard;
        run_update(&cfg, &tx).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|line| Ok::<Event, Infallible>(Event::default().data(line)));
    Sse::new(stream).into_response()
}

pub async fn trigger_update(State(state): State<AppState>) -> Json<serde_json::Value> {
    if let Some(why) = heal_holds(&state.config) {
        return Json(serde_json::json!({ "error": why }));
    }
    if IS_UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Json(serde_json::json!({"error": "already updating"}));
    }

    let cfg = state.config.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);

    let log_path = cfg.rebuild_log.clone();
    tokio::spawn(async move {
        let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new("/")));
        while let Some(line) = rx.recv().await {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                use std::io::Write;
                let _ = writeln!(f, "[trigger] {line}");
            }
        }
    });

    tokio::spawn(async move {
        let _guard = UpdateGuard;
        run_update(&cfg, &tx).await;
    });

    Json(serde_json::json!({"status": "started"}))
}

fn rebuild_args(cfg: &Config, ch: &Channel) -> Vec<String> {
    vec![
        "switch".into(),
        "--flake".into(),
        format!("{}#{}", ch.flake(), cfg.flake_target),
        "--override-input".into(),
        "yolab-machine".into(),
        format!("path:{}", cfg.machine_dir),
        "--refresh".into(),
        "--no-write-lock-file".into(),
        "--print-build-logs".into(),
        "--accept-flake-config".into(),
        "--cores".into(),
        "1".into(),
        "--max-jobs".into(),
        "1".into(),
    ]
}

fn clear_stale_rebuild_unit() {
    let _ = std::process::Command::new("systemctl")
        .args([
            "reset-failed",
            "nixos-rebuild-switch-to-configuration.service",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub async fn update_all(State(state): State<AppState>) -> Response {
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();

    let ch = cfg.channel();
    let channel_body = serde_json::json!({ "url": ch.url, "ref": ch.ref_ });
    let cluster_token = cfg.cluster_token();

    let nodes = kubectl::get_nodes().await.unwrap_or_default();
    for addr in kubectl::peer_ipv6(&nodes, &self_ip) {
        let base = format!("http://[{}]:{}", addr, cfg.port);
        let body = channel_body.clone();
        let token = cluster_token.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let _ = client
                .put(format!("{base}/api/update/channel"))
                .header(crate::auth::CLUSTER_AUTH_HEADER, &token)
                .json(&body)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            let _ = client
                .post(format!("{base}/api/update/trigger"))
                .header(crate::auth::CLUSTER_AUTH_HEADER, &token)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        });
    }

    update(State(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_in(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::for_test(&dir.path().join("config.toml"));
        cfg.built_dir = dir.path().join("built");
        cfg.channel_file = cfg.built_dir.join("channel.json");
        cfg
    }

    #[test]
    fn a_rebuild_uses_the_flake_url_and_this_machines_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_in(&dir);
        cfg.machine_dir = "/var/lib/yolab/machine".into();
        let ch = Channel {
            url: "github:DemyCode/yolab".into(),
            ref_: "main".into(),
        };
        let args = rebuild_args(&cfg, &ch);
        let after = |flag: &str| {
            let i = args.iter().position(|a| a == flag).unwrap();
            args[i + 1..].to_vec()
        };
        assert_eq!(
            after("--flake")[0],
            "github:DemyCode/yolab/main#yolab",
            "a URL, never a path — there is no checkout on this node"
        );
        assert_eq!(
            after("--override-input")[..2],
            ["yolab-machine", "path:/var/lib/yolab/machine"]
        );
        assert!(args.iter().any(|a| a == "--no-write-lock-file"));
        assert!(
            args.iter().any(|a| a == "--refresh"),
            "a mutable ref must be re-resolved, or an update silently rebuilds the stale revision"
        );
        assert!(
            !args.iter().any(|a| a == "--no-update-lock-file"),
            "the override changes the lock in memory"
        );
    }

    #[test]
    fn a_rebuild_honours_a_custom_source_and_ref() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in(&dir);
        let ch = Channel {
            url: "github:someone/fork".into(),
            ref_: "v2.1.0".into(),
        };
        let args = rebuild_args(&cfg, &ch);
        let i = args.iter().position(|a| a == "--flake").unwrap();
        assert_eq!(args[i + 1], "github:someone/fork/v2.1.0#yolab");
    }
}
