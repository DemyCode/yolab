use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use kube::Client;

use crate::runtime::{Ctx, Resource, Scope, Tick};

pub const MANIFESTS_ENV: &str = "YOLAB_CEPH_CSI_MANIFESTS";

pub async fn apply_manifests(client: &Client, path: &Path) -> Result<Tick> {
    let manifests = std::fs::read_to_string(path)
        .with_context(|| format!("read the CSI manifests at {}", path.display()))?;
    crate::k8s::apply_documents(client, &manifests).await?;
    Ok(Tick::Done)
}

pub struct CephCsiController;

impl Resource for CephCsiController {
    fn name(&self) -> &'static str {
        "ceph-csi"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    async fn converge(&self, ctx: &Ctx) -> Result<Tick> {
        converge_with(ctx, std::env::var(MANIFESTS_ENV)).await
    }
}

async fn converge_with(ctx: &Ctx, var: Result<String, std::env::VarError>) -> Result<Tick> {
    let path = var.with_context(|| {
        format!("{MANIFESTS_ENV} is not set; the NixOS module names the CSI manifests")
    })?;
    let b = ctx.tools.backend().await?;
    apply_manifests(&b.kube, Path::new(&path)).await
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

    fn ctx_with(kube: crate::k8s::Kube) -> Ctx {
        Ctx {
            node: "node1".into(),
            tools: crate::tools::Tools {
                host: Default::default(),
                kube,
                http: crate::http::client(),
            },
        }
    }

    fn manifest_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let file = dir.path().join("cephfs.yaml");
        std::fs::write(
            &file,
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: ceph-csi\n",
        )
        .unwrap();
        file
    }

    #[test]
    fn the_csi_controller_runs_once_per_cluster_every_minute_after_the_kube_api() {
        let c = CephCsiController;
        assert_eq!(c.name(), "ceph-csi");
        assert_eq!(c.scope(), Scope::Cluster);
        assert_eq!(c.interval(), Duration::from_secs(60));
        assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
    }

    #[tokio::test]
    async fn the_csi_controller_refuses_to_run_without_the_manifests_variable() {
        let err = CephCsiController
            .converge(&ctx_with(crate::k8s::Kube::from_environment()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is not set"), "{err}");
    }

    #[tokio::test]
    async fn the_csi_controller_fails_when_kubernetes_cannot_be_reached() {
        let err = converge_with(
            &ctx_with(crate::k8s::Kube::from_environment()),
            Ok("/nonexistent/cephfs.yaml".into()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "this test runs without a cluster");
    }

    #[tokio::test]
    async fn the_csi_controller_applies_the_named_manifests() {
        let (server, client) = api_server().await;
        accept_patches(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let file = manifest_file(&dir);
        let tick = converge_with(
            &ctx_with(crate::k8s::Kube::with(client)),
            Ok(file.display().to_string()),
        )
        .await
        .unwrap();
        assert_eq!(tick, Tick::Done);
        assert_eq!(patched(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn a_manifest_the_api_server_refuses_is_an_error() {
        let (_server, client) = api_server().await;
        let dir = tempfile::tempdir().unwrap();
        let file = manifest_file(&dir);
        assert!(apply_manifests(&client, &file).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_manifest_file_is_an_error() {
        let (_server, client) = api_server().await;
        assert!(
            apply_manifests(&client, Path::new("/nonexistent/cephfs.yaml"))
                .await
                .is_err()
        );
    }
}
