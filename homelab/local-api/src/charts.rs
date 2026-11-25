use kube::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::host::Host;

const REPO_CM: &str = "yolab-chart-repos";
const REPO_NS: &str = "kube-system";

#[cfg(not(test))]
pub fn cache_root() -> PathBuf {
    PathBuf::from("/var/lib/yolab/charts")
}

#[cfg(test)]
pub fn cache_root() -> PathBuf {
    testing::CACHE_ROOT
        .with(|root| root.borrow().clone())
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("yolab-test-charts-{}", std::process::id()))
        })
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        pub(super) static CACHE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(crate) struct CacheRoot {
        pub dir: tempfile::TempDir,
    }

    impl Drop for CacheRoot {
        fn drop(&mut self) {
            CACHE_ROOT.with(|root| *root.borrow_mut() = None);
        }
    }

    pub(crate) fn cache_in_temp() -> CacheRoot {
        let dir = tempfile::tempdir().expect("a temporary chart cache");
        CACHE_ROOT.with(|root| *root.borrow_mut() = Some(dir.path().to_path_buf()));
        CacheRoot { dir }
    }

    pub(crate) fn chart(root: &std::path::Path, repo: &str, name: &str, version: &str) {
        let dir = root.join(repo).join(name);
        std::fs::create_dir_all(&dir).expect("a chart folder");
        std::fs::write(
            dir.join("Chart.yaml"),
            format!("apiVersion: v2\nname: {name}\nversion: {version}\n"),
        )
        .expect("a Chart.yaml");
    }

    thread_local! {
        pub(super) static OFFICIAL_URL: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(crate) struct OfficialAt;

    impl Drop for OfficialAt {
        fn drop(&mut self) {
            OFFICIAL_URL.with(|url| *url.borrow_mut() = None);
        }
    }

    pub(crate) fn official_at(url: &str) -> OfficialAt {
        OFFICIAL_URL.with(|official| *official.borrow_mut() = Some(url.to_string()));
        OfficialAt
    }
}

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

#[cfg(not(test))]
fn official_url() -> String {
    default_official_url()
}

#[cfg(test)]
fn official_url() -> String {
    testing::OFFICIAL_URL
        .with(|url| url.borrow().clone())
        .unwrap_or_else(default_official_url)
}

