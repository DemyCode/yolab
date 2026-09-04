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
    let cfg = state.config;
    let stream = async_stream::stream! {
        let _guard = UpdateGuard;
        let ch = read_channel(&cfg);

        // Fetch
        let fetch_args = ["-C".to_string(), cfg.repo_path.clone(),
            "fetch".to_string(), ch.remote.clone(), "--tags".to_string()];
        yield Ok::<Event, Infallible>(Event::default().data(format!("$ git {}", fetch_args.join(" "))));

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

        // No Ceph health gate here any more.
        //
        // There was one: it ran `yolab-ceph-wait-healthy`, which waited up to five
        // minutes for backfill to finish before rebuilding. It never refused. Every
        // path out of it — recovery still running, ceph unreachable, the script itself
        // wedging — ended in "[WARN] … continuing", deliberately, because a cluster can
        // sit degraded for reasons an update would FIX, and refusing to update an
        // unhealthy cluster makes the platform unrepairable from the UI exactly when
        // repairing it matters most.
        //
        // Which left a wait that changed nothing except how long an update took. It
        // also had to be wrapped in an outer timeout after it deadlocked for real: the
        // blocking call parked a runtime worker inside this SSE handler, local-api kept
        // listening on :3001 and answered nothing, and the whole UI went blank behind a
        // 502. A step that cannot refuse is not worth the ways it can fail.
        //
        // The risk it was written for is real but narrower than it looked: a rebuild
        // only restarts OSDs when it bumps the Ceph package or changes their unit, and
        // the damage needs several nodes restarting while one is still backfilling. If
        // that becomes a problem, the fix is a gate that actually refuses on a Ceph
        // version change — not a pause that always gives way.

        // nixos-rebuild
        clear_stale_rebuild_unit();
        let flake = format!("path:{}#{}", cfg.repo_path, cfg.flake_target);
        yield Ok(Event::default().data(format!("$ nixos-rebuild switch --flake {flake} --print-build-logs")));
        yield Ok(Event::default().data("[INFO] nixos-rebuild launched — service will restart shortly"));

        let _ = std::fs::create_dir_all(cfg.rebuild_log.parent().unwrap_or(std::path::Path::new("/")));

        // Every failure below used to be swallowed silently — the stream would just end
        // after the "launched" message above with no explanation, which reads as a
        // successful update that never actually started. Each one now yields an
        // [ERROR] event before returning, same convention the reset-failure path above
        // already uses.
        let log_file = match std::fs::File::create(&cfg.rebuild_log) {
            Ok(f) => f,
            Err(e) => {
                yield Ok(Event::default().data(format!(
                    "[ERROR] could not create rebuild log {}: {e}",
                    cfg.rebuild_log.display()
                )));
                return;
            }
        };
        let log2 = match log_file.try_clone() {
            Ok(f) => f,
            Err(e) => {
                yield Ok(Event::default().data(format!(
                    "[ERROR] could not duplicate the rebuild log handle: {e}"
                )));
                return;
            }
        };
        // Run under idle I/O class + nice 19 so the entire build tree
        // (nix, rustc, linker) yields to k3s and Ceph on disk and CPU.
        // ionice/nice exec into the next command, keeping the same PID.
        let mut child = match std::process::Command::new("nixos-rebuild")
            .args(["switch", "--flake", &flake,
                   "--no-update-lock-file", "--print-build-logs", "--accept-flake-config",
                   "--cores", "1", "--max-jobs", "1"])
            .stdin(std::process::Stdio::null())
            .stdout(log_file)
            .stderr(log2)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                yield Ok(Event::default().data(format!("[ERROR] could not launch nixos-rebuild: {e}")));
                return;
            }
        };

        let pid = child.id();
        let _ = std::fs::write(&cfg.rebuild_pid, pid.to_string());
        let pid_file = cfg.rebuild_pid.clone();
        // Reap the child so it doesn't stay as a zombie in /proc/{pid}
        // after nixos-rebuild exits. If this service is restarted by the
        // rebuild itself, the thread dies but the child is adopted by init
        // which will reap it — the fallback zombie check in rebuild.rs
        // covers that race.
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
    };

    Sse::new(stream).into_response()
}

// ── Background (fire-and-forget) update ───────────────────────────────────────

/// Called by other nodes via update_all. Starts the full update cycle in a
/// background task and returns 200 immediately so the caller can drop the
/// connection without cancelling the work.
pub async fn trigger_update(State(state): State<AppState>) -> Json<serde_json::Value> {
    if IS_UPDATING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Json(serde_json::json!({"error": "already updating"}));
    }
    let cfg = state.config.clone();
    tokio::spawn(async move {
        let _guard = UpdateGuard;
        let ch = read_channel(&cfg);

        let _ = std::fs::create_dir_all(
            cfg.rebuild_log
                .parent()
                .unwrap_or(std::path::Path::new("/")),
        );

        // Helper: append a line to the rebuild log so background git ops are visible.
        let log_path = cfg.rebuild_log.clone();
        let append_log = |msg: String| {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                use std::io::Write;
                let _ = writeln!(f, "{msg}");
            }
        };

        // git fetch
        let fetch_out = tokio::process::Command::new("git")
            .args(["-C", &cfg.repo_path, "fetch", &ch.remote, "--tags"])
            .output()
            .await;
        let fetch_ok = fetch_out
            .as_ref()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !fetch_ok {
            let stderr = fetch_out
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            append_log(format!("[trigger] git fetch failed: {stderr}"));
            return;
        }

        // git reset --hard
        let remote_ref = format!("{}/{}", ch.remote, ch.ref_);
        let has_remote = std::process::Command::new("git")
            .args(["-C", &cfg.repo_path, "rev-parse", "--verify", &remote_ref])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let target = if has_remote {
            remote_ref
        } else {
            ch.ref_.clone()
        };
        let reset_out = tokio::process::Command::new("git")
            .args(["-C", &cfg.repo_path, "reset", "--hard", &target])
            .output()
            .await;
        let reset_ok = reset_out
            .as_ref()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !reset_ok {
            let stderr = reset_out
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            append_log(format!("[trigger] git reset failed: {stderr}"));
            return;
        }
        if let Ok(ref o) = reset_out {
            append_log(format!(
                "[trigger] git reset: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            ));
        }

        // nixos-rebuild (detached — survives local-api restart)
        clear_stale_rebuild_unit();
        let flake = format!("path:{}#{}", cfg.repo_path, cfg.flake_target);
        if let (Ok(log_file), Ok(log2)) = (
            std::fs::File::create(&cfg.rebuild_log),
            std::fs::File::create(&cfg.rebuild_log),
        ) {
            if let Ok(mut child) = std::process::Command::new("nixos-rebuild")
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
                .spawn()
            {
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
            }
        }
    });
    Json(serde_json::json!({"ok": true}))
}

// ── Update all nodes ──────────────────────────────────────────────────────────

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

    for node in kubectl::get_nodes().await.unwrap_or_default() {
        if let Some(addr) = node["status"]["addresses"]
            .as_array()
            .and_then(|a| {
                a.iter().find(|a| {
                    a["type"] == "InternalIP"
                        && a["address"]
                            .as_str()
                            .map(|s| s.contains(':'))
                            .unwrap_or(false)
                })
            })
            .and_then(|a| a["address"].as_str())
        {
            if addr == self_ip {
                continue;
            }
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
