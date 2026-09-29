use std::time::Duration;

use axum::{extract::State, Json};

use crate::{auth::CLUSTER_AUTH_HEADER, host::Host, AppState};

const REBOOT_DELAY_SECS: u64 = 3;

fn spawn_reboot() {
    spawn_reboot_on(crate::host::RealHost);
}

fn spawn_reboot_on<H: Host + 'static>(host: H) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(REBOOT_DELAY_SECS)).await;
        tracing::warn!("rebooting this machine now");
        match host.systemctl(&["reboot"]).await {
            Ok(o) if o.success => {}
            Ok(o) => tracing::error!("reboot did not start: {}", o.stderr.trim()),
            Err(e) => tracing::error!("reboot did not start: {e}"),
        }
    })
}

pub async fn reboot() -> Json<serde_json::Value> {
    spawn_reboot();
    Json(serde_json::json!({ "status": "rebooting" }))
}

const SETTLE_TIMEOUT: Duration = Duration::from_secs(900);
const SETTLE_POLL: Duration = Duration::from_secs(10);

struct RebootFleet {
    client: crate::http::Client,
    port: u16,
    token: String,
}

impl crate::runtime::fleet::Fleet for RebootFleet {
    async fn act(&self, node: &str) -> anyhow::Result<()> {
        let r = self
            .client
            .post(crate::http::peer_url(node, self.port, "/api/system/reboot"))
            .header(CLUSTER_AUTH_HEADER, &self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        anyhow::ensure!(r.status().is_success(), "{node} answered {}", r.status());
        Ok(())
    }

    async fn settled(&self, node: &str) -> bool {
        self.client
            .get(crate::http::peer_url(node, self.port, "/api/status"))
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
    let nodes = match state.kube.client().await {
        Ok(client) => crate::k8s::nodes(&client).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let peers = crate::runtime::fleet::order(&crate::k8s::peer_ipv6(&nodes, &self_ip), &self_ip);

    let fleet = RebootFleet {
        client: crate::http::client(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::runtime::fleet::Fleet;
    use crate::testkit::{peer, PEER};
    use wiremock::{matchers, Mock, ResponseTemplate};

    #[tokio::test(start_paused = true)]
    async fn the_reboot_waits_so_the_answer_reaches_the_caller_first() {
        let host = FakeHost::new().ok("systemctl reboot", "");
        let asked = tokio::time::Instant::now();
        spawn_reboot_on(host.clone()).await.unwrap();
        assert!(asked.elapsed() >= Duration::from_secs(REBOOT_DELAY_SECS));
        assert!(host.ran("systemctl reboot"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reboot_that_systemd_refuses_does_not_panic_the_task() {
        let host = FakeHost::new().fail("systemctl reboot", "Access denied");
        spawn_reboot_on(host.clone()).await.unwrap();
        assert!(host.ran("systemctl reboot"));
    }

    fn fleet(port: u16) -> RebootFleet {
        RebootFleet {
            client: crate::testkit::http(),
            port,
            token: "cluster-tok".into(),
        }
    }

    #[tokio::test]
    async fn a_peer_is_asked_to_reboot_with_the_cluster_token() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/system/reboot"))
            .and(matchers::header(CLUSTER_AUTH_HEADER, "cluster-tok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        fleet(port).act(PEER).await.unwrap();
    }

    #[tokio::test]
    async fn a_peer_that_refuses_to_reboot_is_an_error() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/api/system/reboot"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = fleet(port).act(PEER).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn a_rebooting_peer_has_settled_only_once_its_api_answers() {
        let (server, port) = peer().await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/api/status"))
            .and(matchers::header(CLUSTER_AUTH_HEADER, "cluster-tok"))
            .respond_with(ResponseTemplate::new(502))
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
}
