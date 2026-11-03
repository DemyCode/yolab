use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use kube::Client;

use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};

pub const MANIFESTS_ENV: &str = "YOLAB_CEPH_CSI_MANIFESTS";

pub async fn apply_manifests(client: &Client, path: &Path) -> Result<Tick> {
    let manifests = std::fs::read_to_string(path)
        .with_context(|| format!("read the CSI manifests at {}", path.display()))?;
    crate::k8s::apply_documents(client, &manifests).await?;
    Ok(Tick::Done)
}

pub struct CephCsiController;

impl Controller for CephCsiController {
    fn name(&self) -> &'static str {
        "ceph-csi"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let path = std::env::var(MANIFESTS_ENV).with_context(|| {
            format!("{MANIFESTS_ENV} is not set; the NixOS module names the CSI manifests")
        })?;
        let b = crate::routers::backup_common::Backend::real().await?;
        apply_manifests(&b.kube, Path::new(&path)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{accept_patches, api_server, patched};

    #[tokio::test]
    async fn every_manifest_in_the_file_is_applied() {
        let (server, client) = api_server().await;
        accept_patches(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cephfs.yaml");
        std::fs::write(
            &file,
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: ceph-csi\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ceph-csi-config\n  namespace: ceph-csi\n",
        )
        .unwrap();

        assert_eq!(apply_manifests(&client, &file).await.unwrap(), Tick::Done);

        let names: Vec<String> = patched(&server)
            .await
            .iter()
            .filter_map(|p| p["metadata"]["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(names, ["ceph-csi", "ceph-csi-config"]);
    }

    #[tokio::test]
    async fn a_missing_manifest_file_is_an_error() {
        let (_server, client) = api_server().await;
        assert!(apply_manifests(&client, Path::new("/nonexistent/cephfs.yaml"))
            .await
            .is_err());
    }
}
