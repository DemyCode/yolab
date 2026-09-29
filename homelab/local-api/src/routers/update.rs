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
    host::Host,
    AppState,
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

const SWITCH_UNIT: &str = "nixos-rebuild-switch-to-configuration.service";

async fn run_update<H: Host>(
    host: &H,
    cfg: &Config,
    out: &tokio::sync::mpsc::Sender<String>,
) -> bool {
    let ch = cfg.channel();

    emit(
        out,
        format!("[INFO] building {}#{}", ch.flake(), cfg.flake_target),
    )
    .await;

    let _ = host.systemctl(&["reset-failed", SWITCH_UNIT]).await;

    let args = rebuild_args(cfg, &ch);
    emit(out, format!("$ nixos-rebuild {}", args.join(" "))).await;
    emit(
        out,
        "[INFO] nixos-rebuild launched — this service will restart shortly",
    )
    .await;

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match host
        .spawn_detached("nixos-rebuild", &argv, &cfg.rebuild_log, &cfg.rebuild_pid)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            emit(out, format!("[ERROR] could not launch nixos-rebuild: {e}")).await;
            false
        }
    }
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
        run_update(&crate::host::RealHost, &cfg, &tx).await;
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
        run_update(&crate::host::RealHost, &cfg, &tx).await;
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

const PEER_SETTLE_TIMEOUT: Duration = Duration::from_secs(1800);
const PEER_SETTLE_POLL: Duration = Duration::from_secs(15);

struct UpdateFleet {
    client: crate::http::Client,
    port: u16,
    token: String,
    channel: serde_json::Value,
}

