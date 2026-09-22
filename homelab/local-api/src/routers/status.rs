use axum::{extract::State, Json};
use serde::Serialize;

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
        .ok_or_else(|| anyhow::anyhow!("no console URL for this box"))?;

    let token = crate::config::read_account_token(&state.config.config_path);
    let url = console_link_url(&console, &token)
        .ok_or_else(|| anyhow::anyhow!("could not build the console link"))?;

    Ok(Json(ConsoleLink { url }))
}

#[cfg(test)]
mod tests {
    use super::{console_link_url, console_url_from_api};

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
