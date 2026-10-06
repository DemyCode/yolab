use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;

use crate::auth::CLUSTER_AUTH_HEADER;
use crate::runtime::{Controller, Ctx, Scope, Tick};

use super::{default_path, locked, Store};

const INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const SYNC_PATH: &str = "/api/store/v2/sync";

fn readable(body: &[u8]) -> Result<Store, (StatusCode, String)> {
    readable_as(&crate::system::hostname(), body)
}

fn readable_as(node: &str, body: &[u8]) -> Result<Store, (StatusCode, String)> {
    Store::load(node, body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("this is not a readable desired-state document: {e}"),
        )
    })
}

pub async fn handler(body: Bytes) -> Result<Vec<u8>, (StatusCode, String)> {
    let mut incoming = readable(&body)?;

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
        let client = crate::http::client();
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
    client: &crate::http::Client,
    peer: &str,
    port: u16,
    token: &str,
) -> anyhow::Result<()> {
    exchange_into(
        client,
        peer,
        port,
        token,
        super::shared(),
        &crate::system::hostname(),
        &default_path(),
    )
    .await
}

fn lock(store: &Mutex<Store>) -> std::sync::MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn exchange_into(
    client: &crate::http::Client,
    peer: &str,
    port: u16,
    token: &str,
    store: &Mutex<Store>,
    node: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let ours = lock(store).save();

    let response = client
        .post(crate::http::peer_url(peer, port, SYNC_PATH))
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

    let mut incoming =
        readable_as(node, &theirs).map_err(|(_, why)| anyhow::anyhow!("{peer}: {why}"))?;
    let mut ours = lock(store);
    if ours.merge(&mut incoming)? {
        if let Err(e) = ours.persist(path) {
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

    #[test]
    fn a_clean_document_is_accepted() {
        let mut clean = Store::new("node9");
        clean
            .set_disk_intent("node9", "wwn-z", DiskIntent::On)
            .unwrap();
        assert!(readable(&clean.save()).is_ok());
    }

    #[test]
    fn machines_call_a_sync_route_that_is_actually_served() {
        assert!(crate::surface::ROUTE_TABLE
            .iter()
            .any(|&(path, methods)| path == SYNC_PATH && methods.contains(&"POST")));
    }

    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    fn with_disk(node: &str, disk: &str) -> Store {
        let mut s = Store::new(node);
        s.set_disk_intent(node, disk, DiskIntent::On).unwrap();
        s
    }

    async fn peer_replying(status: u16, reply: Vec<u8>) -> (MockServer, u16) {
        let (server, port) = crate::testkit::peer().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path(SYNC_PATH))
            .and(matchers::header(CLUSTER_AUTH_HEADER, "cluster-tok"))
            .respond_with(ResponseTemplate::new(status).set_body_bytes(reply))
            .mount(&server)
            .await;
        (server, port)
    }

    async fn exchange_with(port: u16, store: &Mutex<Store>, path: &Path) -> anyhow::Result<()> {
        exchange_into(
            &crate::testkit::http(),
            crate::testkit::PEER,
            port,
            "cluster-tok",
            store,
            "node1",
            path,
        )
        .await
    }

    #[tokio::test]
    async fn an_exchange_sends_our_choices_and_keeps_the_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");
        let (server, port) = peer_replying(200, with_disk("node2", "wwn-b").save()).await;
        let store = Mutex::new(with_disk("node1", "wwn-a"));

        exchange_with(port, &store, &path).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0].body;
        assert_eq!(
            Store::load("node2", sent)
                .unwrap()
                .disk_claims()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(lock(&store).disk_claims().unwrap().len(), 2);
        let on_disk = Store::open("node1", &path).unwrap();
        assert_eq!(on_disk.disk_claims().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_reply_that_adds_nothing_is_not_written_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");
        let mut ours = with_disk("node1", "wwn-a");
        let (_server, port) = peer_replying(200, ours.save()).await;
        let store = Mutex::new(ours);

        exchange_with(port, &store, &path).await.unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_peer_that_refuses_the_exchange_changes_nothing_here() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");
        let (_server, port) = peer_replying(401, with_disk("node2", "wwn-b").save()).await;
        let store = Mutex::new(with_disk("node1", "wwn-a"));

        let err = exchange_with(port, &store, &path).await.unwrap_err();

        assert!(err.to_string().contains("401"));
        assert_eq!(lock(&store).disk_claims().unwrap().len(), 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn garbage_from_a_peer_is_an_error_not_a_merge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.automerge");
        let (_server, port) = peer_replying(200, b"<html>proxy error</html>".to_vec()).await;
        let store = Mutex::new(with_disk("node1", "wwn-a"));

        let err = exchange_with(port, &store, &path).await.unwrap_err();

        assert!(err
            .to_string()
            .contains("not a readable desired-state document"));
        assert_eq!(lock(&store).disk_claims().unwrap().len(), 1);
    }
}
