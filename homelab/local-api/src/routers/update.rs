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
// StreamExt for `.map` over the progress receiver — see `update`.
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

/// Why this machine must not update now: a FORCE HEAL rewrote its config.toml
/// for the new cluster, and switching to it would start that cluster on the old
/// state, without the wipe.
fn heal_holds(cfg: &Config) -> Option<String> {
    crate::heal::member::holds_config(&crate::heal::member::Layout::from_config(cfg)).then(|| {
        "a FORCE HEAL has prepared this machine for a new cluster — updates wait until it restarts or the heal is undone".to_string()
    })
}

/// Holds off updates for as long as the guard lives, or `None` when one is
/// running. For a FORCE HEAL building or switching this machine's system, which
/// an update doing the same at the same time would race.
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

// ── The update sequence, written once ─────────────────────────────────────────
//
// There are two ways to ask this node to update itself, and they differ ONLY in
// where the progress lines go:
//
//   update()         — a person clicked Update; stream it back over SSE
//   trigger_update() — another node told us to; append to the rebuild log
//
// The work in between — resolve the flake, launch nixos-rebuild — is identical.
// It used to be written twice, and the two copies had already drifted. It was
// duplicated for a real reason, though, worth stating so nobody "simplifies" it
// back: `update()` is built on `async_stream`, and `yield` only works lexically
// inside the `stream!` macro. You cannot extract a helper that yields. The way
// out is to invert it — the shared code SENDS lines down a channel, and each
// caller decides what to do with them.

/// One line of progress.
async fn emit(out: &tokio::sync::mpsc::Sender<String>, msg: impl Into<String>) {
    // Ignored on purpose: a closed receiver means the person navigated away
    // mid-update. The rebuild must carry on regardless — it is already changing
    // the system, and abandoning it half-done is far worse than talking to
    // nobody.
    let _ = out.send(msg.into()).await;
}

/// Launch the rebuild. Returns false if it stopped early.
///
/// There is no checkout to fetch or reset: the flake is named by its URL (see
/// `Channel`), and `nixos-rebuild` fetches the revision itself. That is the whole
/// reason this node no longer needs the repo on disk, and the same shape a
/// community catalog takes — point at a URL, keep this machine's own files as
/// the only local override.
///
/// The rebuild itself is deliberately NOT awaited: it is spawned detached with
/// its output going to `cfg.rebuild_log`, so it survives this service being
/// restarted by the very switch it just started. That is the normal case, not an
/// edge one — a nixos-rebuild restarts local-api.
async fn run_update(cfg: &Config, out: &tokio::sync::mpsc::Sender<String>) -> bool {
    let ch = cfg.channel();

    emit(
        out,
        format!("[INFO] building {}#{}", ch.flake(), cfg.flake_target),
    )
    .await;

    // A previous rebuild that was interrupted leaves its transient unit behind,
    // and systemd refuses to start a unit that is still loaded-and-failed.
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
    // Reap the child so it does not linger as a zombie once nixos-rebuild
    // exits. If this service is restarted by the rebuild itself the thread
    // dies, the child is adopted by init which reaps it, and the fallback
    // zombie check in rebuild.rs covers that race.
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

/// `GET /api/update` — a person clicked Update; stream the progress back.
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

// ── Background (fire-and-forget) update ───────────────────────────────────────

/// `POST /api/update/trigger` — another node told us to update.
///
/// Returns 200 immediately so the caller can drop the connection without
/// cancelling the work, and the same progress lines go to the rebuild log
/// instead of to a browser.
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

/// The `nixos-rebuild` arguments for this machine.
///
/// The flake is a URL (`github:owner/repo/<ref>`), never a path: there is no
/// checkout on this node, so `nixos-rebuild` fetches the revision named by the
/// channel. This machine's own files still come in as the `yolab-machine` input
/// from `machine_dir` — see flake.nix — so the only thing local is the config.
/// The lock file is not written: the override is this machine's, and the
/// published flake's own lock stays the one the revision carries.
///
/// `--cores 1 --max-jobs 1`: a homelab node is also serving the UI that is
/// watching this, and an unrestricted build starves it.
fn rebuild_args(cfg: &Config, ch: &Channel) -> Vec<String> {
    vec![
        "switch".into(),
        "--flake".into(),
        format!("{}#{}", ch.flake(), cfg.flake_target),
        "--override-input".into(),
        "yolab-machine".into(),
        format!("path:{}", cfg.machine_dir),
        // `main` is a mutable ref. Without --refresh, Nix can reuse a cached
        // resolution of it and rebuild the revision it already had, so the
        // update button appears to run and changes nothing. It happened: two
        // updates in a row built the same stale source after a fix was pushed.
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

/// Clear the leftover of an interrupted `nixos-rebuild`.
///
/// nixos-rebuild runs switch-to-configuration inside a transient systemd unit
/// with a FIXED name. If a previous run was interrupted — killed, or wedged
/// behind a service that would not stop — that unit stays loaded, and every
/// later run then dies instantly on:
///
///   Failed to start transient service unit: Unit
///   nixos-rebuild-switch-to-configuration.service was already loaded or has a
///   fragment file
///
/// So one interrupted update breaks EVERY FUTURE UPDATE, permanently, until
/// someone clears it by hand over SSH. On a machine whose entire update story
/// is a button in a web page, that is the update mechanism disabling itself —
/// and it happened: a rebuild hung behind processes systemd could not kill, and
/// the next attempt failed before it started.
///
/// `reset-failed` is the right tool because of what it will NOT do: it clears
/// failed and inactive units and leaves a genuinely running one alone, so this
/// cannot interrupt a rebuild that is legitimately still going.
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
    // K3s and Ceph keep running through a NixOS rebuild, so there's no quorum
    // risk — just fire all nodes in parallel and stream self's output as usual.
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();

    // Read this node's channel and push it to every other node before rebuilding,
    // so all machines converge to the same source/ref.
    let ch = cfg.channel();
    let channel_body = serde_json::json!({ "url": ch.url, "ref": ch.ref_ });
    let cluster_token = cfg.cluster_token();

    // kubectl::peer_ipv6 rather than a fourth hand-rolled copy of this filter.
    let nodes = kubectl::get_nodes().await.unwrap_or_default();
    for addr in kubectl::peer_ipv6(&nodes, &self_ip) {
        let base = format!("http://[{}]:{}", addr, cfg.port);
        let body = channel_body.clone();
        let token = cluster_token.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            // Sync channel, then fire trigger (returns 200 immediately —
            // the actual work runs in a background task on the remote node).
            // Both carry the shared cluster token so the peer's auth
            // middleware accepts them without a user session.
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

    // Stream self update exactly like the single-node handler.
    update(State(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Config whose channel file lives in a throwaway directory.
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

    /// The channel is what the UI edits, so a non-default source must reach the
    /// command line rather than being silently ignored.
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
