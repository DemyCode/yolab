
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::process::Command;

const REPO_CM: &str = "yolab-chart-repos";
const REPO_NS: &str = "kube-system";

pub const CACHE_DIR: &str = "/var/lib/yolab/charts";

pub const OFFICIAL: &str = "official";

pub const CUSTOM: &str = "custom";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChartRepo {
    pub name: String,
    pub url: String,
    #[serde(default = "yes")]
    pub removable: bool,
}

fn yes() -> bool {
    true
}

fn official_url() -> String {
    std::env::var("YOLAB_OFFICIAL_CHART_REPO").unwrap_or_else(|_| {
        "https://raw.githubusercontent.com/DemyCode/yolab/main/catalog.yaml".into()
    })
}

pub async fn list_repos() -> Vec<ChartRepo> {
    let mut repos = vec![ChartRepo {
        name: OFFICIAL.into(),
        url: official_url(),
        removable: false,
    }];
    let stored: std::collections::HashMap<String, String> = crate::kubectl::get_json(&[
        "get",
        "configmap",
        REPO_CM,
        "-n",
        REPO_NS,
        "-o",
        "jsonpath={.data}",
    ])
    .await
    .ok()
    .and_then(|v| serde_json::from_value(v).ok())
    .unwrap_or_default();
    for (name, url) in stored {
        if name == OFFICIAL {
            continue;
        }
        repos.push(ChartRepo {
            name,
            url,
            removable: true,
        });
    }
    repos
}

pub fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name != OFFICIAL
        && name != CUSTOM
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn valid_repo_url(url: &str) -> bool {
    url.starts_with("https://")
}

