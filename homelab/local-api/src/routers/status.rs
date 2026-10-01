use std::time::Duration;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::Result, AppState};

#[derive(Serialize)]
pub struct StatusInfo {
    pub commit_hash: String,
    pub commit_message: String,
    pub commit_date: String,
    pub platform: String,
    pub flake_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn console_url_from_api(api_url: &str) -> Option<String> {
    let api_url = api_url.trim().trim_end_matches('/');
    if api_url.is_empty() {
        return None;
    }
    let (scheme, host) = api_url.split_once("://")?;
    let rest = host.strip_prefix("api.")?;
    if rest.is_empty() {
        return None;
    }
    Some(format!("{scheme}://console.{rest}"))
}

fn built(state: &AppState, filename: &str) -> String {
    std::fs::read_to_string(state.config.built_dir.join(filename))
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub async fn handler(State(state): State<AppState>) -> Result<Json<StatusInfo>> {
    Ok(Json(StatusInfo {
        commit_hash: built(&state, "built-hash"),
        commit_message: built(&state, "built-message"),
        commit_date: built(&state, "built-date"),
        platform: state.config.platform.clone(),
        flake_target: state.config.flake_target.clone(),
        console_url: platform_api_url(&state.config)
            .as_deref()
            .and_then(console_url_from_api),
        error: None,
    }))
}

fn platform_api_url(cfg: &Config) -> Option<String> {
    cfg.tunnel_table()?
        .get("platform_api_url")?
        .as_str()
        .map(String::from)
}

#[derive(Serialize)]
pub struct ConsoleLink {
    pub url: String,
}

pub(crate) fn console_link_url(console: &str, token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return Some(console.to_string());
    }
    Some(format!("{console}/#token={}", percent_encode(token)))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub async fn console_link(State(state): State<AppState>) -> Result<Json<ConsoleLink>> {
    let console = platform_api_url(&state.config)
        .as_deref()
        .and_then(console_url_from_api)
        .ok_or_else(|| anyhow::anyhow!("no console URL for this server"))?;

    let token = crate::config::read_account_token(&state.config.config_path);
    let url = console_link_url(&console, &token)
        .ok_or_else(|| anyhow::anyhow!("could not build the console link"))?;

    Ok(Json(ConsoleLink { url }))
}

pub const INVOICE_THRESHOLD_CENTS: i64 = 1000;

#[derive(Serialize, Debug, PartialEq)]
pub struct BillingTotal {
    pub total_cents: i64,
    pub threshold_cents: i64,
}

#[derive(Deserialize)]
struct PlatformBalance {
    balance_cents: i64,
}

#[derive(Deserialize)]
struct PlatformEstimate {
    total_cents: i64,
}

pub async fn billing_balance(State(state): State<AppState>) -> Result<Json<BillingTotal>> {
    let api = platform_api_url(&state.config)
        .ok_or_else(|| anyhow::anyhow!("this server is not connected to the YoLab platform"))?;
    let token = crate::config::read_account_token(&state.config.config_path);
    Ok(Json(fetch_total(&state.http, &api, &token).await?))
}

async fn fetch_total(
    client: &crate::http::Client,
    platform_api_url: &str,
    account_token: &str,
) -> anyhow::Result<BillingTotal> {
    let api = platform_api_url.trim_end_matches('/');
    let get = |route: &str| {
        client
            .get(format!("{api}{route}"))
            .bearer_auth(account_token)
            .timeout(Duration::from_secs(10))
            .send()
    };
    let (balance, estimate) = tokio::try_join!(
        async {
            Ok::<_, anyhow::Error>(
                get("/billing/balance")
                    .await?
                    .error_for_status()?
                    .json::<PlatformBalance>()
                    .await?,
            )
        },
        async {
            Ok::<_, anyhow::Error>(
                get("/billing/estimate")
                    .await?
                    .error_for_status()?
                    .json::<PlatformEstimate>()
                    .await?,
            )
        },
    )?;
    Ok(BillingTotal {
        total_cents: balance.balance_cents + estimate.total_cents,
        threshold_cents: INVOICE_THRESHOLD_CENTS,
    })
}

#[cfg(test)]
mod tests {
    use super::{console_link_url, console_url_from_api};

    mod against_the_platform {
        use super::super::{fetch_total, BillingTotal, INVOICE_THRESHOLD_CENTS};
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn platform(balance_cents: i64, month_so_far_cents: i64) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/billing/balance"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "balance_cents": balance_cents,
                    "suspended": false,
                    "has_pending_invoice": false,
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/billing/estimate"))
                .and(header("authorization", "Bearer tok"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "year_month": "2026-10",
                    "total_cents": month_so_far_cents,
                })))
                .expect(1)
                .mount(&server)
                .await;
            server
        }

