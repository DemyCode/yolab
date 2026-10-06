use kube::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::host::Host;

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

fn repos_ref() -> serde_json::Value {
    crate::k8s::reference("v1", "ConfigMap", REPO_NS, REPO_CM)
}

pub async fn list_repos(client: &Client) -> Vec<ChartRepo> {
    let mut repos = vec![ChartRepo {
        name: OFFICIAL.into(),
        url: official_url(),
        removable: false,
    }];
    let stored: std::collections::BTreeMap<String, String> =
        match crate::k8s::get(client, &repos_ref()).await {
            Ok(found) => found
                .and_then(|cm| serde_json::from_value(cm["data"].clone()).ok())
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!("the added chart repositories are unreadable right now: {e:#}");
                Default::default()
            }
        };
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

pub async fn add_repo(client: &Client, name: &str, url: &str) -> anyhow::Result<()> {
    if !valid_repo_name(name) {
        anyhow::bail!(
            "repo name must be lowercase letters, digits and hyphens, and not '{OFFICIAL}'"
        );
    }
    if !valid_repo_url(url) {
        anyhow::bail!("repo URL must start with https://");
    }
    let mut added = repos_ref();
    added["data"] = serde_json::json!({ name: url });
    match crate::k8s::merge_patch(client, &added).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => crate::k8s::apply(client, &added).await,
        other => other,
    }
}

pub async fn remove_repo(client: &Client, name: &str) -> anyhow::Result<()> {
    if name == OFFICIAL {
        anyhow::bail!("the official catalog cannot be removed");
    }
    let mut removed = repos_ref();
    removed["data"] = serde_json::json!({ name: null });
    match crate::k8s::merge_patch(client, &removed).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => {}
        other => other?,
    }
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

const HELM_PULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

async fn pull_into<H: Host>(
    host: &H,
    dir: &Path,
    registry: &str,
    entry: &CatalogEntry,
) -> anyhow::Result<()> {
    let reference = format!("{}/{}", registry.trim_end_matches('/'), entry.name);
    let _ = tokio::fs::remove_dir_all(dir.join(&entry.name)).await;
    let untar_dir = dir.to_string_lossy();
    let out = host
        .run_cmd_bounded(
            "helm",
            &[
                "pull",
                &reference,
                "--version",
                &entry.version,
                "--untar",
                "--untardir",
                &untar_dir,
            ],
            HELM_PULL_TIMEOUT,
        )
        .await?;
    if !out.success {
        anyhow::bail!("pull {reference}:{}: {}", entry.version, out.stderr.trim());
    }
    Ok(())
}

fn http() -> crate::http::Client {
    crate::http::client()
}

