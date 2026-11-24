use super::*;
use crate::store::DiskIntent;

fn twin(actor: &automerge::ActorId, disk: &str) -> Vec<u8> {
    let mut s = Store::new("node1");
    s.doc.set_actor(actor.clone());
    s.set_disk_intent("node1", disk, DiskIntent::On).unwrap();
    s.save()
}

fn claims(bytes: &[u8]) -> usize {
    Store::load("node1", bytes)
        .unwrap()
        .disk_claims()
        .unwrap()
        .len()
}

#[tokio::test]
async fn a_peers_document_is_merged_saved_and_answered_with_ours() {
    let reply = handler(Bytes::from(with_disk("node2", "wwn-b").save()))
        .await
        .unwrap();
    assert_eq!(claims(&reply), 1);
    assert_eq!(claims(&std::fs::read(default_path()).unwrap()), 1);
}

#[tokio::test]
async fn a_document_that_adds_nothing_is_answered_but_not_saved() {
    handler(Bytes::from(Store::new("node2").save()))
        .await
        .unwrap();
    assert!(!default_path().exists());
}

#[tokio::test]
async fn a_body_that_is_not_a_document_is_a_bad_request() {
    let (status, why) = handler(Bytes::from_static(b"<html>")).await.unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        why.starts_with("this is not a readable desired-state document: "),
        "{why}"
    );
}

#[tokio::test]
async fn a_merge_that_cannot_be_saved_is_still_answered() {
    std::fs::create_dir_all(default_path()).unwrap();
    let reply = handler(Bytes::from(with_disk("node2", "wwn-b").save()))
        .await
        .unwrap();
    assert_eq!(claims(&reply), 1);
}

#[tokio::test]
async fn two_histories_from_one_writer_cannot_be_merged() {
    let actor = automerge::ActorId::random();
    *locked() = Store::load("node1", &twin(&actor, "wwn-a")).unwrap();
    let (status, why) = handler(Bytes::from(twin(&actor, "wwn-b")))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        why.starts_with("the document could not be merged: "),
        "{why}"
    );

    let (_server, port) = peer_replying(200, twin(&actor, "wwn-c")).await;
    let store = Mutex::new(Store::load("node1", &twin(&actor, "wwn-a")).unwrap());
    let dir = tempfile::tempdir().unwrap();
    assert!(
        exchange_with(port, &store, &dir.path().join("store.automerge"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_exchange_with_a_peer_that_is_down_or_hangs_up_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.automerge");
    let store = Mutex::new(with_disk("node1", "wwn-a"));
    assert!(exchange_with(crate::testkit::closed_port(), &store, &path)
        .await
        .is_err());

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let read = conn.read(&mut buf).await.unwrap();
        assert!(read > 0);
        conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\npartial")
            .await
            .unwrap();
    });
    assert!(exchange_with(port, &store, &path).await.is_err());
}

#[tokio::test]
async fn choices_merged_from_a_peer_are_kept_even_when_the_disk_refuses_them() {
    let (_server, port) = peer_replying(200, with_disk("node2", "wwn-b").save()).await;
    let store = Mutex::new(with_disk("node1", "wwn-a"));
    let unwritable = std::path::Path::new("/dev/null/store.automerge");
    exchange_with(port, &store, unwritable).await.unwrap();
    assert_eq!(lock(&store).disk_claims().unwrap().len(), 2);
}

fn controller(port: u16) -> (tempfile::TempDir, StoreSyncController) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "[node.k3s]\ntoken = \"k3s-tok\"\n").unwrap();
    let mut config = crate::config::Config::for_test(&file);
    config.port = port;
    config.node_ipv6 = "fd00::1".into();
    (dir, StoreSyncController { config })
}

async fn cluster_with_peer() -> (MockServer, Ctx) {
    let (server, kube) = crate::k8s::testing::api_server().await;
    crate::k8s::testing::serve(
        &server,
        "/api/v1/nodes",
        200,
        crate::k8s::testing::list(
            "Node",
            vec![serde_json::json!({
                "metadata": { "name": "node2" },
                "status": { "addresses": [{ "type": "InternalIP", "address": crate::testkit::PEER }] },
            })],
        ),
    )
    .await;
    let ctx = Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools {
            kube: crate::k8s::Kube::with(kube),
            ..crate::tools::Tools::unreachable()
        },
    };
    (server, ctx)
}

#[tokio::test]
async fn the_sync_controller_exchanges_with_every_other_machine_twice_a_minute() {
    let (_dir, c) = controller(crate::testkit::closed_port());
    assert_eq!(
        (c.name(), c.scope(), c.interval()),
        ("store-sync", Scope::Node, INTERVAL)
    );
    let alone = Ctx {
        node: "node1".into(),
        tools: crate::tools::Tools::unreachable(),
    };
    assert_eq!(
        c.converge(&alone).await.unwrap(),
        Tick::Idle("there are no other machines to exchange choices with".into())
    );

    let (_cluster, ctx) = cluster_with_peer().await;
    assert_eq!(
        c.converge(&ctx).await.unwrap(),
        Tick::Idle(
            "none of the 1 other machines answered — carrying on with what this one knows".into()
        )
    );

    let (peer, port) = crate::testkit::peer().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path(SYNC_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(Store::new("node2").save()))
        .mount(&peer)
        .await;
    let (_dir, c) = controller(port);
    assert_eq!(c.converge(&ctx).await.unwrap(), Tick::Done);
}
