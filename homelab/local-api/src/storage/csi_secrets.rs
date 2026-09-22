use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::host::Host;

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

async fn replace_if_wrong_type<H: Host>(host: &H, name: &str) {
    let have = host
        .kubectl(&["get", "secret", name, "-n", NS, "-o", "jsonpath={.type}"])
        .await
        .unwrap_or_default();
    if needs_type_fix(&have) {
        tracing::warn!(
            "csi-secrets: secret {name} has type {have}, recreating as kubernetes.io/rook"
        );
        host.kubectl(&["delete", "secret", name, "-n", NS, "--ignore-not-found"])
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

async fn apply_rook_secret<H: Host>(
    host: &H,
    name: &str,
    id_key: &str,
    id: &str,
    secret_key: &str,
    secret: &str,
) -> Result<()> {
    replace_if_wrong_type(host, name).await;
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": name, "namespace": NS},
        "type": "kubernetes.io/rook",
        "stringData": {id_key: id, secret_key: secret},
    });
    Ok(host.kubectl_apply(&manifest.to_string()).await?)
}

async fn apply_rook_ceph_mon_secret<H: Host>(host: &H, fsid: &str, admin_key: &str) -> Result<()> {
    replace_if_wrong_type(host, "rook-ceph-mon").await;
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
    Ok(host.kubectl_apply(&manifest.to_string()).await?)
}

async fn apply_mon_endpoints_configmap<H: Host>(
    host: &H,
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
    Ok(host.kubectl_apply(&manifest.to_string()).await?)
}

async fn apply_csi_config_map<H: Host>(host: &H, csi_cfg: &str) -> Result<()> {
    let patch = json!({"data": {"csi-cluster-config-json": csi_cfg}}).to_string();
    if host
        .kubectl(&[
            "patch",
            "configmap",
            "rook-ceph-csi-config",
            "-n",
            NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        .is_err()
    {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "rook-ceph-csi-config", "namespace": NS},
            "data": {"csi-cluster-config-json": csi_cfg},
        });
        host.kubectl_apply(&manifest.to_string()).await?;
    }
    Ok(())
}

pub async fn run<H: Host>(host: &H) -> Result<()> {
    if !host.reachable().await {
        tracing::info!("csi-secrets: ceph not reachable yet");
        return Ok(());
    }
    if host.kubectl(&["get", "ns", NS]).await.is_err() {
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
        host,
        "rook-csi-cephfs-provisioner",
        "adminID",
        "csi-cephfs-provisioner",
        "adminKey",
        &cephfs_prov,
    )
    .await?;
    apply_rook_secret(
        host,
        "rook-csi-cephfs-node",
        "adminID",
        "csi-cephfs-node",
        "adminKey",
        &cephfs_node,
    )
    .await?;

    let admin_key = host.ceph(&["auth", "get-key", "client.admin"]).await?;
    apply_rook_ceph_mon_secret(host, &fsid, admin_key.trim())
        .await
        .context("apply rook-ceph-mon secret")?;

    apply_mon_endpoints_configmap(host, &endpoints, &csi_cfg)
        .await
        .context("apply rook-ceph-mon-endpoints configmap")?;
    apply_csi_config_map(host, &csi_cfg)
        .await
        .context("apply rook-ceph-csi-config configmap")?;

    tracing::info!("csi-secrets: published Ceph credentials for fsid {fsid}, mons {endpoints}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

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

    fn scripted_ok_host() -> FakeHost {
        FakeHost::new()
            .ok("ceph -s", "")
            .ok("kubectl get ns rook-ceph", "")
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
            .ok("kubectl get secret", "")
            .ok("kubectl-apply", "")
            .ok("kubectl patch configmap", "")
    }

    #[tokio::test]
    async fn does_nothing_while_ceph_is_unreachable() {
        let host = FakeHost::new().fail("ceph -s", "unreachable");
        run(&host).await.unwrap();
        assert!(!host.ran("kubectl-apply"));
    }

    #[tokio::test]
    async fn does_nothing_before_kubernetes_answers() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .fail("kubectl get ns rook-ceph", "connection refused");
        run(&host).await.unwrap();
        assert!(!host.ran("kubectl-apply"));
    }

    #[tokio::test]
    async fn refuses_to_publish_with_no_mon_address() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("kubectl get ns rook-ceph", "")
            .ok("ceph fsid", "fsid\n")
            .ok("ceph mon dump", r#"{"mons":[]}"#);
        run(&host).await.unwrap();
        assert!(
            !host.ran("kubectl-apply"),
            "an empty CSI config must never be published"
        );
    }

    #[tokio::test]
    async fn publishes_the_cephfs_secrets_and_both_configmaps() {
        let host = scripted_ok_host().ok("ceph auth get-key client.admin", "adminkey");

        run(&host).await.unwrap();

        let calls = host.calls();
        for name in [
            "rook-csi-cephfs-provisioner",
            "rook-csi-cephfs-node",
            "rook-ceph-mon",
        ] {
            assert!(
                calls
                    .iter()
                    .any(|c| c.starts_with("kubectl-apply") && c.contains(name)),
                "expected a kubectl-apply for {name}, calls were: {calls:?}"
            );
        }
        assert!(calls.iter().any(|c| c.contains("csi-cluster-config-json")));
        assert!(
            !calls.iter().any(|c| c.contains("csi-rbd")),
            "no RBD credential should ever be minted or published: {calls:?}"
        );
    }

    #[tokio::test]
    async fn ensure_key_reuses_an_existing_key_without_recreating_it() {
        let host = scripted_ok_host().ok("ceph auth get-key client.admin", "adminkey");
        run(&host).await.unwrap();
        assert!(
            !host.ran("auth get-or-create"),
            "every ensure_key call above found its key on the first read, none should be created"
        );
    }

    #[tokio::test]
    async fn creates_a_key_that_is_missing() {
        let host = FakeHost::new()
            .ok("ceph -s", "")
            .ok("kubectl get ns rook-ceph", "")
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
            .ok("ceph auth get-key client.admin", "adminkey")
            .ok("kubectl get secret", "")
            .ok("kubectl-apply", "")
            .ok("kubectl patch configmap", "");

        run(&host).await.unwrap();

        assert!(host.ran("auth get-or-create client.csi-cephfs-provisioner"));
    }

    #[tokio::test]
    async fn a_secret_of_the_wrong_type_is_deleted_before_being_reapplied() {
        let host = scripted_ok_host()
            .ok("ceph auth get-key client.admin", "adminkey")
            .ok(
                "kubectl get secret rook-csi-cephfs-provisioner -n rook-ceph -o jsonpath={.type}",
                "Opaque",
            )
            .ok("kubectl delete secret rook-csi-cephfs-provisioner", "");

        run(&host).await.unwrap();

        assert!(host.ran("kubectl delete secret rook-csi-cephfs-provisioner"));
    }
}
