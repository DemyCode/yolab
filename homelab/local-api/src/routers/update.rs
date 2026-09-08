use std::{
    convert::Infallible,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Response, Sse},
    Json,
};
use serde::{Deserialize, Serialize};
// StreamExt for `.map` over the progress receiver — see `update`.
use tokio_stream::StreamExt;

use crate::{config::Config, kubectl, proc::KillOnDrop, AppState};

static IS_UPDATING: AtomicBool = AtomicBool::new(false);

struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        IS_UPDATING.store(false, Ordering::SeqCst);
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Channel {
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            remote: "origin".into(),
            ref_: "main".into(),
        }
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
    else {
        return vec![];
    };
    parse_remotes(&String::from_utf8_lossy(&out.stdout))
}

/// Parses `git remote -v` output. Split from `list_remotes` so the line handling
/// is testable without a git binary or a real repository.
///
/// `git remote -v` prints two lines per remote (fetch and push); only the fetch
/// line is taken, so each remote appears once.
fn parse_remotes(text: &str) -> Vec<RemoteEntry> {
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && line.contains("(fetch)") {
                let name = parts[0].to_string();
                if seen.insert(name.clone()) {
                    return Some(RemoteEntry {
                        name,
                        url: parts[1].to_string(),
                    });
                }
            }
            None
        })
        .collect()
}

pub async fn get_channel(State(state): State<AppState>) -> Json<ChannelInfo> {
    let ch = read_channel(&state.config);
    Json(ChannelInfo {
        remote: ch.remote,
        ref_: ch.ref_,
        remotes: list_remotes(&state.config),
    })
}