impl crate::runtime::fleet::Fleet for UpdateFleet {
    async fn act(&self, node: &str) -> anyhow::Result<()> {
        let base = crate::http::peer_url(node, self.port, "");
        let set = self
            .client
            .put(format!("{base}/api/update/channel"))
            .header(crate::auth::CLUSTER_AUTH_HEADER, &self.token)
            .json(&self.channel)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        anyhow::ensure!(
            set.status().is_success(),
            "{node} refused the channel: {}",
            set.status()
        );
        let go = self
            .client
            .post(format!("{base}/api/update/trigger"))
            .header(crate::auth::CLUSTER_AUTH_HEADER, &self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        anyhow::ensure!(
            go.status().is_success(),
            "{node} refused to start updating: {}",
            go.status()
        );
        Ok(())
    }

    async fn settled(&self, node: &str) -> bool {
        self.client
            .get(crate::http::peer_url(node, self.port, "/api/status"))
            .header(crate::auth::CLUSTER_AUTH_HEADER, &self.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

pub async fn update_all(State(state): State<AppState>) -> Response {
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();
    let ch = cfg.channel();

    let nodes = match state.kube.client().await {
        Ok(client) => crate::k8s::nodes(&client).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let peers = crate::runtime::fleet::order(&crate::k8s::peer_ipv6(&nodes, &self_ip), &self_ip);

    let fleet = UpdateFleet {
        client: crate::http::client(),
        port: cfg.port,
        token: cfg.cluster_token(),
        channel: serde_json::json!({ "url": ch.url, "ref": ch.ref_ }),
    };

    let rolled =
        crate::runtime::fleet::rolling(&fleet, &peers, PEER_SETTLE_TIMEOUT, PEER_SETTLE_POLL).await;

    if let Some(stuck) = rolled.stopped_at.as_deref() {
        tracing::error!(
            "update: stopped at {stuck} ({}) — {:?} were left alone and this machine keeps its \
             current build, so the cluster is not left half-updated with nobody serving",
            rolled.why.clone().unwrap_or_default(),
            rolled.skipped
        );
        return (
            axum::http::StatusCode::CONFLICT,
            Json(serde_json::json!({
                "status": "stopped",
                "updated": rolled.done,
                "stopped_at": stuck,
                "why": rolled.why,
                "skipped": rolled.skipped,
            })),
        )
            .into_response();
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

    use crate::host::fake::FakeHost;
    use crate::runtime::fleet::Fleet;
    use crate::testkit::{peer, PEER};
    use wiremock::{matchers, Mock, ResponseTemplate};

    fn update_cfg(dir: &tempfile::TempDir) -> Config {
        let mut cfg = cfg_in(dir);
        cfg.rebuild_log = dir.path().join("rebuild.log");
        cfg.rebuild_pid = dir.path().join("rebuild.pid");
        cfg
    }

    async fn run(host: &FakeHost, cfg: &Config) -> (bool, Vec<String>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let launched = run_update(host, cfg, &tx).await;
        drop(tx);
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        (launched, lines)
    }

    #[tokio::test]
    async fn an_update_clears_the_failed_switch_unit_before_launching_the_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = update_cfg(&dir);
        let host = FakeHost::new()
            .ok("systemctl reset-failed", "")
            .ok("nixos-rebuild switch", "building the system");
        let (launched, lines) = run(&host, &cfg).await;
        assert!(launched);
        let cleared = host
            .position(&format!("systemctl reset-failed {SWITCH_UNIT}"))
            .expect("a failed switch unit makes the next switch refuse to start");
        let rebuilt = host
            .position("nixos-rebuild switch --flake")
            .expect("never launched");
        assert!(cleared < rebuilt, "{:?}", host.calls());
        assert!(lines
            .iter()
            .any(|l| l.starts_with("$ nixos-rebuild switch")));
        assert!(!lines.iter().any(|l| l.starts_with("[ERROR]")));
    }

    #[tokio::test]
    async fn the_rebuild_writes_into_the_log_and_pid_file_the_ui_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = update_cfg(&dir);
        let host = FakeHost::new()
            .ok("systemctl reset-failed", "")
            .ok("nixos-rebuild switch", "building the system");
        run(&host, &cfg).await;
        assert_eq!(
            std::fs::read_to_string(&cfg.rebuild_log).unwrap(),
            "building the system"
        );
        assert!(cfg.rebuild_pid.exists());
    }

    #[tokio::test]
    async fn a_switch_unit_that_cannot_be_cleared_does_not_stop_the_update() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = update_cfg(&dir);
        let host = FakeHost::new()
            .fail("systemctl reset-failed", "Unit not loaded")
            .ok("nixos-rebuild switch", "");
        let (launched, _) = run(&host, &cfg).await;
        assert!(launched);
        assert!(host.ran("nixos-rebuild switch"));
    }

    #[tokio::test]
    async fn a_rebuild_that_cannot_start_is_reported_not_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = update_cfg(&dir);
        let host = FakeHost::new()
            .ok("systemctl reset-failed", "")
            .fail("nixos-rebuild", "No such file or directory");
        let (launched, lines) = run(&host, &cfg).await;
        assert!(!launched);
        assert!(lines
            .iter()
            .any(|l| l.starts_with("[ERROR] could not launch nixos-rebuild")));
        assert!(!cfg.rebuild_pid.exists());
    }

    fn fleet(port: u16) -> UpdateFleet {
        UpdateFleet {
            client: crate::http::client(),
            port,
            token: "cluster-tok".into(),
            channel: serde_json::json!({ "url": "github:DemyCode/yolab", "ref": "v2" }),
        }
    }

    #[tokio::test]
    async fn a_peer_is_given_this_machines_channel_before_being_told_to_update() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("PUT"))
            .and(matchers::path("/api/update/channel"))
            .and(matchers::header(
                crate::auth::CLUSTER_AUTH_HEADER,
                "cluster-tok",
            ))
            .and(matchers::body_json(
                serde_json::json!({ "url": "github:DemyCode/yolab", "ref": "v2" }),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/update/trigger"))
            .and(matchers::header(
                crate::auth::CLUSTER_AUTH_HEADER,
                "cluster-tok",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        fleet(port).act(PEER).await.unwrap();
        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(paths, ["/api/update/channel", "/api/update/trigger"]);
    }

    #[tokio::test]
    async fn a_peer_that_refuses_the_channel_is_never_told_to_update() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("PUT"))
            .and(matchers::path("/api/update/channel"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/update/trigger"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let err = fleet(port).act(PEER).await.unwrap_err();
        assert!(err.to_string().contains("refused the channel"));
    }

    #[tokio::test]
    async fn a_peer_that_refuses_to_update_stops_the_roll() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("PUT"))
            .and(matchers::path("/api/update/channel"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/update/trigger"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let err = fleet(port).act(PEER).await.unwrap_err();
        assert!(err.to_string().contains("refused to start updating"));
    }

    #[tokio::test]
    async fn a_peer_has_settled_only_once_its_api_answers() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/api/status"))
            .and(matchers::header(
                crate::auth::CLUSTER_AUTH_HEADER,
                "cluster-tok",
            ))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/api/status"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let f = fleet(port);
        assert!(!f.settled(PEER).await);
        assert!(f.settled(PEER).await);
    }

    #[tokio::test]
    async fn a_peer_that_is_down_has_not_settled() {
        let port = std::net::TcpListener::bind("[::1]:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(!fleet(port).settled(PEER).await);
    }
}
