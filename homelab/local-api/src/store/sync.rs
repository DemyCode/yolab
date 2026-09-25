use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;

use crate::auth::CLUSTER_AUTH_HEADER;
use crate::runtime::{Controller, Ctx, Scope, Tick};

use super::{default_path, locked, Store};

const INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handler(body: Bytes) -> Result<Vec<u8>, (StatusCode, String)> {
    let mut incoming = Store::load(&crate::system::hostname(), &body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("this is not a readable desired-state document: {e}"),
        )
    })?;

    let mut ours = locked();
    let changed = ours.merge(&mut incoming).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the document could not be merged: {e}"),
        )
    })?;
    let reply = ours.save();
    if changed {
        if let Err(e) = ours.persist(&default_path()) {
            tracing::warn!("a peer's choices were merged but not saved to disk ({e})");
        }
    }
    Ok(reply)
}

pub struct StoreSyncController;

impl Controller for StoreSyncController {
    fn name(&self) -> &'static str {
        "store-sync"
    }

    fn scope(&self) -> Scope {
        Scope::Node
    }

    fn interval(&self) -> Duration {
        INTERVAL
    }

    async fn reconcile(&self, _ctx: &Ctx) -> anyhow::Result<Tick> {
        let cfg = crate::config::Config::from_env();
        let peers = crate::mesh::peer_addresses(&cfg.node_ipv6).await;
        if peers.is_empty() {
            return Ok(Tick::Idle(
                "there are no other machines to exchange choices with".into(),
            ));
        }

        let token = cfg.cluster_token();
        let client = reqwest::Client::new();
        let mut reached = 0usize;
        for peer in &peers {
            match exchange(&client, peer, cfg.port, &token).await {
                Ok(()) => reached += 1,
                Err(e) => tracing::debug!("store-sync: {peer} did not answer ({e})"),
            }
        }

        if reached == 0 {
            return Ok(Tick::Idle(format!(
                "none of the {} other machines answered — carrying on with what this one knows",
                peers.len()
            )));
        }
        Ok(Tick::Done)
    }
}

async fn exchange(
    client: &reqwest::Client,
    peer: &str,
    port: u16,
    token: &str,
) -> anyhow::Result<()> {
    let ours = locked().save();

    let response = client
        .post(format!("http://[{peer}]:{port}/api/store/sync"))
        .header(CLUSTER_AUTH_HEADER, token)
        .body(ours)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "{peer} answered {}",
        response.status()
    );
    let theirs = response.bytes().await?;

    let mut incoming = Store::load(&crate::system::hostname(), &theirs)?;
    let mut ours = locked();
    if ours.merge(&mut incoming)? {
        if let Err(e) = ours.persist(&default_path()) {
            tracing::warn!("{peer}'s choices were merged but not saved to disk ({e})");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DiskIntent;

    #[test]
    fn an_exchange_is_a_whole_document_in_each_direction() {
        let mut ours = Store::new("node1");
        ours.set_disk_intent("node1", "wwn-a", DiskIntent::On)
            .unwrap();
        let sent = ours.save();

        let mut theirs = Store::new("node2");
        theirs
            .set_disk_intent("node2", "wwn-b", DiskIntent::On)
            .unwrap();

        let mut received = Store::load("node2", &sent).unwrap();
        theirs.merge(&mut received).unwrap();
        let replied = theirs.save();

        let mut back = Store::load("node1", &replied).unwrap();
        ours.merge(&mut back).unwrap();

        assert_eq!(ours.disk_claims().unwrap(), theirs.disk_claims().unwrap());
        assert_eq!(ours.disk_claims().unwrap().len(), 2);
    }

    #[test]
    fn a_body_that_is_not_a_document_is_refused_rather_than_merged() {
        assert!(Store::load("node1", b"not a document").is_err());
    }
}
