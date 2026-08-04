//! Chart sources: where an app's chart comes from, and how it gets onto this node.
//!
//! Phase 1 resolved charts from a single directory baked into the NixOS closure, which
//! meant adding or fixing an app required `git reset --hard` plus a full `nixos-rebuild`
//! on every node. This module decouples the two: charts are pulled from Helm repositories
//! into a cache, and the bundled directory becomes the pre-warmed cache for the official
//! repo rather than the only place charts can live.
//!
//! ## Distribution and discovery are separate problems
//!
//! Conflating them is what sent this module through GitHub Pages and then through
//! committing chart tarballs to git before arriving somewhere sensible. They want
//! different mechanisms:
//!
//! **Distribution is solved by the registry.** `helm pull oci://ghcr.io/...` fetches a
//! chart with no hosting on our side, and public packages pull anonymously — verified,
//! no login and no registry config needed. Nothing to serve, nothing in git.
//!
//! **Discovery is not.** A registry cannot be enumerated without credentials: GHCR
//! returns 401 to an unauthenticated caller on both the OCI catalog endpoint
//! (`/v2/_catalog`) and the GitHub Packages API. Asking "what apps exist?" would mean
//! shipping every node a GitHub token, which is not acceptable for a storefront.
//!
//! So a repo here is a **catalog manifest** — a few hundred bytes of YAML naming the
//! registry and the charts in it — and the chart bytes come from that registry. The
//! manifest is the only thing anyone has to host, and for the official catalog it is a
//! file in this repo served over raw.githubusercontent.com.
//!
//! Chart metadata (display name, icon, schema) deliberately is NOT duplicated into the
//! manifest: it lives in each chart's Chart.yaml, which is the single source of truth,
//! and the storefront reads it from the pulled chart in the cache.
//!
//! ## Trust
//!
//! Adding a repository is not like adding a package source to a Linux distro. A chart can
//! declare arbitrary cluster objects — a privileged DaemonSet, a ClusterRoleBinding, a
//! hostPath mount of `/` — so `add_repo` hands the publisher the ability to do anything to
//! the user's cluster. That is why every chart records the repo it came from (see
//! `ANN_CHART_REPO`) and why third-party repos must stay behind explicit consent in the UI
//! rather than being presented as equivalent to the curated catalog.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Repo config lives in a ConfigMap rather than on disk so every node in the cluster
/// agrees on the same set, and so it is captured by the cluster-state backup.
const REPO_CM: &str = "yolab-chart-repos";
const REPO_NS: &str = "kube-system";

/// Where pulled charts are unpacked. Not in the Nix store: the whole point is that this
/// changes without rebuilding the system.
const CACHE_DIR: &str = "/var/lib/yolab/charts";

/// The curated catalog. Always present, cannot be removed, and is the only repo whose
/// charts the UI presents as vouched-for.
pub const OFFICIAL: &str = "official";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChartRepo {
    pub name: String,
    pub url: String,
    /// False for the official repo — the UI must not offer to remove it, and a node with
    /// no repos at all would have an empty storefront.
    #[serde(default = "yes")]
    pub removable: bool,
}

fn yes() -> bool {
    true
}

/// The official catalog manifest — one small generated file in the yolab repo. The charts
/// it names are pulled from the OCI registry the manifest itself declares.
///
/// Configurable because this URL is baked into every deployed node: moving the manifest
/// later (to a CDN, or to a route on yolab-external for a stable own-domain address)
/// must not require patching code, and the old URL has to keep working until the fleet
/// has rolled over.
fn official_url() -> String {
    std::env::var("YOLAB_OFFICIAL_CHART_REPO")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com/DemyCode/yolab/main/catalog.yaml".into())
}

/// Every configured repo, official first.
///
/// The official entry is synthesised rather than stored, so it cannot be edited away by a
/// malformed ConfigMap and so its URL follows the deployment rather than whatever was
/// written at first boot.
pub async fn list_repos() -> Vec<ChartRepo> {
    let mut repos = vec![ChartRepo {
        name: OFFICIAL.into(),
        url: official_url(),
        removable: false,
    }];
    let stored: std::collections::HashMap<String, String> = crate::kubectl::get_json(&[
        "get", "configmap", REPO_CM, "-n", REPO_NS, "-o", "jsonpath={.data}",
    ])
    .await
    .ok()
    .and_then(|v| serde_json::from_value(v).ok())
    .unwrap_or_default();
    for (name, url) in stored {
        if name == OFFICIAL {
            continue; // never shadowed by stored config
        }
        repos.push(ChartRepo { name, url, removable: true });
    }
    repos
}

