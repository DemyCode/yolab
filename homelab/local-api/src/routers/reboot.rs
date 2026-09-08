//! Rebooting machines, one or all.
//!
//! REBOOTING IS NOT AN UPDATE, and the difference decides how this is written.
//! A `nixos-rebuild` leaves k3s and Ceph running, so `update_all` fires every
//! node in parallel and nothing notices. A reboot takes the whole machine away:
//! its etcd member, its Ceph mon, its OSDs and every pod on it.
//!
//! ORDER IS THE ONE THING THAT CANNOT BE GOT WRONG. This node has to tell the
//! others before it goes down, because a rebooting machine cannot send anything.
//! Reboot self first and the peers simply never hear about it — you get one
//! rebooted node and a button that appears to have half worked.
//!
//! Ceph's side is already handled elsewhere: `yolab-ceph-noout.service` holds
//! the `noout` flag across a reboot, so the cluster does not start rebalancing
//! away from disks that are about to come straight back.
//!
//! What this deliberately does NOT do is roll — reboot one machine, wait for the
//! cluster to be healthy, then the next. That is the safer thing and a much
//! larger one: it needs health gating, a progress model, and a way to stop
//! half-way. "Reboot all machines" as written here is an all-at-once operation,
//! and the UI says so rather than implying an orchestration that does not exist.

use std::time::Duration;

use axum::{extract::State, Json};

use crate::{auth::CLUSTER_AUTH_HEADER, kubectl, AppState};

/// Long enough for the HTTP response to reach the caller, short enough that
/// nobody wonders whether the button worked.
///
/// `systemctl reboot` from inside a request handler kills the connection before
/// the response is written, so the UI sees a network error and cannot tell a
/// successful reboot from a failed one. Detaching the reboot lets this return
/// 200 first and go down afterwards.
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

/// `POST /api/system/reboot` — reboot THIS machine.
///
/// Also the endpoint peers call during a reboot-all, which is why it accepts the
/// cluster token like every other node-to-node route.
pub async fn reboot() -> Json<serde_json::Value> {
    spawn_reboot();
    Json(serde_json::json!({ "status": "rebooting" }))
}

/// `POST /api/system/reboot/all` — reboot every machine in the cluster.
pub async fn reboot_all(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = state.config.clone();
    let self_ip = cfg.node_ipv6.clone();
    let token = cfg.cluster_token();

    let nodes = kubectl::get_nodes().await.unwrap_or_default();
    let peers = kubectl::peer_ipv6(&nodes, &self_ip);

    // AWAITED, not spawned. Every peer must have accepted the request before
    // this node reboots itself — a detached task would race the local reboot
    // and lose, leaving peers that were never told. Bounded so one unreachable
    // machine cannot hold the others up indefinitely.
    let client = reqwest::Client::new();
    let mut asked = Vec::new();
    let mut unreachable = Vec::new();
    for addr in &peers {
        let url = format!("http://[{addr}]:{}/api/system/reboot", cfg.port);
        match client
            .post(&url)
            .header(CLUSTER_AUTH_HEADER, &token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => asked.push(addr.clone()),
            _ => unreachable.push(addr.clone()),
        }
    }

    if !unreachable.is_empty() {
        // Reported, not fatal. A machine that is already down or unreachable
        // does not need rebooting, and refusing to reboot the rest because of
        // it would make the button useless in exactly the situation someone
        // reaches for it.
        tracing::warn!("reboot: could not reach {unreachable:?} — rebooting the rest anyway");
    }

    // Self LAST. See this module's header.
    spawn_reboot();

    Json(serde_json::json!({
        "status": "rebooting",
        "asked": asked,
        "unreachable": unreachable,
    }))
}
