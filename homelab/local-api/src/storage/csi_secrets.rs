use anyhow::{Context, Result};
use serde_json::{json, Value};

use kube::Client;

use crate::error::Outcome;
use crate::host::Host;
use crate::routers::backup_common::Backend;

const NS: &str = "rook-ceph";

fn mon_v1_addrs(dump: &Value) -> Vec<String> {
    let Some(mons) = dump["mons"].as_array() else {
        return Vec::new();
    };
    mons.iter()
        .filter_map(|m| {
            m["public_addrs"]["addrvec"]
                .as_array()?
                .iter()
                .find(|a| a["type"] == "v1")?["addr"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

fn mon_endpoints(dump: &Value) -> String {
    let Some(mons) = dump["mons"].as_array() else {
        return String::new();
    };
    mons.iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            let addr = m["public_addrs"]["addrvec"]
                .as_array()?
                .iter()
                .find(|a| a["type"] == "v1")?["addr"]
                .as_str()?;
            Some(format!("{name}={addr}"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn csi_cluster_config_json(mon_addrs: &[String]) -> String {
    json!([{
        "clusterID": NS,
        "monitors": mon_addrs,
        "cephFS": {"subvolumeGroup": ""},
        "rbd": {},
    }])
    .to_string()
}

fn needs_type_fix(current_type: &str) -> bool {
    !current_type.is_empty() && current_type != "kubernetes.io/rook"
}

async fn replace_if_wrong_type(client: &Client, name: &str) {
    let secret = crate::k8s::reference("v1", "Secret", NS, name);
    let have = match crate::k8s::get(client, &secret).await {
        Ok(found) => found
            .and_then(|s| s["type"].as_str().map(str::to_string))
            .unwrap_or_default(),
        Err(e) => {
            tracing::debug!("csi-secrets: could not read {name} ({e:#})");
            String::new()
        }
    };
    if needs_type_fix(&have) {
        tracing::warn!(
            "csi-secrets: secret {name} has type {have}, recreating as kubernetes.io/rook"
        );
        crate::k8s::delete_if_present(client, &secret)
            .await
            .warn_on_err(format!(
                "csi-secrets: delete {name} to recreate it with the right type"
            ));
    }
}

async fn ensure_key<H: Host>(host: &H, entity: &str, caps: &[&str]) -> Result<String> {
    if host.ceph(&["auth", "get-key", entity]).await.is_err() {
        let mut args = vec!["auth", "get-or-create", entity];
        args.extend_from_slice(caps);
        host.ceph(&args).await?;
    }
    Ok(host
        .ceph(&["auth", "get-key", entity])
        .await?
        .trim()
        .to_string())
}

async fn apply_rook_secret(
    client: &Client,
    name: &str,
    id_key: &str,
    id: &str,
    secret_key: &str,
    secret: &str,
) -> Result<()> {
    replace_if_wrong_type(client, name).await;
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": name, "namespace": NS},
        "type": "kubernetes.io/rook",
        "stringData": {id_key: id, secret_key: secret},
    });
    crate::k8s::apply(client, &manifest).await
}

async fn apply_rook_ceph_mon_secret(client: &Client, fsid: &str, admin_key: &str) -> Result<()> {
    replace_if_wrong_type(client, "rook-ceph-mon").await;
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "rook-ceph-mon", "namespace": NS},
        "type": "kubernetes.io/rook",
        "stringData": {
            "cluster-name": NS,
            "fsid": fsid,
            "admin-secret": "admin-secret",
            "mon-secret": "mon-secret",
            "ceph-username": "client.admin",
            "ceph-secret": admin_key,
        },
    });
    crate::k8s::apply(client, &manifest).await
}

async fn apply_mon_endpoints_configmap(
    client: &Client,
    mon_endpoints: &str,
    csi_cfg: &str,
) -> Result<()> {
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "rook-ceph-mon-endpoints", "namespace": NS},
        "data": {
            "data": mon_endpoints,
            "maxMonId": "0",
            "mapping": "{}",
            "csi-cluster-config-json": csi_cfg,
        },
    });
    crate::k8s::apply(client, &manifest).await
}