async fn fetch_manifest(repo: &ChartRepo) -> anyhow::Result<CatalogManifest> {
    let body = http()
        .get(&repo.url)
        .timeout(std::time::Duration::from_secs(30))
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

pub async fn sync_chart<H: Host>(
    host: &H,
    cache_root: &Path,
    repo: &ChartRepo,
    name: &str,
) -> anyhow::Result<()> {
    if !valid_chart_name(name) {
        anyhow::bail!("unusable chart name {name:?}");
    }

    let manifest = fetch_manifest(repo).await?;
    let entry = manifest
        .charts
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| anyhow::anyhow!("{name} is not in {}", repo.name))?;

    let dir = cache_root.join(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;
    pull_into(host, &dir, &manifest.registry, entry).await
}

pub async fn fetch_newest<H: Host>(
    host: &H,
    cache_root: &Path,
    repos: &[ChartRepo],
    name: &str,
    from: Option<&str>,
) -> anyhow::Result<String> {
    let mut last = None;
    for repo in repos.iter().filter(|r| from.is_none_or(|f| f == r.name)) {
        match sync_chart(host, cache_root, repo, name).await {
            Ok(()) => return Ok(repo.name.clone()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no catalog to fetch {name} from")))
}

pub async fn fetch_exact<H: Host>(
    host: &H,
    dest: &Path,
    repos: &[ChartRepo],
    name: &str,
    version: &str,
    from: &str,
) -> anyhow::Result<PathBuf> {
    if !valid_chart_name(name) {
        anyhow::bail!("unusable chart name {name:?}");
    }
    let Some(repo) = repos.iter().find(|r| r.name == from) else {
        anyhow::bail!("{name} came from {from}, which is no longer a chart repository here");
    };
    let manifest = fetch_manifest(repo).await?;
    let entry = CatalogEntry {
        name: name.to_string(),
        version: version.to_string(),
    };
    pull_into(host, dest, &manifest.registry, &entry).await?;
    Ok(dest.join(name))
}

#[derive(Deserialize)]
struct ChartVersion {
    version: String,
}

#[derive(Deserialize, Default)]
struct ChartAnnotations {
    #[serde(default)]
    annotations: std::collections::HashMap<String, String>,
}

pub fn github_repos(dirs: &[PathBuf]) -> Vec<String> {
    let mut repos: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("Chart.yaml")).ok())
        .filter_map(|text| serde_norway::from_str::<ChartAnnotations>(&text).ok())
        .filter_map(|chart| chart.annotations.get("yolab.io/github").cloned())
        .filter(|repo| crate::github::is_repo(repo))
        .collect();
    repos.sort();
    repos.dedup();
    repos
}

pub fn cached_at_version(
    cache_root: &Path,
    repo: &str,
    name: &str,
    version: &str,
) -> Option<PathBuf> {
    let dir = cache_root.join(repo).join(name);
    let text = std::fs::read_to_string(dir.join("Chart.yaml")).ok()?;
    let found: ChartVersion = serde_norway::from_str(&text).ok()?;
    (found.version == version).then_some(dir)
}

pub async fn sync_repo<H: Host>(
    host: &H,
    cache_root: &Path,
    repo: &ChartRepo,
) -> anyhow::Result<usize> {
    let manifest = fetch_manifest(repo).await?;

    let dir = cache_root.join(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;

    if let Some(library) = &manifest.library {
        if valid_chart_name(&library.name) {
            if let Err(e) = pull_into(host, &dir, &manifest.registry, library).await {
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
        match pull_into(host, &dir, &manifest.registry, entry).await {
            Ok(()) => pulled += 1,
            Err(e) => tracing::warn!("{e}"),
        }
    }
    Ok(pulled)
}

pub async fn chart_sources(client: &Client) -> Vec<(String, PathBuf)> {
    let mut sources = Vec::new();
    let custom = cache_dir_for(CUSTOM);
    if custom.is_dir() {
        sources.push((CUSTOM.to_string(), custom));
    }
    for repo in list_repos(client).await {
        let dir = cache_dir_for(&repo.name);
        if dir.is_dir() {
            sources.push((repo.name.clone(), dir));
        }
    }
    sources
}

pub async fn resolve_chart(
    client: &Client,
    id: &str,
    repo: Option<&str>,
) -> Option<(String, PathBuf)> {
    for (name, dir) in chart_sources(client).await {
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
        let b = crate::routers::backup_common::Backend::real().await?;
        sync_all(&b.kube, &b.host).await
    }
}

async fn refresh_github_stars(client: &Client) {
    let dirs: Vec<PathBuf> = chart_sources(client)
        .await
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    let repos = github_repos(&dirs);
    let now = i64::try_from(crate::system::now_secs()).unwrap_or(i64::MAX);
    match crate::github::refresh(client, &http(), crate::github::API, &repos, now).await {
        Ok(n) if n > 0 => tracing::info!("chart sync: GitHub stars refreshed for {n} project(s)"),
        Ok(_) => {}
        Err(e) => tracing::warn!("chart sync: GitHub stars not refreshed: {e:#}"),
    }
}

async fn sync_all<H: Host>(client: &Client, host: &H) -> anyhow::Result<crate::runtime::Tick> {
    let mut failed = Vec::new();
    for repo in list_repos(client).await {
        match sync_repo(host, Path::new(CACHE_DIR), &repo).await {
            Ok(n) if n > 0 => tracing::info!("chart sync: {} — {n} chart(s)", repo.name),
            Ok(_) => {}
            Err(e) => failed.push(format!("{}: {e}", repo.name)),
        }
    }
    refresh_github_stars(client).await;
    if failed.is_empty() {
        Ok(crate::runtime::Tick::Done)
    } else {
        anyhow::bail!("{}", failed.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart_in(root: &Path, name: &str, annotations: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Chart.yaml"),
            format!("name: {name}\nversion: 1.0.0\nannotations:\n{annotations}"),
        )
        .unwrap();
    }

    #[test]
    fn every_repository_and_zip_chart_s_github_project_is_collected_once() {
        let official = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        chart_in(official.path(), "immich", "  yolab.io/github: immich-app/immich\n");
        chart_in(official.path(), "media-stack", "  yolab.io/tagline: no project\n");
        chart_in(official.path(), "broken", "  yolab.io/github: not a path\n");
        chart_in(custom.path(), "my-immich", "  yolab.io/github: immich-app/immich\n");
        chart_in(custom.path(), "notes", "  yolab.io/github: someone/notes\n");
        let dirs = vec![official.path().to_path_buf(), custom.path().to_path_buf()];
        assert_eq!(
            github_repos(&dirs),
            vec!["immich-app/immich".to_string(), "someone/notes".to_string()]
        );
    }

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

    mod against_the_world {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, status};
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const REPOS_PATH: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-chart-repos";

        async fn catalog(body: &str) -> (MockServer, ChartRepo) {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/catalog.yaml"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            let repo = ChartRepo {
                name: "community".into(),
                url: format!("{}/catalog.yaml", server.uri()),
                removable: true,
            };
            (server, repo)
        }

        #[tokio::test]
        async fn a_catalog_pointing_anywhere_but_an_oci_registry_is_refused() {
            let (_server, repo) = catalog("registry: https://evil.example\n").await;
            assert!(fetch_manifest(&repo).await.is_err());
        }

        #[tokio::test]
        async fn a_chart_is_pulled_at_the_version_the_catalog_names() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts/\ncharts:\n  - name: notes\n    version: \"1.2.3\"\n",
            )
            .await;
            let host = FakeHost::new().ok("helm pull", "");
            let cache = tempfile::tempdir().unwrap();
            sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .unwrap();
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes --version 1.2.3 --untar"));
        }

        #[tokio::test]
        async fn a_chart_the_catalog_does_not_list_is_never_pulled() {
            let (_server, repo) = catalog("registry: oci://ghcr.io/x/charts\n").await;
            let host = FakeHost::new();
            let cache = tempfile::tempdir().unwrap();
            assert!(sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .is_err());
            assert!(host.calls().is_empty());
        }

        #[tokio::test]
        async fn the_newest_chart_is_fetched_from_the_catalog_the_app_came_from() {
            let (_server, community) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"2\"\n",
            )
            .await;
            let official = ChartRepo {
                name: OFFICIAL.into(),
                url: "http://[::1]:9/unreachable.yaml".into(),
                removable: false,
            };
            let host = FakeHost::new().ok("helm pull", "");
            let cache = tempfile::tempdir().unwrap();
            let from = fetch_newest(
                &host,
                cache.path(),
                &[official, community],
                "notes",
                Some("community"),
            )
            .await
            .unwrap();
            assert_eq!(from, "community");
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes --version 2"));
        }

        #[tokio::test]
        async fn without_a_preference_the_first_catalog_that_has_the_chart_wins() {
            let (_empty, without) = catalog("registry: oci://ghcr.io/x/charts\n").await;
            let (_server, with) = catalog(
                "registry: oci://ghcr.io/y/charts\ncharts:\n  - name: notes\n    version: \"3\"\n",
            )
            .await;
            let with = ChartRepo {
                name: "second".into(),
                ..with
            };
            let host = FakeHost::new().ok("helm pull", "");
            let cache = tempfile::tempdir().unwrap();
            let from = fetch_newest(&host, cache.path(), &[without, with], "notes", None)
                .await
                .unwrap();
            assert_eq!(from, "second");
            assert!(host.ran("helm pull oci://ghcr.io/y/charts/notes --version 3"));
        }

        #[tokio::test]
        async fn a_chart_no_catalog_can_supply_says_why() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1\"\n",
            )
            .await;
            let host = FakeHost::new().fail("helm pull", "manifest unknown");
            let cache = tempfile::tempdir().unwrap();
            let repos = [repo];
            let e = fetch_newest(&host, cache.path(), &repos, "notes", None)
                .await
                .unwrap_err();
            assert!(e.to_string().contains("manifest unknown"), "{e}");
            let e = fetch_newest(&host, cache.path(), &repos, "notes", Some("custom"))
                .await
                .unwrap_err();
            assert!(e.to_string().contains("no catalog"), "{e}");
        }

        #[tokio::test]
        async fn an_exact_chart_is_pulled_at_its_own_version_even_after_the_catalog_moved_on() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"9.0.0\"\n",
            )
            .await;
            let host = FakeHost::new().ok("helm pull", "");
            let dest = tempfile::tempdir().unwrap();
            let dir = fetch_exact(&host, dest.path(), &[repo], "notes", "1.2.3", "community")
                .await
                .unwrap();
            assert_eq!(dir, dest.path().join("notes"));
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes --version 1.2.3 --untar"));
            assert!(!host.ran("--version 9.0.0"));
        }

        #[tokio::test]
        async fn an_exact_chart_from_a_removed_repository_is_refused_without_pulling() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1\"\n",
            )
            .await;
            let host = FakeHost::new();
            let dest = tempfile::tempdir().unwrap();
            let e = fetch_exact(&host, dest.path(), &[repo], "notes", "1", "gone")
                .await
                .unwrap_err();
            assert!(e.to_string().contains("gone"), "{e}");
            assert!(host.calls().is_empty());
        }

        #[test]
        fn a_cached_chart_is_used_only_at_the_exact_version_asked_for() {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join(CUSTOM).join("notes");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Chart.yaml"), "name: notes\nversion: 1.2.3\n").unwrap();
            assert_eq!(
                cached_at_version(root.path(), CUSTOM, "notes", "1.2.3"),
                Some(dir)
            );
            assert_eq!(
                cached_at_version(root.path(), CUSTOM, "notes", "1.2.4"),
                None
            );
            assert_eq!(
                cached_at_version(root.path(), OFFICIAL, "notes", "1.2.3"),
                None
            );
        }

        #[tokio::test]
        async fn a_failed_pull_is_reported_with_helms_reason() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1\"\n",
            )
            .await;
            let host = FakeHost::new().fail("helm pull", "manifest unknown");
            let cache = tempfile::tempdir().unwrap();
            let e = sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .unwrap_err();
            assert!(e.to_string().contains("manifest unknown"), "{e}");
        }

        #[tokio::test]
        async fn the_official_catalog_is_listed_even_when_the_cluster_is_down() {
            let (server, kube) = api_server().await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(503).set_body_json(status(503, "ServiceUnavailable")),
                )
                .mount(&server)
                .await;
            let repos = list_repos(&kube).await;
            assert_eq!(repos.len(), 1);
            assert_eq!(repos[0].name, OFFICIAL);
            assert!(!repos[0].removable);
        }

        #[tokio::test]
        async fn the_first_added_repo_creates_the_list() {
            let (server, kube) = api_server().await;
            Mock::given(method("PATCH"))
                .and(path(REPOS_PATH))
                .and(wiremock::matchers::header(
                    "content-type",
                    "application/merge-patch+json",
                ))
                .respond_with(ResponseTemplate::new(404).set_body_json(status(404, "NotFound")))
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .and(path(REPOS_PATH))
                .and(wiremock::matchers::header("content-type", "application/apply-patch+yaml"))
                .and(body_partial_json(serde_json::json!({ "data": { "community": "https://c.example/catalog.yaml" } })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "metadata": { "name": "yolab-chart-repos", "namespace": "kube-system" }
                })))
                .expect(1)
                .mount(&server)
                .await;
            add_repo(&kube, "community", "https://c.example/catalog.yaml")
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn a_bad_repo_is_refused_before_the_cluster_is_asked() {
            let (server, kube) = api_server().await;
            assert!(add_repo(&kube, "community", "http://plain.example")
                .await
                .is_err());
            assert!(add_repo(&kube, OFFICIAL, "https://x.example")
                .await
                .is_err());
            assert!(remove_repo(&kube, OFFICIAL).await.is_err());
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}