pub async fn set_channel(
    State(state): State<AppState>,
    Json(ch): Json<Channel>,
) -> impl IntoResponse {
    match write_channel(&state.config, &ch) {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"remote": ch.remote, "ref": ch.ref_})),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn add_remote(
    State(state): State<AppState>,
    Json(body): Json<RemoteBody>,
) -> impl IntoResponse {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &state.config.repo_path,
            "remote",
            "add",
            &body.name,
            &body.url,
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => (
            StatusCode::OK,
            Json(serde_json::json!({"name": body.name, "url": body.url})),
        )
            .into_response(),
        Ok(o) => (
            StatusCode::BAD_REQUEST,
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn remove_remote(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let out = std::process::Command::new("git")
        .args(["-C", &state.config.repo_path, "remote", "remove", &name])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        // Idempotent: a remote that is already gone is the end state a DELETE
        // asked for either way, so this does not count as a failure.
        Ok(o) if String::from_utf8_lossy(&o.stderr).contains("No such remote") => {
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(o) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": String::from_utf8_lossy(&o.stderr).trim(),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
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
// The work in between — fetch, resolve the ref, reset, launch nixos-rebuild — is
// identical, and used to be written twice. That is not a style complaint: the
// two copies had already drifted into `has_remote_ref`/`reset_target` in one and
// `has_remote`/`target` in the other, so a fix to the ref-resolution logic in
// one would silently not reach the other.
//
// It was duplicated for a real reason, though, and the reason is worth stating
// so nobody "simplifies" it back: `update()` is built on `async_stream`, and
// `yield` only works lexically inside the `stream!` macro. You cannot extract a
// helper that yields. The way out is to invert it — the shared code SENDS lines
// down a channel, and each caller decides what to do with them.

/// One line of progress.
async fn emit(out: &tokio::sync::mpsc::Sender<String>, msg: impl Into<String>) {
    // Ignored on purpose: a closed receiver means the person navigated away
    // mid-update. The rebuild must carry on regardless — it is already changing
    // the system, and abandoning it half-done is far worse than talking to
    // nobody.
    let _ = out.send(msg.into()).await;
}

/// Runs a git subcommand, streaming both its streams line by line.
///
/// stderr as well as stdout, and interleaved: git writes progress ("Receiving
/// objects…") to stderr, so a version that forwarded only stdout showed a blank
/// screen for the entire clone and then a result.
async fn run_git(cfg: &Config, out: &tokio::sync::mpsc::Sender<String>, args: &[&str]) -> bool {
    let mut full = vec!["-C", cfg.repo_path.as_str()];
    full.extend_from_slice(args);
    emit(out, format!("$ git {}", full.join(" "))).await;

    let child = tokio::process::Command::new("git")
        .args(&full)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut guard = match child {
        Ok(c) => KillOnDrop(c),
        Err(e) => {
            emit(out, format!("[ERROR] could not launch git: {e}")).await;
            return false;
        }
    };

    use tokio::io::AsyncBufReadExt;
    let stdout = guard.0.stdout.take();
    let stderr = guard.0.stderr.take();
    if let Some(s) = stdout {
        let mut lines = tokio::io::BufReader::new(s).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            emit(out, l).await;
        }
    }
    if let Some(s) = stderr {
        let mut lines = tokio::io::BufReader::new(s).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            emit(out, l).await;
        }
    }
    guard
        .0
        .wait()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Which ref to reset to.
///
/// Prefers `<remote>/<ref>` when git can resolve it, and falls back to the bare
/// ref otherwise — which is what makes a tag or a local branch work as a channel
/// alongside a remote branch. Pure, so the choice is testable without a repo;
/// the resolution itself is the caller's `rev-parse`.
fn reset_target(ch: &Channel, remote_ref_exists: bool) -> String {
    if remote_ref_exists {
        format!("{}/{}", ch.remote, ch.ref_)
    } else {
        ch.ref_.clone()
    }
}

/// Whether git can resolve `<remote>/<ref>`.
fn remote_ref_exists(cfg: &Config, ch: &Channel) -> bool {
    std::process::Command::new("git")
        .args([
            "-C",
            &cfg.repo_path,
            "rev-parse",
            "--verify",
            &format!("{}/{}", ch.remote, ch.ref_),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fetch, reset, and launch the rebuild. Returns false if it stopped early.
///
/// The rebuild itself is deliberately NOT awaited: it is spawned detached with
/// its output going to `cfg.rebuild_log`, so it survives this service being
/// restarted by the very switch it just started. That is the normal case, not an
/// edge one — a nixos-rebuild restarts local-api.
async fn run_update(cfg: &Config, out: &tokio::sync::mpsc::Sender<String>) -> bool {
    let ch = read_channel(cfg);

    if !run_git(cfg, out, &["fetch", &ch.remote, "--tags"]).await {
        emit(out, "[ERROR] git fetch failed").await;
        return false;
    }

    let target = reset_target(&ch, remote_ref_exists(cfg, &ch));
    if !run_git(cfg, out, &["reset", "--hard", &target]).await {
        emit(out, "[ERROR] git reset failed").await;
        return false;
    }

    // A previous rebuild that was interrupted leaves its transient unit behind,
    // and systemd refuses to start a unit that is still loaded-and-failed.
    clear_stale_rebuild_unit();

    let flake = format!("path:{}#{}", cfg.repo_path, cfg.flake_target);
    emit(
        out,
        format!("$ nixos-rebuild switch --flake {flake} --print-build-logs"),
    )
    .await;
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

    // --cores 1 --max-jobs 1: a homelab node is also serving the UI that is
    // watching this, and an unrestricted build starves it.
    let child = std::process::Command::new("nixos-rebuild")
        .args([
            "switch",
            "--flake",
            &flake,
            "--no-update-lock-file",
            "--print-build-logs",
            "--accept-flake-config",
            "--cores",
            "1",
            "--max-jobs",
            "1",
        ])
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
    // so all machines converge to the same remote/ref.
    let ch = read_channel(&cfg);
    let channel_body = serde_json::json!({ "remote": ch.remote, "ref": ch.ref_ });
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

    // ── reset_target ──────────────────────────────────────────────────────────
    //
    // This logic existed in two copies before the rewrite — `has_remote_ref`/
    // `reset_target` in the streaming path and `has_remote`/`target` in the
    // background one — so a fix to either would silently not reach the other.
    // Now there is one, and these pin its behaviour.

    #[test]
    fn a_resolvable_remote_ref_wins() {
        let ch = Channel {
            remote: "origin".into(),
            ref_: "main".into(),
        };
        assert_eq!(reset_target(&ch, true), "origin/main");
    }

    /// The fallback is what lets a TAG or a purely local branch work as a
    /// channel: `origin/v2.1.0` does not resolve, but `v2.1.0` does.
    #[test]
    fn an_unresolvable_remote_ref_falls_back_to_the_bare_ref() {
        let ch = Channel {
            remote: "origin".into(),
            ref_: "v2.1.0".into(),
        };
        assert_eq!(reset_target(&ch, false), "v2.1.0");
    }

    #[test]
    fn a_non_origin_remote_is_honoured() {
        let ch = Channel {
            remote: "upstream".into(),
            ref_: "release".into(),
        };
        assert_eq!(reset_target(&ch, true), "upstream/release");
    }

    // ── read_channel / write_channel ──────────────────────────────────────────

    /// The channel decides which git ref this node builds itself from. Defaulting
    /// to origin/main is what keeps an unreadable or corrupted file from pointing
    /// a machine at nothing — or worse, at a partially-parsed ref.
    #[test]
    fn an_absent_channel_file_reads_as_origin_main() {
        let dir = tempfile::tempdir().unwrap();
        let ch = read_channel(&cfg_in(&dir));
        assert_eq!(ch.remote, "origin");
        assert_eq!(ch.ref_, "main");
    }

    #[test]
    fn a_written_channel_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in(&dir);
        let written = Channel {
            remote: "upstream".into(),
            ref_: "v2.1.0".into(),
        };
        write_channel(&cfg, &written).unwrap();

        let read = read_channel(&cfg);
        assert_eq!(read.remote, "upstream");
        assert_eq!(read.ref_, "v2.1.0");
    }

    #[test]
    fn writing_a_channel_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in(&dir);
        assert!(!cfg.built_dir.exists());
        write_channel(&cfg, &Channel::default()).unwrap();
        assert!(cfg.channel_file.exists());
    }

    /// A half-written or hand-edited file must fall back wholesale rather than
    /// mix a parsed remote with a defaulted ref — that combination points at a
    /// ref that may not exist on that remote.
    #[test]
    fn a_malformed_channel_file_falls_back_completely() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in(&dir);
        std::fs::create_dir_all(&cfg.built_dir).unwrap();

        for body in [
            "",
            "not json at all",
            r#"{"remote": "upstream"}"#,     // ref missing
            r#"{"ref": "v2"}"#,              // remote missing
            r#"{"remote": 5, "ref": "v2"}"#, // wrong type
            r#"{"remote": null, "ref": null}"#,
            "[]",
        ] {
            std::fs::write(&cfg.channel_file, body).unwrap();
            let ch = read_channel(&cfg);
            assert_eq!(
                (ch.remote.as_str(), ch.ref_.as_str()),
                ("origin", "main"),
                "body: {body}"
            );
        }
    }

    #[test]
    fn a_channel_file_with_extra_keys_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in(&dir);
        std::fs::create_dir_all(&cfg.built_dir).unwrap();
        std::fs::write(
            &cfg.channel_file,
            r#"{"remote":"origin","ref":"dev","note":"hi"}"#,
        )
        .unwrap();
        assert_eq!(read_channel(&cfg).ref_, "dev");
    }

    // ── parse_remotes ─────────────────────────────────────────────────────────

    const GIT_REMOTE_V: &str = "\
origin\thttps://github.com/DemyCode/yolab.git (fetch)
origin\thttps://github.com/DemyCode/yolab.git (push)
fork\tgit@github.com:someone/yolab.git (fetch)
fork\tgit@github.com:someone/yolab.git (push)
";

    /// git prints a fetch and a push line per remote; listing both would show
    /// every remote twice in the update UI.
    #[test]
    fn each_remote_is_listed_once() {
        let remotes = parse_remotes(GIT_REMOTE_V);
        let names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["origin", "fork"]);
    }

    #[test]
    fn remote_urls_are_read_from_the_fetch_line() {
        let remotes = parse_remotes(GIT_REMOTE_V);
        assert_eq!(remotes[0].url, "https://github.com/DemyCode/yolab.git");
        assert_eq!(remotes[1].url, "git@github.com:someone/yolab.git");
    }

    /// A push-only remote cannot be updated from, so it does not belong in the
    /// list of things you can switch your channel to.
    #[test]
    fn a_push_only_remote_is_not_listed() {
        let text = "backup\tgit@example.com:mirror.git (push)\n";
        assert!(parse_remotes(text).is_empty());
    }

    #[test]
    fn parsing_survives_empty_and_ragged_output() {
        assert!(parse_remotes("").is_empty());
        assert!(parse_remotes("\n\n  \n").is_empty());
        assert!(parse_remotes("origin\n").is_empty()); // name with no url
        assert!(parse_remotes("fatal: not a git repository").is_empty());
    }

    #[test]
    fn remotes_keep_gits_own_ordering() {
        // The first entry is what the UI preselects, so ordering is load-bearing.
        let text = "zebra\turl-z (fetch)\nalpha\turl-a (fetch)\n";
        let names: Vec<String> = parse_remotes(text).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["zebra", "alpha"]);
    }

    // ── remove_remote ────────────────────────────────────────────────────────
    //
    // This used to be `let _ = ...output(); Json({"ok": true})` — every call
    // reported success, whether or not git did anything at all. These run a
    // real git against a throwaway repo (see nix/rust.nix's gitMinimal note on
    // the local-api crate) rather than mocking the subprocess, because the bug
    // was specifically in what happens when that subprocess fails.

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        dir
    }

    fn state_in(dir: &tempfile::TempDir) -> crate::AppState {
        let mut cfg = Config::for_test(&dir.path().join("config.toml"));
        cfg.repo_path = dir.path().to_string_lossy().into_owned();
        let cfg = std::sync::Arc::new(cfg);
        crate::AppState {
            auth: crate::auth::AuthState {
                sessions: crate::auth::new_sessions(),
                config: std::sync::Arc::clone(&cfg),
            },
            config: cfg,
        }
    }

    async fn body_json(res: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn remove_remote_deletes_a_remote_that_exists() {
        let dir = git_repo();
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let state = state_in(&dir);

        let res = remove_remote(State(state.clone()), Path("origin".to_string())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["ok"], true);

        let list = std::process::Command::new("git")
            .args(["remote"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&list.stdout).trim().is_empty());
    }

    /// DELETE is idempotent: a remote that is already gone is the end state
    /// being asked for, not a failure — the caller should not have to check
    /// "does it exist?" before every delete just to avoid a spurious error.
    #[tokio::test]
    async fn remove_remote_on_a_nonexistent_remote_still_reports_ok() {
        let dir = git_repo();
        let state = state_in(&dir);

        let res = remove_remote(State(state), Path("never-existed".to_string())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_json(res).await["ok"], true);
    }

    /// A real failure — not "already gone" — must be reported, not swallowed
    /// into the same {"ok": true} every call used to return.
    #[tokio::test]
    async fn remove_remote_reports_a_real_git_failure() {
        // Not a git repository at all: `git remote remove` fails with something
        // other than "No such remote", which is the case that must surface.
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(&dir);

        let res = remove_remote(State(state), Path("origin".to_string())).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_json(res).await;
        assert_eq!(body["ok"], false);
        assert!(
            body["error"].as_str().unwrap().contains("git"),
            "expected git's own error text, got: {body}"
        );
    }
}
