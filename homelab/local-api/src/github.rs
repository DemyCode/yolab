use std::collections::HashMap;

use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const API: &str = "https://api.github.com";
const STATS_CM: &str = "yolab-github-stats";
const STATS_NS: &str = "kube-system";
pub(crate) const MAX_AGE_SECS: i64 = 24 * 3600;
pub(crate) const BATCH: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RepoStats {
    pub stars: u64,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub archived: bool,
    pub fetched_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    Stats(RepoStats),
    Missing,
    RateLimited,
}

pub(crate) fn is_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let valid = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => valid(owner) && valid(name),
        _ => false,
    }
}

fn key_for(repo: &str) -> String {
    repo.replacen('/', "__", 1)
}

fn repo_for(key: &str) -> String {
    key.replacen("__", "/", 1)
}

pub(crate) fn parse_repo(v: &Value, now: i64) -> Option<RepoStats> {
    Some(RepoStats {
        stars: v["stargazers_count"].as_u64()?,
        pushed_at: v["pushed_at"].as_str().map(str::to_string),
        archived: v["archived"].as_bool().unwrap_or(false),
        fetched_at: now,
    })
}

pub(crate) fn stale<'a>(
    repos: &'a [String],
    known: &HashMap<String, RepoStats>,
    now: i64,
    batch: usize,
) -> Vec<&'a String> {
    let mut due: Vec<(i64, &String)> = repos
        .iter()
        .filter(|r| is_repo(r))
        .filter_map(|r| match known.get(r.as_str()) {
            None => Some((i64::MIN, r)),
            Some(s) if now - s.fetched_at >= MAX_AGE_SECS => Some((s.fetched_at, r)),
            Some(_) => None,
        })
        .collect();
    due.sort();
    due.dedup_by(|a, b| a.1 == b.1);
    due.into_iter().take(batch).map(|(_, r)| r).collect()
}

pub(crate) async fn fetch(
    http: &crate::http::Client,
    base: &str,
    repo: &str,
    now: i64,
) -> anyhow::Result<Fetched> {
    let response = http
        .get(format!("{}/repos/{repo}", base.trim_end_matches('/')))
        .header("user-agent", "yolab")
        .header("accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    match response.status().as_u16() {
        200 => {
            let body: Value = response.json().await?;
            parse_repo(&body, now)
                .map(Fetched::Stats)
                .ok_or_else(|| anyhow::anyhow!("{repo}: GitHub answered without a star count"))
        }
        404 | 451 => Ok(Fetched::Missing),
        403 | 429 => Ok(Fetched::RateLimited),
        other => anyhow::bail!("{repo}: GitHub answered {other}"),
    }
}

fn stats_ref() -> Value {
    crate::k8s::reference("v1", "ConfigMap", STATS_NS, STATS_CM)
}

pub(crate) fn from_config_map(cm: &Value) -> HashMap<String, RepoStats> {
    cm["data"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, raw)| {
            let stats = serde_json::from_str(raw.as_str()?).ok()?;
            Some((repo_for(key), stats))
        })
        .collect()
}

pub(crate) async fn read_all(client: &Client) -> anyhow::Result<HashMap<String, RepoStats>> {
    Ok(crate::k8s::get(client, &stats_ref())
        .await?
        .map(|cm| from_config_map(&cm))
        .unwrap_or_default())
}

async fn store(client: &Client, repo: &str, stats: &RepoStats) -> anyhow::Result<()> {
    let mut patch = stats_ref();
    patch["data"] = json!({ key_for(repo): serde_json::to_string(stats)? });
    match crate::k8s::merge_patch(client, &patch).await {
        Err(e) if crate::k8s::refused_with(&e, 404) => crate::k8s::apply(client, &patch).await,
        other => other,
    }
}

