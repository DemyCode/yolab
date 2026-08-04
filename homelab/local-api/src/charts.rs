//! Chart sources: where an app's chart comes from, and how it gets onto this node.
//!
//! Phase 1 resolved charts from a single directory baked into the NixOS closure, which
//! meant adding or fixing an app required `git reset --hard` plus a full `nixos-rebuild`
//! on every node. This module decouples the two: charts are pulled from Helm repositories
//! into a cache, and the bundled directory becomes the pre-warmed cache for the official
//! repo rather than the only place charts can live.
//!
//! ## Classic repos, not OCI
//!
//! OCI registries have no chart index. Verified against GHCR: both the OCI registry-wide
//! catalog endpoint (`/v2/_catalog`) and the GitHub Packages API return 401 to an
//! unauthenticated caller, so there is no way for a node to ask "what apps exist?"
//! without shipping it a GitHub token. Pulling a known chart by name works fine — but
//! enumeration is the storefront, so the catalog needs an index either way.
//!
//! That index is a plain `index.yaml` committed to the yolab repo and served over
//! raw.githubusercontent.com, which is the whole hosting story: no GitHub Pages, no
//! gh-pages branch, no server. It is a standard Helm repository, so third parties can
//! publish one anywhere that serves static files and `helm repo add` just works.
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

/// The official repo's URL — the `charts/` directory of the yolab repo, served as static
/// files. `helm repo add` appends `index.yaml`, so this is a normal Helm repository that
/// happens to be hosted by the same place the source lives.
///
/// Configurable because this URL is baked into every deployed node: moving the catalog
/// later (to a CDN, or to a route on yolab-external for a stable own-domain address)
/// must not require patching code, and the old URL has to keep working until the fleet
/// has rolled over.
fn official_url() -> String {
    std::env::var("YOLAB_OFFICIAL_CHART_REPO")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com/DemyCode/yolab/main/charts/".into())
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

/// Refreshes a repo's index and pulls every chart it advertises into the cache.
///
/// Pulling eagerly rather than on demand keeps install latency predictable and means an
/// install does not fail because a registry is briefly unreachable at exactly the wrong
/// moment. The catalog is small enough (tens of charts, a few KB each) that this is cheap.
pub async fn sync_repo(repo: &ChartRepo) -> anyhow::Result<usize> {
    let dir = cache_dir_for(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;

    // `helm repo add --force-update` is idempotent and updates the URL if it changed.
    let out = Command::new("helm")
        .args(["repo", "add", &repo.name, &repo.url, "--force-update"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("helm repo add {}: {}", repo.name, String::from_utf8_lossy(&out.stderr).trim());
    }
    let out = Command::new("helm").args(["repo", "update", &repo.name]).output().await?;
    if !out.status.success() {
        anyhow::bail!("helm repo update {}: {}", repo.name, String::from_utf8_lossy(&out.stderr).trim());
    }

    let listed = Command::new("helm")
        .args(["search", "repo", &format!("{}/", repo.name), "-o", "json"])
        .output()
        .await?;
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&listed.stdout).unwrap_or_default();

    let mut pulled = 0usize;
    for e in &entries {
        let Some(full) = e["name"].as_str() else { continue };
        // "official/gitea" -> "gitea"
        let short = full.split_once('/').map(|(_, c)| c).unwrap_or(full);
        // The library chart is a dependency, not an app; it is published so downstream
        // charts can resolve it, but it must never appear in the storefront.
        if short == "yolab-common" {
            continue;
        }
        let target = dir.join(short);
        let _ = tokio::fs::remove_dir_all(&target).await;
        let out = Command::new("helm")
            .args(["pull", full, "--untar", "--untardir", &dir.to_string_lossy()])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => pulled += 1,
            Ok(o) => tracing::warn!(
                "chart pull {full}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!("chart pull {full}: {e}"),
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
        assert!(valid_repo_url("https://charts.example.com/"));
        // A chart is arbitrary cluster objects; plaintext transport means anything on the
        // path can substitute one.
        assert!(!valid_repo_url("http://charts.example.com/"));
        assert!(!valid_repo_url("oci://ghcr.io/x/y"));
        assert!(!valid_repo_url("file:///etc"));
        assert!(!valid_repo_url(""));
    }
}