pub async fn add_repo(name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_repo_name(name) {
        anyhow::bail!(
            "repo name must be lowercase letters, digits and hyphens, and not '{OFFICIAL}'"
        );
    }
    if !valid_repo_url(url) {
        anyhow::bail!("repo URL must start with https://");
    }
    let patch = serde_json::json!({ "data": { name: url } }).to_string();
    if crate::kubectl::run(&[
        "patch",
        "configmap",
        REPO_CM,
        "-n",
        REPO_NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await
    .is_err()
    {
        let _ = crate::kubectl::run(&["create", "configmap", REPO_CM, "-n", REPO_NS]).await;
        crate::kubectl::run(&[
            "patch",
            "configmap",
            REPO_CM,
            "-n",
            REPO_NS,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await?;
    }
    Ok(())
}

pub async fn remove_repo(name: &str) -> anyhow::Result<()> {
    if name == OFFICIAL {
        anyhow::bail!("the official catalog cannot be removed");
    }
    let patch = serde_json::json!({ "data": { name: null } }).to_string();
    crate::kubectl::run(&[
        "patch",
        "configmap",
        REPO_CM,
        "-n",
        REPO_NS,
        "--type",
        "merge",
        "-p",
        &patch,
    ])
    .await?;
    let dir = cache_dir_for(name);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

fn cache_dir_for(repo: &str) -> PathBuf {
    PathBuf::from(CACHE_DIR).join(repo)
}

pub fn official_dir() -> PathBuf {
    cache_dir_for(OFFICIAL)
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct CatalogManifest {
    pub registry: String,
    #[serde(default)]
    pub library: Option<CatalogEntry>,
    #[serde(default)]
    pub charts: Vec<CatalogEntry>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct CatalogEntry {
    pub name: String,
    pub version: String,
}

fn valid_registry(registry: &str) -> bool {
    registry.starts_with("oci://")
}

fn valid_chart_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

async fn pull_into(dir: &Path, registry: &str, entry: &CatalogEntry) -> anyhow::Result<()> {
    let reference = format!("{}/{}", registry.trim_end_matches('/'), entry.name);
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
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "pull {reference}:{}: {}",
            entry.version,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn fetch_manifest(repo: &ChartRepo) -> anyhow::Result<CatalogManifest> {
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
    Ok(manifest)
}

pub async fn sync_chart(repo: &ChartRepo, name: &str) -> anyhow::Result<()> {
    if !valid_chart_name(name) {
        anyhow::bail!("unusable chart name {name:?}");
    }

    let manifest = fetch_manifest(repo).await?;
    let entry = manifest
        .charts
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("{name} is not in {}", repo.name))?;

    let dir = cache_dir_for(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;
    pull_into(&dir, &manifest.registry, entry).await
}

pub async fn sync_repo(repo: &ChartRepo) -> anyhow::Result<usize> {
    let manifest = fetch_manifest(repo).await?;

    let dir = cache_dir_for(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;

    if let Some(library) = &manifest.library {
        if valid_chart_name(&library.name) {
            if let Err(e) = pull_into(&dir, &manifest.registry, library).await {
                tracing::warn!("{}: library pull: {e}", repo.name);
            }
        } else {
            tracing::warn!(
                "{}: skipping library with unusable name {:?}",
                repo.name,
                library.name
            );
        }
    }

    let mut pulled = 0usize;
    for entry in &manifest.charts {
        if !valid_chart_name(&entry.name) {
            tracing::warn!(
                "{}: skipping chart with unusable name {:?}",
                repo.name,
                entry.name
            );
            continue;
        }
        match pull_into(&dir, &manifest.registry, entry).await {
            Ok(()) => pulled += 1,
            Err(e) => tracing::warn!("{e}"),
        }
    }
    Ok(pulled)
}

pub async fn chart_sources() -> Vec<(String, PathBuf)> {
    let mut sources = Vec::new();
    let custom = cache_dir_for(CUSTOM);
    if custom.is_dir() {
        sources.push((CUSTOM.to_string(), custom));
    }
    for repo in list_repos().await {
        let dir = cache_dir_for(&repo.name);
        if dir.is_dir() {
            sources.push((repo.name.clone(), dir));
        }
    }
    sources
}

pub async fn resolve_chart(id: &str, repo: Option<&str>) -> Option<(String, PathBuf)> {
    for (name, dir) in chart_sources().await {
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

pub struct ChartSyncController;

impl crate::runtime::Controller for ChartSyncController {
    fn name(&self) -> &'static str {
        "chart-sync"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let mut failed = Vec::new();
        for repo in list_repos().await {
            match sync_repo(&repo).await {
                Ok(n) if n > 0 => tracing::info!("chart sync: {} — {n} chart(s)", repo.name),
                Ok(_) => {}
                Err(e) => failed.push(format!("{}: {e}", repo.name)),
            }
        }
        if failed.is_empty() {
            Ok(crate::runtime::Tick::Done)
        } else {
            anyhow::bail!("{}", failed.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_constrained() {
        assert!(valid_repo_name("community"));
        assert!(valid_repo_name("my-charts-2"));
        assert!(!valid_repo_name(OFFICIAL));
        assert!(!valid_repo_name("../etc"));
        assert!(!valid_repo_name("a/b"));
        assert!(!valid_repo_name("UPPER"));
        assert!(!valid_repo_name(""));
    }

    #[test]
    fn repo_urls_must_be_https() {
        assert!(valid_repo_url("https://charts.example.com/catalog.yaml"));
        assert!(!valid_repo_url("http://charts.example.com/catalog.yaml"));
        assert!(!valid_repo_url("file:///etc"));
        assert!(!valid_repo_url(""));
    }

    #[test]
    fn registries_must_be_oci() {
        assert!(valid_registry("oci://ghcr.io/demycode/charts"));
        assert!(!valid_registry("https://ghcr.io/demycode/charts"));
        assert!(!valid_registry(""));
    }

    #[test]
    fn chart_names_cannot_escape_the_cache() {
        assert!(valid_chart_name("filebrowser"));
        assert!(valid_chart_name("reactive-resume"));
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
        assert_eq!(
            m.charts[0],
            CatalogEntry {
                name: "filebrowser".into(),
                version: "0.1.0".into()
            }
        );
        assert_eq!(m.charts[1].version, "0.2.1");
    }

    #[test]
    fn manifest_without_charts_is_valid_and_empty() {
        let m: CatalogManifest =
            serde_norway::from_str("registry: oci://ghcr.io/x/charts\n").unwrap();
        assert!(m.charts.is_empty());
        assert!(m.library.is_none());
    }

    #[test]
    fn manifest_reads_the_library_when_it_declares_one() {
        let m: CatalogManifest = serde_norway::from_str(
            "registry: oci://ghcr.io/demycode/charts\n\
             library:\n\
             \x20 name: yolab-common\n\
             \x20 version: \"0.1.1\"\n\
             charts:\n\
             \x20 - name: gitea\n\
             \x20   version: \"0.1.1\"\n",
        )
        .unwrap();
        assert_eq!(
            m.library,
            Some(CatalogEntry {
                name: "yolab-common".into(),
                version: "0.1.1".into()
            })
        );
        assert_eq!(m.charts.len(), 1);
    }
}