async fn apply_csi_config_map(client: &Client, csi_cfg: &str) -> Result<()> {
    let config = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "rook-ceph-csi-config", "namespace": NS},
        "data": {"csi-cluster-config-json": csi_cfg},
    });
    match crate::k8s::merge_patch(client, &config).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => crate::k8s::apply(client, &config).await,
        other => other,
    }
}

pub async fn run<H: Host>(b: &Backend<H>) -> Result<()> {
    let (host, client) = (&b.host, &b.kube);
    if !host.reachable().await {
        tracing::info!("csi-secrets: ceph not reachable yet");
        return Ok(());
    }
    let namespace = crate::k8s::cluster_reference("v1", "Namespace", NS);
    if !matches!(crate::k8s::exists(client, &namespace).await, Ok(true)) {
        tracing::info!("csi-secrets: kubernetes not reachable yet (or namespace missing)");
        return Ok(());
    }

    let fsid = host.ceph(&["fsid"]).await?.trim().to_string();

    let dump = host
        .ceph_json(&["mon", "dump"])
        .await
        .unwrap_or(Value::Null);
    let v1_addrs = mon_v1_addrs(&dump);
    if v1_addrs.is_empty() {
        tracing::warn!(
            "csi-secrets: no mon address in ceph mon dump — not publishing a broken CSI config"
        );
        return Ok(());
    }
    let endpoints = mon_endpoints(&dump);
    let csi_cfg = csi_cluster_config_json(&v1_addrs);

    let cephfs_prov = ensure_key(
        host,
        "client.csi-cephfs-provisioner",
        &[
            "mon",
            "allow r",
            "mgr",
            "allow rw",
            "osd",
            "allow rw tag cephfs metadata=*",
        ],
    )
    .await?;
    let cephfs_node = ensure_key(
        host,
        "client.csi-cephfs-node",
        &[
            "mon",
            "allow r",
            "mgr",
            "allow rw",
            "osd",
            "allow rw tag cephfs *=*",
            "mds",
            "allow rw",
        ],
    )
    .await?;

    apply_rook_secret(
        client,
        "rook-csi-cephfs-provisioner",
        "adminID",
        "csi-cephfs-provisioner",
        "adminKey",
        &cephfs_prov,
    )
    .await?;
    apply_rook_secret(
        client,
        "rook-csi-cephfs-node",
        "adminID",
        "csi-cephfs-node",
        "adminKey",
        &cephfs_node,
    )
    .await?;

    let admin_key = host.ceph(&["auth", "get-key", "client.admin"]).await?;
    apply_rook_ceph_mon_secret(client, &fsid, admin_key.trim())
        .await
        .context("apply rook-ceph-mon secret")?;

    apply_mon_endpoints_configmap(client, &endpoints, &csi_cfg)
        .await
        .context("apply rook-ceph-mon-endpoints configmap")?;
    apply_csi_config_map(client, &csi_cfg)
        .await
        .context("apply rook-ceph-csi-config configmap")?;

    tracing::info!("csi-secrets: published Ceph credentials for fsid {fsid}, mons {endpoints}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};

    fn dump_with(mons: &[(&str, &str)]) -> Value {
        json!({"mons": mons.iter().map(|(name, addr)| json!({
            "name": name,
            "public_addrs": {"addrvec": [
                {"type": "v2", "addr": format!("[{addr}]:3300")},
                {"type": "v1", "addr": format!("[{addr}]:6789")},
            ]},
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn mon_v1_addrs_picks_only_the_v1_endpoint() {
        let d = dump_with(&[("yolab-n1", "fd00:cafe::1")]);
        assert_eq!(mon_v1_addrs(&d), vec!["[fd00:cafe::1]:6789"]);
    }

    #[test]
    fn mon_v1_addrs_covers_every_mon() {
        let d = dump_with(&[("yolab-n1", "fd00:cafe::1"), ("yolab-n2", "fd00:cafe::2")]);
        assert_eq!(mon_v1_addrs(&d).len(), 2);
    }

    #[test]
    fn mon_v1_addrs_is_empty_on_an_unreadable_dump() {
        assert!(mon_v1_addrs(&Value::Null).is_empty());
        assert!(mon_v1_addrs(&json!({"mons": []})).is_empty());
    }

    #[test]
    fn mon_endpoints_joins_name_equals_addr_pairs() {
        let d = dump_with(&[("yolab-n1", "fd00:cafe::1"), ("yolab-n2", "fd00:cafe::2")]);
        assert_eq!(
            mon_endpoints(&d),
            "yolab-n1=[fd00:cafe::1]:6789,yolab-n2=[fd00:cafe::2]:6789"
        );
    }

    #[test]
    fn csi_cluster_config_names_the_cluster_id_and_carries_the_monitors() {
        let cfg: Value =
            serde_json::from_str(&csi_cluster_config_json(&["[fd00::1]:6789".into()])).unwrap();
        assert_eq!(cfg[0]["clusterID"], "rook-ceph");
        assert_eq!(cfg[0]["monitors"][0], "[fd00::1]:6789");
    }

    #[test]
    fn needs_type_fix_ignores_absent_and_matching_types() {
        assert!(!needs_type_fix(""));
        assert!(!needs_type_fix("kubernetes.io/rook"));
        assert!(needs_type_fix("Opaque"));
    }

    const NS_PATH: &str = "/api/v1/namespaces/rook-ceph";

    fn ceph_ok() -> FakeHost {
        FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", "11111111-2222-3333-4444-555555555555\n")
            .ok(
                "ceph mon dump",
                &dump_with(&[("yolab-n1", "fd00:cafe::1")]).to_string(),
            )
            .ok(
                "ceph auth get-key client.csi-cephfs-provisioner",
                "cephfsprovkey",
            )
            .ok("ceph auth get-key client.csi-cephfs-node", "cephfsnodekey")
            .ok("ceph auth get-key client.admin", "adminkey")
    }

    async fn cluster_up() -> (wiremock::MockServer, Client) {
        let (server, kube) = api_server().await;
        serve(&server, NS_PATH, 200, json!({ "metadata": { "name": NS } })).await;
        accept_patches(&server).await;
        (server, kube)
    }

    async fn applied_names(server: &wiremock::MockServer) -> Vec<String> {
        patched(server)
            .await
            .iter()
            .filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn does_nothing_while_ceph_is_unreachable() {
        let (server, kube) = cluster_up().await;
        let b = Backend {
            kube,
            host: FakeHost::new().fail("ceph -s", "unreachable"),
        };
        run(&b).await.unwrap();
        assert!(patched(&server).await.is_empty());
    }

    #[tokio::test]
    async fn does_nothing_before_kubernetes_answers() {
        let (server, kube) = api_server().await;
        serve(&server, NS_PATH, 503, status(503, "ServiceUnavailable")).await;
        let b = Backend {
            kube,
            host: FakeHost::new().ok("ceph -s", ""),
        };
        run(&b).await.unwrap();
        assert!(patched(&server).await.is_empty());
        assert!(!b.host.ran("ceph auth"));
    }

    #[tokio::test]
    async fn refuses_to_publish_with_no_mon_address() {
        let (server, kube) = cluster_up().await;
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", "fsid\n")
            .ok("ceph mon dump", r#"{"mons":[]}"#);
        run(&Backend { kube, host }).await.unwrap();
        assert!(
            patched(&server).await.is_empty(),
            "an empty CSI config must never be published"
        );
    }

    #[tokio::test]
    async fn publishes_the_cephfs_secrets_and_both_configmaps() {
        let (server, kube) = cluster_up().await;
        run(&Backend {
            kube,
            host: ceph_ok(),
        })
        .await
        .unwrap();

        let names = applied_names(&server).await;
        for name in [
            "rook-csi-cephfs-provisioner",
            "rook-csi-cephfs-node",
            "rook-ceph-mon",
            "rook-ceph-mon-endpoints",
            "rook-ceph-csi-config",
        ] {
            assert!(
                names.iter().any(|n| n == name),
                "{name} missing from {names:?}"
            );
        }
        assert!(
            !names.iter().any(|n| n.contains("rbd")),
            "no RBD credential should ever be minted or published: {names:?}"
        );
        let mon = patched(&server)
            .await
            .into_iter()
            .find(|p| p["metadata"]["name"] == "rook-ceph-mon")
            .unwrap();
        assert_eq!(mon["type"], "kubernetes.io/rook");
        assert_eq!(mon["stringData"]["ceph-secret"], "adminkey");
    }

    #[tokio::test]
    async fn ensure_key_reuses_an_existing_key_without_recreating_it() {
        let (_server, kube) = cluster_up().await;
        let b = Backend {
            kube,
            host: ceph_ok(),
        };
        run(&b).await.unwrap();
        assert!(
            !b.host.ran("auth get-or-create"),
            "every ensure_key call above found its key on the first read, none should be created"
        );
    }

    #[tokio::test]
    async fn creates_a_key_that_is_missing() {
        let (_server, kube) = cluster_up().await;
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph fsid", "fsid\n")
            .ok(
                "ceph mon dump",
                &dump_with(&[("yolab-n1", "fd00:cafe::1")]).to_string(),
            )
            .fail(
                "ceph auth get-key client.csi-cephfs-provisioner",
                "not found",
            )
            .ok("ceph auth get-or-create client.csi-cephfs-provisioner", "")
            .ok(
                "ceph auth get-key client.csi-cephfs-provisioner",
                "freshly-minted-key",
            )
            .ok("ceph auth get-key client.csi-cephfs-node", "k")
            .ok("ceph auth get-key client.admin", "adminkey");
        let b = Backend { kube, host };

        run(&b).await.unwrap();

        assert!(b
            .host
            .ran("auth get-or-create client.csi-cephfs-provisioner"));
    }

    #[tokio::test]
    async fn a_secret_of_the_wrong_type_is_deleted_before_being_reapplied() {
        let (server, kube) = cluster_up().await;
        serve(
            &server,
            "/api/v1/namespaces/rook-ceph/secrets/rook-csi-cephfs-provisioner",
            200,
            json!({ "metadata": { "name": "rook-csi-cephfs-provisioner" }, "type": "Opaque" }),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/api/v1/namespaces/rook-ceph/secrets/rook-csi-cephfs-provisioner",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;

        run(&Backend {
            kube,
            host: ceph_ok(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_csi_config_is_created_when_it_does_not_exist_yet() {
        let (server, kube) = api_server().await;
        serve(&server, NS_PATH, 200, json!({ "metadata": { "name": NS } })).await;
        wiremock::Mock::given(wiremock::matchers::method("PATCH"))
            .and(wiremock::matchers::path(
                "/api/v1/namespaces/rook-ceph/configmaps/rook-ceph-csi-config",
            ))
            .and(wiremock::matchers::header(
                "content-type",
                "application/merge-patch+json",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(404).set_body_json(status(404, "NotFound")),
            )
            .mount(&server)
            .await;
        accept_patches(&server).await;

        run(&Backend {
            kube,
            host: ceph_ok(),
        })
        .await
        .unwrap();

        let csi = patched(&server)
            .await
            .into_iter()
            .filter(|p| p["metadata"]["name"] == "rook-ceph-csi-config")
            .count();
        assert_eq!(csi, 2, "the merge was refused, so it was applied whole");
    }
}
