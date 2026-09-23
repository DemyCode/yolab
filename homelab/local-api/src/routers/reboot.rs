use std::time::Duration;

use axum::{extract::State, Json};

use crate::{auth::CLUSTER_AUTH_HEADER, kubectl, AppState};

const REBOOT_DELAY_SECS: u64 = 3;

fn spawn_reboot() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(REBOOT_DELAY_SECS)).await;
        tracing::warn!("rebooting this machine now");
        let _ = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .stdin(std::process::Stdio::null())
            .status()
            .await;
    });
}

pub async fn reboot() -> Json<serde_json::Value> {
    spawn_reboot();
    Json(serde_json::json!({ "status": "rebooting" }))
}

const SETTLE_TIMEOUT: Duration = Duration::from_secs(900);
const SETTLE_POLL: Duration = Duration::from_secs(10);

struct RebootFleet {
    client: reqwest::Client,
    port: u16,
    token: String,
}

impl crate::runtime::fleet::Fleet for RebootFleet {
    async fn act(&self, node: &str) -> anyhow::Result<()> {
        let r = self
            .client
            .post(format!("http://[{node}]:{}/api/system/reboot", self.port))
            .header(CLUSTER_AUTH_HEADER, &self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        anyhow::ensure!(r.status().is_success(), "{node} answered {}", r.status());
        Ok(())
    }

    async fn settled(&self, node: &str) -> bool {
        self.client
            .get(format!("http://[{node}]:{}/api/status", self.port))
            .header(CLUSTER_AUTH_HEADER, &self.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

pub async fn reboot_all(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();
    let nodes = kubectl::get_nodes().await.unwrap_or_default();
    let peers = crate::runtime::fleet::order(&kubectl::peer_ipv6(&nodes, &self_ip), &self_ip);

    let fleet = RebootFleet {
        client: reqwest::Client::new(),
        port: cfg.port,
        token: cfg.cluster_token(),
    };

    tokio::spawn(async move {
        let rolled =
            crate::runtime::fleet::rolling(&fleet, &peers, SETTLE_TIMEOUT, SETTLE_POLL).await;
        if let Some(stuck) = rolled.stopped_at.as_deref() {
            tracing::error!(
                "reboot: stopped at {stuck} ({}) — {:?} were left alone and this machine will \
                 not restart either",
                rolled.why.unwrap_or_default(),
                rolled.skipped
            );
            return;
        }
        tracing::warn!(
            "reboot: {:?} are back, restarting this machine now",
            rolled.done
        );
        spawn_reboot();
    });

    Json(serde_json::json!({
        "status": "rebooting",
        "order": "one machine at a time, this one last",
    }))
}