pub(crate) async fn refresh(
    client: &Client,
    http: &crate::http::Client,
    base: &str,
    repos: &[String],
    now: i64,
) -> anyhow::Result<usize> {
    let known = read_all(client).await?;
    let mut refreshed = 0;
    for repo in stale(repos, &known, now, BATCH) {
        let stats = match fetch(http, base, repo, now).await? {
            Fetched::Stats(stats) => stats,
            Fetched::Missing => RepoStats {
                stars: 0,
                pushed_at: None,
                archived: true,
                fetched_at: now,
            },
            Fetched::RateLimited => break,
        };
        store(client, repo, &stats).await?;
        refreshed += 1;
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW: i64 = 1_800_000_000;

    fn stats(fetched_at: i64) -> RepoStats {
        RepoStats {
            stars: 1,
            pushed_at: None,
            archived: false,
            fetched_at,
        }
    }

    #[test]
    fn only_an_owner_and_a_repository_make_a_path() {
        for good in [
            "immich-app/immich",
            "dgtlmoon/changedetection.io",
            "Yooooomi/your_spotify",
        ] {
            assert!(is_repo(good), "{good}");
        }
        for bad in [
            "immich",
            "a/b/c",
            "/immich",
            "owner/",
            "o wner/r",
            "https://github.com/a/b",
        ] {
            assert!(!is_repo(bad), "{bad}");
        }
    }

    #[test]
    fn a_repository_round_trips_through_its_config_map_key() {
        for repo in ["Yooooomi/your_spotify", "miniflux/v2"] {
            assert_eq!(repo_for(&key_for(repo)), repo);
        }
    }

    #[test]
    fn github_s_answer_becomes_stars_and_last_push() {
        let body = json!({
            "stargazers_count": 52_000, "pushed_at": "2026-10-01T10:00:00Z", "archived": false
        });
        assert_eq!(
            parse_repo(&body, NOW),
            Some(RepoStats {
                stars: 52_000,
                pushed_at: Some("2026-10-01T10:00:00Z".into()),
                archived: false,
                fetched_at: NOW,
            })
        );
        assert_eq!(parse_repo(&json!({ "message": "Not Found" }), NOW), None);
    }

    #[test]
    fn never_fetched_repositories_go_first_then_the_oldest_and_fresh_ones_wait() {
        let repos: Vec<String> = ["a/fresh", "a/old", "a/never", "a/older", "not a repo"]
            .iter()
            .map(|r| r.to_string())
            .collect();
        let known = HashMap::from([
            ("a/fresh".to_string(), stats(NOW - 60)),
            ("a/old".to_string(), stats(NOW - MAX_AGE_SECS - 10)),
            ("a/older".to_string(), stats(NOW - MAX_AGE_SECS - 1000)),
        ]);
        assert_eq!(
            stale(&repos, &known, NOW, 10),
            vec!["a/never", "a/older", "a/old"]
        );
        assert_eq!(stale(&repos, &known, NOW, 1), vec!["a/never"]);
    }

    #[test]
    fn a_repository_two_apps_share_is_fetched_once() {
        let repos = vec!["a/b".to_string(), "a/b".to_string()];
        assert_eq!(stale(&repos, &HashMap::new(), NOW, 10), vec!["a/b"]);
    }

    #[test]
    fn stored_stats_read_back_under_their_repository_name() {
        let cm = json!({ "data": {
            "Yooooomi__your_spotify": serde_json::to_string(&stats(NOW)).unwrap(),
            "broken": "{not json"
        }});
        let all = from_config_map(&cm);
        assert_eq!(all.len(), 1);
        assert_eq!(all["Yooooomi/your_spotify"], stats(NOW));
    }

    #[tokio::test]
    async fn a_fetch_asks_github_politely_and_reads_the_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/immich-app/immich"))
            .and(header("user-agent", "yolab"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "stargazers_count": 7, "pushed_at": "2026-10-01T10:00:00Z"
            })))
            .mount(&server)
            .await;
        let got = fetch(&crate::testkit::http(), &server.uri(), "immich-app/immich", NOW)
            .await
            .unwrap();
        assert!(matches!(got, Fetched::Stats(RepoStats { stars: 7, .. })));
    }

    #[tokio::test]
    async fn a_rate_limit_is_reported_rather_than_read_as_zero_stars() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let got = fetch(&crate::testkit::http(), &server.uri(), "a/b", NOW)
            .await
            .unwrap();
        assert_eq!(got, Fetched::RateLimited);
    }

    #[tokio::test]
    async fn a_repository_that_is_gone_is_missing_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let got = fetch(&crate::testkit::http(), &server.uri(), "a/b", NOW)
            .await
            .unwrap();
        assert_eq!(got, Fetched::Missing);
    }
}
