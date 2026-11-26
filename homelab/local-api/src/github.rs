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
    patch["data"] = json!({ key_for(repo): json!(stats).to_string() });
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
    fn stats_go_stale_after_a_day() {
        assert_eq!(MAX_AGE_SECS, 86_400);
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
        let got = fetch(
            &crate::testkit::http(),
            &server.uri(),
            "immich-app/immich",
            NOW,
        )
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

    async fn github_answering(answers: &[(&str, u16, Value)]) -> MockServer {
        let server = MockServer::start().await;
        for (repo, code, body) in answers {
            Mock::given(method("GET"))
                .and(path(format!("/repos/{repo}")))
                .respond_with(ResponseTemplate::new(*code).set_body_json(body.clone()))
                .mount(&server)
                .await;
        }
        server
    }

    const STATS_PATH: &str = "/api/v1/namespaces/kube-system/configmaps/yolab-github-stats";

    fn stats_config_map() -> Value {
        json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": STATS_CM } })
    }

    async fn kube_without_stats() -> (MockServer, Client) {
        let (server, client) = crate::k8s::testing::api_server().await;
        crate::k8s::testing::serve(
            &server,
            STATS_PATH,
            404,
            crate::k8s::testing::status(404, "NotFound"),
        )
        .await;
        (server, client)
    }

    async fn patches_with(server: &MockServer, content_type: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .filter(|r| r.headers.get("content-type").unwrap().to_str().unwrap() == content_type)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn an_unreachable_github_is_an_error() {
        let base = format!("http://127.0.0.1:{}", crate::testkit::closed_port());
        assert!(fetch(&crate::testkit::http(), &base, "a/b", NOW)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_answer_that_is_not_json_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount(&server)
            .await;
        assert!(fetch(&crate::testkit::http(), &server.uri(), "a/b", NOW)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_answer_without_a_star_count_is_an_error_not_zero_stars() {
        let server = github_answering(&[("a/b", 200, json!({ "message": "hi" }))]).await;
        let err = fetch(&crate::testkit::http(), &server.uri(), "a/b", NOW)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "a/b: GitHub answered without a star count");
    }

    #[tokio::test]
    async fn an_unexpected_status_is_an_error() {
        let server = github_answering(&[("a/b", 500, json!({}))]).await;
        let err = fetch(&crate::testkit::http(), &server.uri(), "a/b", NOW)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "a/b: GitHub answered 500");
    }

    #[test]
    fn a_stored_value_that_is_not_text_is_skipped() {
        let cm = json!({ "data": { "a__b": 7 } });
        assert!(from_config_map(&cm).is_empty());
    }

    #[tokio::test]
    async fn stats_that_cannot_be_read_from_kubernetes_are_an_error() {
        assert!(read_all(&crate::k8s::testing::unreachable()).await.is_err());
        let refreshed = refresh(
            &crate::k8s::testing::unreachable(),
            &crate::testkit::http(),
            "http://127.0.0.1:9",
            &["a/b".to_string()],
            NOW,
        )
        .await;
        assert!(refreshed.is_err());
    }

    #[tokio::test]
    async fn a_refresh_stores_stars_marks_gone_repositories_archived_and_stops_at_a_rate_limit() {
        let github = github_answering(&[
            ("o/a-stars", 200, json!({ "stargazers_count": 9 })),
            ("o/b-gone", 404, json!({})),
            ("o/c-limited", 429, json!({})),
            ("o/d-later", 200, json!({ "stargazers_count": 1 })),
        ])
        .await;
        let (kube, client) = kube_without_stats().await;
        Mock::given(method("PATCH"))
            .and(path(STATS_PATH))
            .and(header("content-type", "application/merge-patch+json"))
            .and(wiremock::matchers::body_string_contains("o__a-stars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stats_config_map()))
            .with_priority(1)
            .mount(&kube)
            .await;
        Mock::given(method("PATCH"))
            .and(path(STATS_PATH))
            .and(header("content-type", "application/merge-patch+json"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(crate::k8s::testing::status(404, "NotFound")),
            )
            .with_priority(2)
            .mount(&kube)
            .await;
        Mock::given(method("PATCH"))
            .and(path(STATS_PATH))
            .and(header("content-type", "application/apply-patch+yaml"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stats_config_map()))
            .with_priority(2)
            .mount(&kube)
            .await;
        let repos: Vec<String> = ["o/d-later", "o/c-limited", "o/b-gone", "o/a-stars"]
            .iter()
            .map(|r| r.to_string())
            .collect();
        let refreshed = refresh(&client, &crate::testkit::http(), &github.uri(), &repos, NOW)
            .await
            .unwrap();
        assert_eq!(refreshed, 2);
        let stars = RepoStats {
            stars: 9,
            pushed_at: None,
            archived: false,
            fetched_at: NOW,
        };
        let gone = RepoStats {
            stars: 0,
            pushed_at: None,
            archived: true,
            fetched_at: NOW,
        };
        let merged = patches_with(&kube, "application/merge-patch+json").await;
        assert_eq!(
            merged.iter().map(from_config_map).collect::<Vec<_>>(),
            vec![
                HashMap::from([("o/a-stars".to_string(), stars)]),
                HashMap::from([("o/b-gone".to_string(), gone.clone())]),
            ]
        );
        let applied = patches_with(&kube, "application/apply-patch+yaml").await;
        assert_eq!(
            applied.iter().map(from_config_map).collect::<Vec<_>>(),
            vec![HashMap::from([("o/b-gone".to_string(), gone)])]
        );
    }

    #[tokio::test]
    async fn a_refresh_whose_fetch_fails_is_an_error() {
        let github = github_answering(&[("o/a", 500, json!({}))]).await;
        let (_kube, client) = kube_without_stats().await;
        let refreshed = refresh(
            &client,
            &crate::testkit::http(),
            &github.uri(),
            &["o/a".to_string()],
            NOW,
        )
        .await;
        assert!(refreshed.is_err());
    }

    #[tokio::test]
    async fn a_refresh_whose_store_fails_is_an_error() {
        let github = github_answering(&[("o/a", 200, json!({ "stargazers_count": 3 }))]).await;
        let (kube, client) = kube_without_stats().await;
        Mock::given(method("PATCH"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(crate::k8s::testing::status(500, "InternalError")),
            )
            .mount(&kube)
            .await;
        let refreshed = refresh(
            &client,
            &crate::testkit::http(),
            &github.uri(),
            &["o/a".to_string()],
            NOW,
        )
        .await;
        assert!(refreshed.is_err());
    }
}
