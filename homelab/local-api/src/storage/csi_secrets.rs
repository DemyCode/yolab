use anyhow::{Context, Result};
use kube::Client;
use serde_json::{json, Value};

use crate::csi::{CLUSTER_ID, CONFIG_MAP, NODE_SECRET, NS, PROVISIONER_SECRET};
use crate::host::Host;
use crate::routers::backup_common::Backend;

const KERNEL_MOUNT_OPTIONS: &str = "ms_mode=secure";

pub const PROVISIONER_ENTITY: &str = "client.csi-cephfs-provisioner";
pub const NODE_ENTITY: &str = "client.csi-cephfs-node";

pub const PROVISIONER_CAPS: &[&str] = &[
    "mon",
    "allow r",
    "mgr",
    "allow rw",
    "osd",
    "allow rw tag cephfs metadata=*",
];

pub const NODE_CAPS: &[&str] = &[
    "mon",
    "allow r",
    "mgr",
    "allow rw",
    "osd",
    "allow rw tag cephfs *=*",
    "mds",
    "allow rw",
];

fn mon_v2_addrs(dump: &Value) -> Vec<String> {
    let Some(mons) = dump["mons"].as_array() else {
        return Vec::new();
    };
    mons.iter()
        .filter_map(|m| {
            m["public_addrs"]["addrvec"]
                .as_array()?
                .iter()
                .find(|a| a["type"] == "v2")?["addr"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

fn csi_cluster_config_json(mon_addrs: &[String]) -> String {
    json!([{
        "clusterID": CLUSTER_ID,
        "monitors": mon_addrs,
        "cephFS": {"subvolumeGroup": "", "kernelMountOptions": KERNEL_MOUNT_OPTIONS},
    }])
    .to_string()
}

async fn ensure_key<H: Host>(host: &H, entity: &str, caps: &[&str]) -> Result<String> {
    match host.ceph(&["auth", "get-key", entity]).await {
        Ok(key) => return Ok(key.trim().to_string()),
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e).with_context(|| format!("read the key of {entity}")),
    }
    let mut args = vec!["auth", "get-or-create", entity];
    args.extend_from_slice(caps);
    args.extend(["--key_type", super::cephx::AES256K]);
    host.ceph(&args).await?;
    Ok(host
        .ceph(&["auth", "get-key", entity])
        .await?
        .trim()
        .to_string())
}

fn user_id(entity: &str) -> &str {
    entity.strip_prefix("client.").unwrap_or(entity)
}

async fn apply_secret(client: &Client, name: &str, data: Value) -> Result<()> {
    crate::k8s::apply(
        client,
        &json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": name, "namespace": NS},
            "type": "Opaque",
            "stringData": data,
        }),
    )
    .await
    .with_context(|| format!("apply the secret {name}"))
}

pub async fn run<H: Host>(b: &Backend<H>) -> Result<()> {
    let (host, client) = (&b.host, &b.kube);
    if !host.reachable().await {
        tracing::info!("csi-secrets: ceph not reachable yet");
        return Ok(());
    }
    let namespace = crate::k8s::cluster_reference("v1", "Namespace", NS);
    if !crate::k8s::exists(client, &namespace).await? {
        tracing::info!("csi-secrets: the {NS} namespace does not exist yet");
        return Ok(());
    }

    let v2_addrs = mon_v2_addrs(&host.ceph_json(&["mon", "dump"]).await?);
    if v2_addrs.is_empty() {
        tracing::warn!(
            "csi-secrets: no mon address in ceph mon dump — not publishing a broken CSI config"
        );
        return Ok(());
    }

    let provisioner = ensure_key(host, PROVISIONER_ENTITY, PROVISIONER_CAPS).await?;
    let node = ensure_key(host, NODE_ENTITY, NODE_CAPS).await?;

    apply_secret(
        client,
        PROVISIONER_SECRET,
        json!({"adminID": user_id(PROVISIONER_ENTITY), "adminKey": provisioner}),
    )
    .await?;
    apply_secret(
        client,
        NODE_SECRET,
        json!({"adminID": user_id(NODE_ENTITY), "adminKey": node}),
    )
    .await?;
    crate::k8s::apply(
        client,
        &json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": CONFIG_MAP, "namespace": NS},
            "data": {"config.json": csi_cluster_config_json(&v2_addrs)},
        }),
    )
    .await
    .with_context(|| format!("apply the {CONFIG_MAP} configmap"))?;

    tracing::info!(
        "csi-secrets: published Ceph credentials for mons {}",
        v2_addrs.join(",")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};

    const NS_PATH: &str = "/api/v1/namespaces/ceph-csi";

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
    fn mon_v2_addrs_picks_only_the_v2_endpoint_because_msgr1_cannot_encrypt() {
        let d = dump_with(&[("yolab-n1", "fd00:cafe::1")]);
        assert_eq!(mon_v2_addrs(&d), vec!["[fd00:cafe::1]:3300"]);
    }

    #[test]
    fn mon_v2_addrs_covers_every_mon() {
        let d = dump_with(&[("yolab-n1", "fd00:cafe::1"), ("yolab-n2", "fd00:cafe::2")]);
        assert_eq!(mon_v2_addrs(&d).len(), 2);
    }

    #[test]
    fn mon_v2_addrs_is_empty_on_an_unreadable_dump() {
        assert!(mon_v2_addrs(&Value::Null).is_empty());
        assert!(mon_v2_addrs(&json!({"mons": []})).is_empty());
    }

    #[test]
    fn the_csi_config_names_the_cluster_and_asks_for_encrypted_mounts() {
        let cfg: Value =
            serde_json::from_str(&csi_cluster_config_json(&["[fd00::1]:3300".into()])).unwrap();
        assert_eq!(cfg[0]["clusterID"], "yolab");
        assert_eq!(cfg[0]["monitors"][0], "[fd00::1]:3300");
        assert_eq!(cfg[0]["cephFS"]["kernelMountOptions"], "ms_mode=secure");
    }

    fn ceph_ok() -> FakeHost {
        FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "ceph mon dump",
                &dump_with(&[("yolab-n1", "fd00:cafe::1")]).to_string(),
            )
            .ok("ceph auth get-key client.csi-cephfs-provisioner", "provkey")
            .ok("ceph auth get-key client.csi-cephfs-node", "nodekey")
    }

    async fn cluster_up() -> (wiremock::MockServer, Client) {
        let (server, kube) = api_server().await;
        serve(&server, NS_PATH, 200, json!({ "metadata": { "name": NS } })).await;
        accept_patches(&server).await;
        (server, kube)
    }

    fn named(patches: &[Value], name: &str) -> Value {
        patches
            .iter()
            .find(|p| p["metadata"]["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} was not published"))
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
    async fn waits_for_the_namespace_the_manifests_create() {
        let (server, kube) = api_server().await;
        serve(&server, NS_PATH, 404, status(404, "NotFound")).await;
        let b = Backend {
            kube,
            host: FakeHost::new().ok("ceph -s", ""),
        };
        run(&b).await.unwrap();
        assert!(patched(&server).await.is_empty());
        assert!(!b.host.ran("ceph auth"));
    }

    #[tokio::test]
    async fn an_unanswering_kubernetes_is_an_error_not_a_quiet_skip() {
        let b = Backend {
            kube: crate::k8s::testing::unreachable(),
            host: FakeHost::new().ok("ceph -s", ""),
        };
        assert!(run(&b).await.is_err());
    }

    #[tokio::test]
    async fn refuses_to_publish_with_no_mon_address() {
        let (server, kube) = cluster_up().await;
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("ceph mon dump", r#"{"mons":[]}"#);
        run(&Backend { kube, host }).await.unwrap();
        assert!(patched(&server).await.is_empty());
    }

    #[tokio::test]
    async fn publishes_both_credentials_and_the_csi_config_and_no_static_volume_secret() {
        let (server, kube) = cluster_up().await;
        let b = Backend {
            kube,
            host: ceph_ok(),
        };
        run(&b).await.unwrap();

        let patches = patched(&server).await;
        let prov = named(&patches, "csi-cephfs-provisioner");
        assert_eq!(prov["type"], "Opaque");
        assert_eq!(prov["stringData"]["adminID"], "csi-cephfs-provisioner");
        assert_eq!(prov["stringData"]["adminKey"], "provkey");
        let node = named(&patches, "csi-cephfs-node");
        assert_eq!(node["stringData"]["adminID"], "csi-cephfs-node");
        assert_eq!(node["stringData"]["adminKey"], "nodekey");
        assert!(patches
            .iter()
            .all(|p| p["metadata"]["name"] != "yolab-cephfs-static"));
        let config = named(&patches, "ceph-csi-config");
        let parsed: Value =
            serde_json::from_str(config["data"]["config.json"].as_str().unwrap()).unwrap();
        assert_eq!(parsed[0]["monitors"][0], "[fd00:cafe::1]:3300");
        assert!(!b.host.ran("auth get-or-create"));
        assert!(!b.host.ran("client.admin"));
    }

    #[tokio::test]
    async fn a_missing_key_is_created_as_aes256k() {
        let (_server, kube) = cluster_up().await;
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "ceph mon dump",
                &dump_with(&[("yolab-n1", "fd00:cafe::1")]).to_string(),
            )
            .fail(
                "ceph auth get-key client.csi-cephfs-provisioner",
                "Error ENOENT: failed to find client.csi-cephfs-provisioner in keyring",
            )
            .ok("ceph auth get-key client.csi-cephfs-provisioner", "fresh")
            .ok("ceph auth get-or-create client.csi-cephfs-provisioner", "")
            .ok("ceph auth get-key client.csi-cephfs-node", "nodekey");
        let b = Backend { kube, host };
        run(&b).await.unwrap();
        assert!(b.host.ran(
            "auth get-or-create client.csi-cephfs-provisioner mon allow r mgr allow rw osd allow rw tag cephfs metadata=* --key_type aes256k"
        ));
    }

    #[tokio::test]
    async fn an_unreadable_key_is_an_error_never_a_new_key() {
        let (_server, kube) = cluster_up().await;
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok(
                "ceph mon dump",
                &dump_with(&[("yolab-n1", "fd00:cafe::1")]).to_string(),
            )
            .fail(
                "ceph auth get-key client.csi-cephfs-provisioner",
                "RADOS timed out",
            );
        let b = Backend { kube, host };
        assert!(run(&b).await.is_err());
        assert!(!b.host.ran("auth get-or-create"));
    }
}