        #[tokio::test]
        async fn the_total_is_every_unpaid_month_plus_this_month_so_far() {
            let server = platform(640, 180).await;
            let api = format!("{}/", server.uri());
            assert_eq!(
                fetch_total(&crate::http::Client::new(), &api, "tok")
                    .await
                    .unwrap(),
                BillingTotal {
                    total_cents: 820,
                    threshold_cents: INVOICE_THRESHOLD_CENTS,
                }
            );
        }

        #[test]
        fn the_threshold_matches_the_platform_invoice_threshold_of_ten_euros() {
            assert_eq!(INVOICE_THRESHOLD_CENTS, 1000);
        }

        #[tokio::test]
        async fn a_refused_token_is_an_error_not_a_zero_total() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            assert!(
                fetch_total(&crate::http::Client::new(), &server.uri(), "bad")
                    .await
                    .is_err()
            );
        }

        #[tokio::test]
        async fn a_missing_estimate_is_an_error_not_a_smaller_total() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/billing/balance"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "balance_cents": 640 })),
                )
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/billing/estimate"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            assert!(
                fetch_total(&crate::http::Client::new(), &server.uri(), "tok")
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn the_token_goes_in_the_fragment_never_the_query() {
        let url = console_link_url("https://console.example", "tok-123").unwrap();
        assert!(url.contains("#token=tok-123"), "{url}");
        assert!(
            !url.contains("?token="),
            "a query parameter is logged: {url}"
        );
        let (before, _) = url.split_once('#').unwrap();
        assert!(!before.contains("tok-123"), "{url}");
    }

    #[test]
    fn a_token_with_awkward_characters_is_encoded() {
        let url = console_link_url("https://console.example", "a b&c#d").unwrap();
        assert!(!url.contains("a b"), "{url}");
        assert!(
            url.matches('#').count() == 1,
            "a stray # splits the fragment: {url}"
        );
    }

    #[test]
    fn without_a_token_the_link_still_points_at_the_console() {
        assert_eq!(
            console_link_url("https://console.example", "   ").as_deref(),
            Some("https://console.example")
        );
    }

    #[test]
    fn derives_the_console_host_from_the_api_host() {
        assert_eq!(
            console_url_from_api("https://api.yolab.io").as_deref(),
            Some("https://console.yolab.io")
        );
        assert_eq!(
            console_url_from_api("https://api.yolab.io/").as_deref(),
            Some("https://console.yolab.io")
        );
    }

    #[test]
    fn a_host_that_merely_contains_api_is_not_rewritten() {
        for untouched in [
            "https://rapid.example.com",
            "https://example.com/api",
            "https://apiary.example.com",
        ] {
            assert_eq!(console_url_from_api(untouched), None, "{untouched}");
        }
    }

    #[test]
    fn anything_unrecognisable_yields_no_link() {
        for bad in ["", "   ", "not a url", "api.yolab.io", "https://api."] {
            assert_eq!(console_url_from_api(bad), None, "{bad:?}");
        }
    }
}