/// Repo names must be usable as a `helm repo` name and as a path segment in the cache.
pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name != OFFICIAL
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Only https. A chart pulled over plaintext http can be swapped in flight by anything on
/// the path, and a chart is arbitrary cluster objects — this is not a transport we can be
/// relaxed about.
pub fn valid_repo_url(url: &str) -> bool {
    url.starts_with("https://")
}

pub async fn add_repo(name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_repo_name(name) {
        anyhow::bail!("repo name must be lowercase letters, digits and hyphens, and not '{OFFICIAL}'");
    }
    if !valid_repo_url(url) {
        anyhow::bail!("repo URL must start with https://");
    }
    let patch = serde_json::json!({ "data": { name: url } }).to_string();
    if crate::kubectl::run(&["patch", "configmap", REPO_CM, "-n", REPO_NS, "--type", "merge", "-p", &patch])
        .await
        .is_err()
    {
        let _ = crate::kubectl::run(&["create", "configmap", REPO_CM, "-n", REPO_NS]).await;
        crate::kubectl::run(&["patch", "configmap", REPO_CM, "-n", REPO_NS, "--type", "merge", "-p", &patch]).await?;
    }
    Ok(())
}

pub async fn remove_repo(name: &str) -> anyhow::Result<()> {
    if name == OFFICIAL {
        anyhow::bail!("the official catalog cannot be removed");
    }
    // JSON-patch remove on a key that isn't there fails, so go through merge-null.
    let patch = serde_json::json!({ "data": { name: null } }).to_string();
    crate::kubectl::run(&["patch", "configmap", REPO_CM, "-n", REPO_NS, "--type", "merge", "-p", &patch]).await?;
    let dir = cache_dir_for(name);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

fn cache_dir_for(repo: &str) -> PathBuf {
    PathBuf::from(CACHE_DIR).join(repo)
}

/// What a repo's catalog manifest declares: where the charts are, and which exist.
#[derive(Deserialize, Debug, PartialEq)]
pub struct CatalogManifest {
    /// OCI reference the charts live under, e.g. `oci://ghcr.io/demycode/charts`.
    pub registry: String,
    #[serde(default)]
    pub charts: Vec<CatalogEntry>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct CatalogEntry {
    pub name: String,
    pub version: String,
}

/// Only oci:// registries, and only https for the manifest itself. A chart is arbitrary
/// cluster objects, so neither the list of what to install nor the bytes themselves may
/// arrive over a transport anything on the path can rewrite.
fn valid_registry(registry: &str) -> bool {
    registry.starts_with("oci://")
}

/// Rejects anything that could escape the cache directory or confuse a chart reference.
fn valid_chart_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Fetches a repo's catalog manifest and pulls every chart it names into the cache.
///
/// Pulling eagerly rather than on demand keeps install latency predictable, and means an
/// install does not fail because the registry is briefly unreachable at exactly the wrong
/// moment. The catalog is small — tens of charts at a few KB each — so this is cheap.
pub async fn sync_repo(repo: &ChartRepo) -> anyhow::Result<usize> {
    let body = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(&repo.url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let manifest: CatalogManifest = serde_norway::from_str(&body)
        .map_err(|e| anyhow::anyhow!("{}: catalog manifest is not valid: {e}", repo.name))?;

    if !valid_registry(&manifest.registry) {
        anyhow::bail!("{}: registry must be an oci:// reference", repo.name);
    }

    let dir = cache_dir_for(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;
    let registry = manifest.registry.trim_end_matches('/').to_string();

    let mut pulled = 0usize;
    for entry in &manifest.charts {
        // The library chart is a dependency, not an app. It is published so downstream
        // charts can resolve it, but it must never reach the storefront.
        if entry.name == "yolab-common" {
            continue;
        }
        if !valid_chart_name(&entry.name) {
            tracing::warn!("{}: skipping chart with unusable name {:?}", repo.name, entry.name);
            continue;
        }
        let reference = format!("{registry}/{}", entry.name);
        // Untar over a clean directory so a chart that lost files between versions does
        // not keep stale templates from the previous pull.
        let _ = tokio::fs::remove_dir_all(dir.join(&entry.name)).await;
        let out = Command::new("helm")
            .args([
                "pull",
                &reference,
                "--version",
                &entry.version,
                "--untar",
                "--untardir",
                &dir.to_string_lossy(),
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => pulled += 1,
            Ok(o) => tracing::warn!(
                "chart pull {reference}:{}: {}",
                entry.version,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!("chart pull {reference}: {e}"),
        }
    }
    Ok(pulled)
}

/// Every directory that may contain charts, in resolution order.
///
/// The cache wins over the bundled copy so a published fix reaches users without waiting
/// for an OS rebuild — which is the reason this module exists. The bundled directory
/// remains as the seed: a node that has never synced still has the full official catalog.
pub async fn chart_sources(catalog_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut sources = Vec::new();
    for repo in list_repos().await {
        let dir = cache_dir_for(&repo.name);
        if dir.is_dir() {
            sources.push((repo.name.clone(), dir));
        }
    }
    sources.push((OFFICIAL.to_string(), catalog_dir.to_path_buf()));
    sources
}

/// Locates a chart by id, preferring `repo` when given. Returns (repo name, chart dir).
pub async fn resolve_chart(
    catalog_dir: &Path,
    id: &str,
    repo: Option<&str>,
) -> Option<(String, PathBuf)> {
    for (name, dir) in chart_sources(catalog_dir).await {
        if let Some(want) = repo {
            if want != name {
                continue;
            }
        }
        let candidate = dir.join(id);
        if candidate.join("Chart.yaml").is_file() {
            return Some((name, candidate));
        }
    }
    None
}

/// Background refresh so a node discovers newly published apps on its own.
///
/// This is the payoff for the whole module: the catalog gains apps and fixes without a
/// `nixos-rebuild` on every machine. A failed sync is logged and retried next tick rather
/// than escalated — a node whose network is briefly unhappy should keep serving the
/// charts it already has, not lose its storefront.
pub async fn run_chart_sync() {
    // Let k3s settle before the first sync; nothing here works without the API server.
    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    loop {
        for repo in list_repos().await {
            match sync_repo(&repo).await {
                Ok(n) if n > 0 => tracing::info!("chart sync: {} — {n} chart(s)", repo.name),
                Ok(_) => {}
                Err(e) => tracing::warn!("chart sync: {}: {e}", repo.name),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_constrained() {
        assert!(valid_repo_name("community"));
        assert!(valid_repo_name("my-charts-2"));
        // Must not be able to shadow the curated catalog.
        assert!(!valid_repo_name(OFFICIAL));
        // Path traversal into another repo's cache, or out of it entirely.
        assert!(!valid_repo_name("../etc"));
        assert!(!valid_repo_name("a/b"));
        assert!(!valid_repo_name("UPPER"));
        assert!(!valid_repo_name(""));
    }

    #[test]
    fn repo_urls_must_be_https() {
        assert!(valid_repo_url("https://charts.example.com/catalog.yaml"));
        // A chart is arbitrary cluster objects; plaintext transport means anything on the
        // path can rewrite the list of what gets installed.
        assert!(!valid_repo_url("http://charts.example.com/catalog.yaml"));
        assert!(!valid_repo_url("file:///etc"));
        assert!(!valid_repo_url(""));
    }

    #[test]
    fn registries_must_be_oci() {
        assert!(valid_registry("oci://ghcr.io/demycode/charts"));
        // A manifest that redirected chart bytes to plain HTTP would undo the point of
        // requiring https for the manifest itself.
        assert!(!valid_registry("https://ghcr.io/demycode/charts"));
        assert!(!valid_registry(""));
    }

    #[test]
    fn chart_names_cannot_escape_the_cache() {
        assert!(valid_chart_name("filebrowser"));
        assert!(valid_chart_name("reactive-resume"));
        // A hostile manifest must not be able to write outside its own cache directory
        // or forge a reference to another registry path.
        assert!(!valid_chart_name("../../etc/passwd"));
        assert!(!valid_chart_name("a/b"));
        assert!(!valid_chart_name("Upper"));
        assert!(!valid_chart_name(""));
    }

    #[test]
    fn manifest_parses_the_published_shape() {
        let m: CatalogManifest = serde_norway::from_str(
            "apiVersion: yolab.io/v1\n\
             registry: oci://ghcr.io/demycode/charts\n\
             charts:\n\
             \x20 - name: filebrowser\n\
             \x20   version: \"0.1.0\"\n\
             \x20 - name: gitea\n\
             \x20   version: \"0.2.1\"\n",
        )
        .unwrap();
        assert_eq!(m.registry, "oci://ghcr.io/demycode/charts");
        assert_eq!(m.charts.len(), 2);
        assert_eq!(m.charts[0], CatalogEntry { name: "filebrowser".into(), version: "0.1.0".into() });
        assert_eq!(m.charts[1].version, "0.2.1");
    }

    #[test]
    fn manifest_without_charts_is_valid_and_empty() {
        // A newly created repo that has published nothing yet must sync cleanly to zero
        // rather than erroring and leaving the repo looking broken.
        let m: CatalogManifest =
            serde_norway::from_str("registry: oci://ghcr.io/x/charts\n").unwrap();
        assert!(m.charts.is_empty());
    }
}