fn default_official_url() -> String {
    std::env::var("YOLAB_OFFICIAL_CHART_REPO").unwrap_or_else(|_| {
        "https://raw.githubusercontent.com/DemyCode/yolab/refs/heads/main/catalog.yaml".into()
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
            Ok(None) => Default::default(),
            Ok(Some(cm)) => match serde_json::from_value(cm["data"].clone()) {
                Ok(stored) => stored,
                Err(e) => {
                    tracing::warn!("the added chart repositories are not a name-to-url map: {e}");
                    Default::default()
                }
            },
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
    crate::error::remove_dir_if_present(&dir).await?;
    Ok(())
}

fn cache_dir_for(repo: &str) -> PathBuf {
    cache_root().join(repo)
}

pub fn official_dir() -> PathBuf {
    cache_dir_for(OFFICIAL)
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct CatalogManifest {
    pub registry: String,
    #[serde(default)]
    pub libraries: Vec<CatalogEntry>,
    #[serde(default)]
    pub charts: Vec<CatalogEntry>,
}

#[derive(Deserialize, Debug, PartialEq, Clone)]
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
    tokio::fs::create_dir_all(dir).await?;
    let staging = tempfile::tempdir_in(dir)?;
    let untar_dir = staging.path().to_string_lossy();
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
    let pulled = staging.path().join(&entry.name);
    if !pulled.join("Chart.yaml").is_file() {
        anyhow::bail!(
            "pull {reference}:{}: helm finished but left no chart named {} behind",
            entry.version,
            entry.name
        );
    }
    crate::error::remove_dir_if_present(&dir.join(&entry.name)).await?;
    tokio::fs::rename(&pulled, dir.join(&entry.name)).await?;
    Ok(())
}

fn http() -> crate::http::Client {
    crate::http::client()
}

const MANIFEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn github_contents_url(api: &str, url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://raw.githubusercontent.com/")?;
    let mut parts = rest.splitn(3, '/');
    let (Some(owner), Some(repo), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let (git_ref, file) = tail
        .strip_prefix("refs/heads/")
        .or_else(|| tail.strip_prefix("refs/tags/"))
        .unwrap_or(tail)
        .split_once('/')?;
    if [owner, repo, git_ref, file].iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(format!(
        "{}/repos/{owner}/{repo}/contents/{file}?ref={git_ref}",
        api.trim_end_matches('/')
    ))
}

async fn body_of(request: crate::http::RequestBuilder) -> anyhow::Result<String> {
    Ok(request
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

async fn manifest_text(repo: &ChartRepo, api: &str) -> anyhow::Result<String> {
    if let Some(contents) = github_contents_url(api, &repo.url) {
        let request = http()
            .get(&contents)
            .header("user-agent", "yolab")
            .header("accept", "application/vnd.github.raw+json");
        match body_of(request).await {
            Ok(body) => return Ok(body),
            Err(e) => tracing::warn!(
                "{}: the GitHub API did not serve the catalog, reading {} instead, \
                 which GitHub may serve up to 5 minutes old: {e:#}",
                repo.name,
                repo.url
            ),
        }
    }
    body_of(http().get(&repo.url)).await
}

async fn fetch_manifest(repo: &ChartRepo) -> anyhow::Result<CatalogManifest> {
    fetch_manifest_via(repo, crate::github::API).await
}

async fn fetch_manifest_via(repo: &ChartRepo, api: &str) -> anyhow::Result<CatalogManifest> {
    let body = manifest_text(repo, api).await?;
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

    static SYNCING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = SYNCING.lock().await;

    let dir = cache_root.join(&repo.name);
    tokio::fs::create_dir_all(&dir).await?;
    let stale = |entry: &CatalogEntry| {
        cached_at_version(cache_root, &repo.name, &entry.name, &entry.version).is_none()
    };

    for library in manifest.libraries.iter().filter(|l| stale(l)) {
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

    let wanted: Vec<CatalogEntry> = manifest
        .charts
        .iter()
        .filter(|e| stale(e))
        .filter(|e| {
            let usable = valid_chart_name(&e.name);
            if !usable {
                tracing::warn!(
                    "{}: skipping chart with unusable name {:?}",
                    repo.name,
                    e.name
                );
            }
            usable
        })
        .cloned()
        .collect();
    let dir = dir.as_path();
    let registry = manifest.registry.as_str();
    Ok(pull_all(wanted, |entry: CatalogEntry| async move {
        pull_into(host, dir, registry, &entry).await
    })
    .await)
}

const PARALLEL_PULLS: usize = 8;

async fn pull_all<T, F, Fut>(items: Vec<T>, pull: F) -> usize
where
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures::StreamExt;
    futures::stream::iter(items)
        .map(pull)
        .buffer_unordered(PARALLEL_PULLS)
        .fold(0usize, |pulled, result| async move {
            match result {
                Ok(()) => pulled + 1,
                Err(e) => {
                    tracing::warn!("{e:#}");
                    pulled
                }
            }
        })
        .await
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

const CHART_SYNC_EVERY: std::time::Duration = std::time::Duration::from_secs(600);
const STARS_EVERY_SECS: u64 = 3600;
static LAST_STARS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn stars_due(last: &std::sync::atomic::AtomicU64, now: u64) -> bool {
    use std::sync::atomic::Ordering;
    let before = last.load(Ordering::Relaxed);
    if before != 0 && now.saturating_sub(before) < STARS_EVERY_SECS {
        return false;
    }
    last.compare_exchange(before, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

pub struct ChartSyncController;

impl crate::runtime::Resource for ChartSyncController {
    fn name(&self) -> &'static str {
        "chart-sync"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Node
    }
    fn interval(&self) -> std::time::Duration {
        CHART_SYNC_EVERY
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &[crate::runtime::KUBE_API]
    }
    async fn converge(&self, ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let b = ctx.tools.backend().await?;
        sync_all(&b.kube, &b.host).await
    }
}

async fn refresh_github_stars(client: &Client, api: &str) {
    let dirs: Vec<PathBuf> = chart_sources(client)
        .await
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    let repos = github_repos(&dirs);
    let now = i64::try_from(crate::system::now_secs()).unwrap_or(i64::MAX);
    match crate::github::refresh(client, &http(), api, &repos, now).await {
        Ok(n) if n > 0 => tracing::info!("chart sync: GitHub stars refreshed for {n} project(s)"),
        Ok(_) => {}
        Err(e) => tracing::warn!("chart sync: GitHub stars not refreshed: {e:#}"),
    }
}

async fn sync_all<H: Host>(client: &Client, host: &H) -> anyhow::Result<crate::runtime::Tick> {
    sync_all_with(client, host, &LAST_STARS, crate::github::API).await
}

async fn sync_all_with<H: Host>(
    client: &Client,
    host: &H,
    stars: &std::sync::atomic::AtomicU64,
    github_api: &str,
) -> anyhow::Result<crate::runtime::Tick> {
    let mut failed = Vec::new();
    for repo in list_repos(client).await {
        match sync_repo(host, &cache_root(), &repo).await {
            Ok(n) if n > 0 => tracing::info!("chart sync: {} — {n} chart(s)", repo.name),
            Ok(_) => {}
            Err(e) => failed.push(format!("{}: {e}", repo.name)),
        }
    }
    if stars_due(stars, crate::system::now_secs()) {
        refresh_github_stars(client, github_api).await;
    }
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
        chart_in(
            official.path(),
            "immich",
            "  yolab.io/github: immich-app/immich\n",
        );
        chart_in(
            official.path(),
            "media-stack",
            "  yolab.io/tagline: no project\n",
        );
        chart_in(official.path(), "broken", "  yolab.io/github: not a path\n");
        chart_in(
            custom.path(),
            "my-immich",
            "  yolab.io/github: immich-app/immich\n",
        );
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
        assert!(m.libraries.is_empty());
    }

    #[test]
    fn manifest_reads_every_library_it_lists() {
        let m: CatalogManifest = serde_norway::from_str(
            "registry: oci://ghcr.io/demycode/charts\n\
             libraries:\n\
             \x20 - name: yolab-common\n\
             \x20   version: \"0.1.15\"\n\
             \x20 - name: yolab-stack\n\
             \x20   version: \"0.1.0\"\n\
             charts:\n\
             \x20 - name: gitea\n\
             \x20   version: \"0.1.1\"\n",
        )
        .unwrap();
        assert_eq!(
            m.libraries
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            vec!["yolab-common", "yolab-stack"]
        );
        assert_eq!(m.charts.len(), 1);
    }

    #[test]
    fn the_official_catalog_is_read_through_the_github_api_which_skips_the_raw_file_cache() {
        assert_eq!(
            github_contents_url("https://api.github.com", &official_url()).as_deref(),
            Some("https://api.github.com/repos/DemyCode/yolab/contents/catalog.yaml?ref=main")
        );
    }

    #[test]
    fn a_raw_github_url_keeps_its_branch_and_nested_path() {
        assert_eq!(
            github_contents_url(
                "https://api.github.com/",
                "https://raw.githubusercontent.com/me/charts/dev/out/catalog.yaml"
            )
            .as_deref(),
            Some("https://api.github.com/repos/me/charts/contents/out/catalog.yaml?ref=dev")
        );
        assert_eq!(
            github_contents_url(
                "https://api.github.com",
                "https://raw.githubusercontent.com/me/charts/refs/tags/v1/catalog.yaml"
            )
            .as_deref(),
            Some("https://api.github.com/repos/me/charts/contents/catalog.yaml?ref=v1")
        );
    }

    #[test]
    fn a_catalog_hosted_anywhere_but_raw_github_is_read_from_its_own_url() {
        assert_eq!(
            github_contents_url(
                "https://api.github.com",
                "https://charts.example.com/catalog.yaml"
            ),
            None
        );
        assert_eq!(
            github_contents_url(
                "https://api.github.com",
                "https://raw.githubusercontent.com/me/charts/main"
            ),
            None
        );
    }

    #[test]
    fn new_charts_are_looked_for_every_ten_minutes_but_stars_only_hourly() {
        assert_eq!(CHART_SYNC_EVERY, std::time::Duration::from_secs(600));
        let last = std::sync::atomic::AtomicU64::new(0);
        assert!(stars_due(&last, 10_000));
        assert!(!stars_due(&last, 10_600));
        assert!(!stars_due(&last, 13_599));
        assert!(stars_due(&last, 13_600));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_charts_are_pulled_several_at_a_time() {
        let started = tokio::time::Instant::now();
        let pulled = pull_all((0..PARALLEL_PULLS).collect(), |_| async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(())
        })
        .await;
        assert_eq!(pulled, PARALLEL_PULLS);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[tokio::test]
    async fn a_failed_pull_is_not_counted_and_does_not_stop_the_others() {
        let pulled = pull_all(vec![true, false, true], |ok| async move {
            if ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("not found"))
            }
        })
        .await;
        assert_eq!(pulled, 2);
    }

    mod against_the_world {
        use super::*;
        use crate::host::fake::FakeHost;
        use crate::k8s::testing::{api_server, status};
        use wiremock::matchers::{body_partial_json, header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const REPOS_PATH: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-chart-repos";

        fn pulling() -> FakeHost {
            FakeHost::new()
                .ok("helm pull", "")
                .effect("helm pull", |_, args| {
                    let name = args[1].rsplit('/').next().unwrap();
                    let dir = std::path::Path::new(args[6]).join(name);
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(
                        dir.join("Chart.yaml"),
                        format!("name: {name}\nversion: {}\n", args[3]),
                    )
                    .unwrap();
                })
        }

        #[tokio::test]
        async fn a_pull_that_leaves_no_chart_behind_is_an_error() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1\"\n",
            )
            .await;
            let host = FakeHost::new().ok("helm pull", "");
            let cache = tempfile::tempdir().unwrap();
            assert_eq!(
                sync_chart(&host, cache.path(), &repo, "notes")
                    .await
                    .unwrap_err()
                    .to_string(),
                "pull oci://ghcr.io/x/charts/notes:1: helm finished but left no chart named notes behind"
            );
        }

        #[tokio::test]
        async fn a_pulled_chart_replaces_the_cached_copy() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"2\"\n",
            )
            .await;
            let cache = tempfile::tempdir().unwrap();
            let dir = cache.path().join(&repo.name).join("notes");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Chart.yaml"), "name: notes\nversion: 1\n").unwrap();
            sync_chart(&pulling(), cache.path(), &repo, "notes")
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("Chart.yaml")).unwrap(),
                "name: notes\nversion: 2\n"
            );
        }

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
        async fn a_raw_github_catalog_is_fetched_from_the_api_as_raw_content() {
            let api = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/o/r/contents/catalog.yaml"))
                .and(query_param("ref", "main"))
                .and(header("user-agent", "yolab"))
                .and(header("accept", "application/vnd.github.raw+json"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string("registry: oci://ghcr.io/x/charts\ncharts: []\n"),
                )
                .expect(1)
                .mount(&api)
                .await;
            let repo = ChartRepo {
                name: OFFICIAL.into(),
                url: "https://raw.githubusercontent.com/o/r/refs/heads/main/catalog.yaml".into(),
                removable: false,
            };
            let manifest = fetch_manifest_via(&repo, &api.uri()).await.unwrap();
            assert_eq!(manifest.registry, "oci://ghcr.io/x/charts");
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
            let host = pulling();
            let cache = tempfile::tempdir().unwrap();
            sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .unwrap();
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes --version 1.2.3 --untar"));
        }

        #[tokio::test]
        async fn a_sync_pulls_only_the_charts_whose_version_moved() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\n\
                 libraries:\n  - name: yolab-common\n    version: \"0.1.0\"\n\
                 charts:\n  - name: notes\n    version: \"1.0.0\"\n  - name: wiki\n    version: \"2.0.0\"\n",
            )
            .await;
            let cache = tempfile::tempdir().unwrap();
            for (name, version) in [
                ("yolab-common", "0.1.0"),
                ("notes", "1.0.0"),
                ("wiki", "1.0.0"),
            ] {
                let dir = cache.path().join(&repo.name).join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("Chart.yaml"),
                    format!("name: {name}\nversion: {version}\n"),
                )
                .unwrap();
            }
            let host = pulling();
            assert_eq!(sync_repo(&host, cache.path(), &repo).await.unwrap(), 1);
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/wiki --version 2.0.0"));
            assert!(!host.ran("charts/notes"));
            assert!(!host.ran("charts/yolab-common"));
        }

        #[tokio::test]
        async fn a_sync_pulls_a_new_library_next_to_the_one_it_has() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\n\
                 libraries:\n  - name: yolab-common\n    version: \"0.1.0\"\n  - name: yolab-stack\n    version: \"0.1.0\"\n\
                 charts: []\n",
            )
            .await;
            let cache = tempfile::tempdir().unwrap();
            let dir = cache.path().join(&repo.name).join("yolab-common");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Chart.yaml"),
                "name: yolab-common\nversion: 0.1.0\n",
            )
            .unwrap();
            let host = pulling();
            sync_repo(&host, cache.path(), &repo).await.unwrap();
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/yolab-stack --version 0.1.0"));
            assert!(!host.ran("charts/yolab-common"));
        }

        #[tokio::test]
        async fn a_version_the_registry_does_not_have_leaves_the_last_good_copy() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"2.0.0\"\n",
            )
            .await;
            let cache = tempfile::tempdir().unwrap();
            let dir = cache.path().join(&repo.name).join("notes");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Chart.yaml"), "name: notes\nversion: 1.0.0\n").unwrap();
            let host = FakeHost::new().fail("helm pull", "notes:2.0.0: not found");
            assert_eq!(sync_repo(&host, cache.path(), &repo).await.unwrap(), 0);
            assert_eq!(
                std::fs::read_to_string(dir.join("Chart.yaml")).unwrap(),
                "name: notes\nversion: 1.0.0\n"
            );
            let leftovers: Vec<_> = std::fs::read_dir(cache.path().join(&repo.name))
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(leftovers, vec!["notes"]);
        }

        #[test]
        fn the_official_catalog_is_read_from_the_main_branch_ref() {
            assert_eq!(
                official_url(),
                "https://raw.githubusercontent.com/DemyCode/yolab/refs/heads/main/catalog.yaml"
            );
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
            let host = pulling();
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
            let host = pulling();
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
            let host = pulling();
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

        use std::os::unix::fs::PermissionsExt as _;

        const STATS_PATH: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-github-stats";
        const NOTES_V1: &str =
            "registry: oci://ghcr.io/x/charts\ncharts:\n  - name: notes\n    version: \"1\"\n";

        fn config_map(name: &str, data: serde_json::Value) -> serde_json::Value {
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": { "name": name, "namespace": REPO_NS },
                "data": data,
            })
        }

        async fn repos_stored(server: &MockServer, data: serde_json::Value) {
            crate::k8s::testing::serve(server, REPOS_PATH, 200, config_map(REPO_CM, data)).await;
        }

        async fn nothing_at(server: &MockServer, at: &str) {
            crate::k8s::testing::serve(server, at, 404, status(404, "NotFound")).await;
        }

        async fn accepts_patches(server: &MockServer, at: &str) {
            Mock::given(method("PATCH"))
                .and(path(at))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(config_map("x", serde_json::json!({}))),
                )
                .mount(server)
                .await;
        }

        async fn refuses_patches(server: &MockServer, at: &str, code: u16) {
            Mock::given(method("PATCH"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(code).set_body_json(status(code, "Refused")))
                .mount(server)
                .await;
        }

        fn set_mode(dir: &Path, mode: u32) {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        fn closed_url() -> String {
            format!(
                "http://[::1]:{}/catalog.yaml",
                crate::testkit::closed_port()
            )
        }

        fn ctx_with(kube: crate::k8s::Kube) -> crate::runtime::Ctx {
            crate::runtime::Ctx {
                node: "node1".into(),
                tools: crate::tools::Tools {
                    host: FakeHost::new(),
                    kube,
                    http: crate::http::client(),
                },
            }
        }

        #[test]
        fn a_stored_repository_without_a_removable_flag_can_be_removed() {
            let repo: ChartRepo = serde_json::from_value(serde_json::json!({
                "name": "community", "url": "https://c.example/catalog.yaml"
            }))
            .unwrap();
            assert!(repo.removable);
        }

        #[tokio::test]
        async fn added_repositories_follow_the_official_one_which_they_cannot_replace() {
            let (server, kube) = api_server().await;
            repos_stored(
                &server,
                serde_json::json!({
                    "official": "https://evil.example/catalog.yaml",
                    "community": "https://c.example/catalog.yaml"
                }),
            )
            .await;
            assert_eq!(
                list_repos(&kube).await,
                vec![
                    ChartRepo {
                        name: OFFICIAL.into(),
                        url: official_url(),
                        removable: false,
                    },
                    ChartRepo {
                        name: "community".into(),
                        url: "https://c.example/catalog.yaml".into(),
                        removable: true,
                    },
                ]
            );
        }

        #[tokio::test]
        async fn a_repository_list_that_is_not_a_name_to_url_map_lists_only_the_official_one() {
            let (server, kube) = api_server().await;
            repos_stored(&server, serde_json::json!({ "community": 5 })).await;
            let repos = list_repos(&kube).await;
            assert_eq!(
                repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                vec![OFFICIAL]
            );
        }

        #[tokio::test]
        async fn a_removed_repository_is_forgotten_and_its_cached_charts_deleted() {
            let cache = crate::charts::testing::cache_in_temp();
            crate::charts::testing::chart(cache.dir.path(), "community", "notes", "1");
            let (server, kube) = api_server().await;
            accepts_patches(&server, REPOS_PATH).await;
            remove_repo(&kube, "community").await.unwrap();
            assert!(!cache.dir.path().join("community").exists());
            let (missing, kube) = api_server().await;
            refuses_patches(&missing, REPOS_PATH, 404).await;
            remove_repo(&kube, "community").await.unwrap();
        }

        #[tokio::test]
        async fn a_repository_list_that_cannot_be_updated_keeps_the_cached_charts() {
            let cache = crate::charts::testing::cache_in_temp();
            crate::charts::testing::chart(cache.dir.path(), "community", "notes", "1");
            let (server, kube) = api_server().await;
            refuses_patches(&server, REPOS_PATH, 500).await;
            assert!(remove_repo(&kube, "community").await.is_err());
            assert!(cache.dir.path().join("community").join("notes").is_dir());
        }

        #[tokio::test]
        async fn cached_charts_that_cannot_be_deleted_make_the_removal_an_error() {
            let cache = crate::charts::testing::cache_in_temp();
            std::fs::write(cache.dir.path().join("community"), "not a folder").unwrap();
            let (server, kube) = api_server().await;
            accepts_patches(&server, REPOS_PATH).await;
            assert!(remove_repo(&kube, "community").await.is_err());
        }

        #[test]
        fn a_raw_github_url_missing_its_owner_repository_or_file_is_read_from_its_own_url() {
            for url in [
                "https://raw.githubusercontent.com/me",
                "https://raw.githubusercontent.com/me/charts",
                "https://raw.githubusercontent.com//charts/main/catalog.yaml",
            ] {
                assert_eq!(
                    github_contents_url("https://api.github.com", url),
                    None,
                    "{url}"
                );
            }
        }

        #[tokio::test]
        async fn a_catalog_that_is_not_yaml_names_its_repository() {
            let (_server, repo) = catalog("registry: [unclosed").await;
            let e = fetch_manifest(&repo).await.unwrap_err();
            assert!(
                e.to_string()
                    .starts_with("community: catalog manifest is not valid: "),
                "{e}"
            );
        }

        #[tokio::test]
        async fn a_catalog_whose_body_breaks_off_is_an_error() {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut channel, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = channel.read(&mut buf).await.unwrap();
                    request.extend_from_slice(&buf[..read]);
                }
                channel
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nregistry")
                    .await
                    .unwrap();
            });
            let repo = ChartRepo {
                name: "community".into(),
                url: format!("http://[::1]:{port}/catalog.yaml"),
                removable: true,
            };
            assert!(fetch_manifest(&repo).await.is_err());
        }

        #[tokio::test]
        async fn an_unusable_chart_name_is_refused_before_any_catalog_is_read() {
            let host = FakeHost::new();
            let repo = ChartRepo {
                name: "community".into(),
                url: closed_url(),
                removable: true,
            };
            assert_eq!(
                sync_chart(&host, Path::new("/unused"), &repo, "../etc")
                    .await
                    .unwrap_err()
                    .to_string(),
                "unusable chart name \"../etc\""
            );
            assert_eq!(
                fetch_exact(
                    &host,
                    Path::new("/unused"),
                    &[repo],
                    "../etc",
                    "1",
                    "community"
                )
                .await
                .unwrap_err()
                .to_string(),
                "unusable chart name \"../etc\""
            );
            assert!(host.calls().is_empty());
        }

        #[tokio::test]
        async fn a_chart_cache_that_is_a_file_is_an_error() {
            let (_server, repo) = catalog(NOTES_V1).await;
            let file = tempfile::NamedTempFile::new().unwrap();
            assert!(sync_chart(&pulling(), file.path(), &repo, "notes")
                .await
                .is_err());
            assert!(sync_repo(&pulling(), file.path(), &repo).await.is_err());
        }

        #[tokio::test]
        async fn helm_that_cannot_start_is_an_error() {
            let (_server, repo) = catalog(NOTES_V1).await;
            let host = FakeHost::new().unstartable("helm pull");
            let cache = tempfile::tempdir().unwrap();
            assert!(sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .is_err());
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes"));
        }

        #[tokio::test]
        async fn a_cached_copy_that_cannot_be_removed_is_an_error() {
            let (_server, repo) = catalog(NOTES_V1).await;
            let cache = tempfile::tempdir().unwrap();
            let dir = cache.path().join(&repo.name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("notes"), "a file, not a chart").unwrap();
            assert!(sync_chart(&pulling(), cache.path(), &repo, "notes")
                .await
                .is_err());
        }

        #[tokio::test]
        async fn a_cache_folder_without_room_for_a_staging_folder_is_an_error() {
            let (_server, repo) = catalog(NOTES_V1).await;
            let cache = tempfile::tempdir().unwrap();
            let dir = cache.path().join(&repo.name);
            std::fs::create_dir_all(&dir).unwrap();
            set_mode(&dir, 0o555);
            let e = sync_chart(&FakeHost::new(), cache.path(), &repo, "notes")
                .await
                .unwrap_err();
            set_mode(&dir, 0o755);
            assert_eq!(
                e.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }

        #[tokio::test]
        async fn a_pulled_chart_that_cannot_be_moved_into_the_cache_is_an_error() {
            let (_server, repo) = catalog(NOTES_V1).await;
            let cache = tempfile::tempdir().unwrap();
            let host = FakeHost::new()
                .ok("helm pull", "")
                .effect("helm pull", |_, args| {
                    let staging = std::path::Path::new(args[6]);
                    let pulled = staging.join("notes");
                    std::fs::create_dir_all(&pulled).unwrap();
                    std::fs::write(pulled.join("Chart.yaml"), "name: notes\nversion: 1\n").unwrap();
                    set_mode(staging.parent().unwrap(), 0o555);
                });
            let e = sync_chart(&host, cache.path(), &repo, "notes")
                .await
                .unwrap_err();
            set_mode(&cache.path().join(&repo.name), 0o755);
            assert_eq!(
                e.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert!(!cache.path().join(&repo.name).join("notes").exists());
        }

        #[tokio::test]
        async fn an_exact_chart_fails_when_its_catalog_or_its_destination_cannot_be_used() {
            let gone = ChartRepo {
                name: "community".into(),
                url: closed_url(),
                removable: true,
            };
            assert!(fetch_exact(
                &FakeHost::new(),
                Path::new("/unused"),
                &[gone],
                "notes",
                "1",
                "community"
            )
            .await
            .is_err());
            let (_server, repo) = catalog(NOTES_V1).await;
            let file = tempfile::NamedTempFile::new().unwrap();
            let host = pulling();
            let e = fetch_exact(
                &host,
                &file.path().join("charts"),
                &[repo],
                "notes",
                "1",
                "community",
            )
            .await
            .unwrap_err();
            assert!(e.downcast_ref::<std::io::Error>().is_some(), "{e}");
            assert!(host.calls().is_empty());
        }

        #[test]
        fn a_cached_chart_without_a_version_is_not_used() {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join(CUSTOM).join("notes");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Chart.yaml"), "name: notes\n").unwrap();
            assert_eq!(cached_at_version(root.path(), CUSTOM, "notes", "1"), None);
        }

        #[tokio::test]
        async fn a_sync_skips_unusable_names_and_carries_on_past_failed_pulls() {
            let (_server, repo) = catalog(
                "registry: oci://ghcr.io/x/charts\n\
                 libraries:\n  - name: \"../evil\"\n    version: \"1\"\n  - name: yolab-common\n    version: \"1\"\n\
                 charts:\n  - name: Bad\n    version: \"1\"\n  - name: notes\n    version: \"1\"\n",
            )
            .await;
            let host = FakeHost::new().fail("helm pull", "manifest unknown");
            let cache = tempfile::tempdir().unwrap();
            assert_eq!(sync_repo(&host, cache.path(), &repo).await.unwrap(), 0);
            assert!(host.ran("charts/yolab-common --version 1"));
            assert!(host.ran("charts/notes --version 1"));
            assert!(!host.ran("evil"));
            assert!(!host.ran("charts/Bad"));
        }

        #[tokio::test]
        async fn a_sync_of_every_catalog_pulls_what_moved_and_refreshes_stars_when_due() {
            let cache = crate::charts::testing::cache_in_temp();
            let custom = cache.dir.path().join(CUSTOM).join("mine");
            std::fs::create_dir_all(&custom).unwrap();
            std::fs::write(
                custom.join("Chart.yaml"),
                "name: mine\nversion: 1.0.0\nannotations:\n  yolab.io/github: o/mine\n",
            )
            .unwrap();
            let (_catalog, official) = catalog(NOTES_V1).await;
            let _official = crate::charts::testing::official_at(&official.url);
            let github = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/o/mine"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "stargazers_count": 5 })),
                )
                .expect(1)
                .mount(&github)
                .await;
            let (server, kube) = api_server().await;
            nothing_at(&server, REPOS_PATH).await;
            nothing_at(&server, STATS_PATH).await;
            accepts_patches(&server, STATS_PATH).await;
            let host = pulling();
            let stars = std::sync::atomic::AtomicU64::new(0);

            let tick = sync_all_with(&kube, &host, &stars, &github.uri())
                .await
                .unwrap();

            assert_eq!(tick, crate::runtime::Tick::Done);
            assert!(host.ran("helm pull oci://ghcr.io/x/charts/notes --version 1"));
            assert!(crate::k8s::testing::patched(&server)
                .await
                .iter()
                .any(|p| p["data"]["o__mine"].is_string()));
        }

        #[tokio::test]
        async fn a_sync_with_nothing_new_asks_for_stars_only_once_an_hour() {
            let _cache = crate::charts::testing::cache_in_temp();
            let (_catalog, official) = catalog("registry: oci://ghcr.io/x/charts\n").await;
            let _official = crate::charts::testing::official_at(&official.url);
            let (server, kube) = api_server().await;
            nothing_at(&server, REPOS_PATH).await;
            nothing_at(&server, STATS_PATH).await;
            let host = FakeHost::new();
            let stars = std::sync::atomic::AtomicU64::new(0);

            for _ in 0..2 {
                assert_eq!(
                    sync_all_with(&kube, &host, &stars, &closed_url())
                        .await
                        .unwrap(),
                    crate::runtime::Tick::Done
                );
            }

            let stats_reads = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == STATS_PATH)
                .count();
            assert_eq!(stats_reads, 1);
            assert!(host.calls().is_empty());
        }

        #[tokio::test]
        async fn a_catalog_that_cannot_be_read_fails_the_sync_and_names_it() {
            let _cache = crate::charts::testing::cache_in_temp();
            let _official = crate::charts::testing::official_at(&closed_url());
            let stars = std::sync::atomic::AtomicU64::new(0);
            let e = sync_all_with(
                &crate::k8s::testing::unreachable(),
                &FakeHost::new(),
                &stars,
                &closed_url(),
            )
            .await
            .unwrap_err();
            assert!(e.to_string().starts_with("official: "), "{e}");
        }

        #[tokio::test]
        async fn the_chart_sync_runs_on_every_machine_every_ten_minutes_after_the_kube_api() {
            use crate::runtime::Resource as _;
            let c = ChartSyncController;
            assert_eq!(c.name(), "chart-sync");
            assert_eq!(c.scope(), crate::runtime::Scope::Node);
            assert_eq!(c.interval(), CHART_SYNC_EVERY);
            assert_eq!(c.depends_on(), &[crate::runtime::KUBE_API]);
            assert!(c
                .converge(&ctx_with(crate::k8s::Kube::from_environment()))
                .await
                .is_err());

            let _cache = crate::charts::testing::cache_in_temp();
            let (_catalog, official) = catalog("registry: oci://ghcr.io/x/charts\n").await;
            let _official = crate::charts::testing::official_at(&official.url);
            let (server, kube) = api_server().await;
            nothing_at(&server, REPOS_PATH).await;
            nothing_at(&server, STATS_PATH).await;
            assert_eq!(
                c.converge(&ctx_with(crate::k8s::Kube::with(kube)))
                    .await
                    .unwrap(),
                crate::runtime::Tick::Done
            );
        }
    }
}
