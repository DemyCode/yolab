use axum::{extract::State, Json};
use serde::Serialize;

use crate::{error::Result, AppState};

#[derive(Serialize)]
pub struct StatusInfo {
    pub commit_hash: String,
    pub commit_message: String,
    pub commit_date: String,
    pub platform: String,
    pub flake_target: String,
    /// Where this box's account and billing live, for the Settings link.
    ///
    /// Absent rather than empty when it cannot be worked out, so the UI omits
    /// the row entirely. A button that goes nowhere is worse than no button:
    /// it teaches the reader that the settings page lies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The account console that belongs to a given platform API.
///
/// Derived rather than configured, because every install already has
/// `tunnel.platform_api_url` and none has a console setting — asking owners to
/// add one by hand to get a link back would mean nobody has the link. The two
/// hosts are the same deployment under different subdomains
/// (`api.demycode.ovh` -> `console.demycode.ovh`), so the API URL already
/// carries the answer.
///
/// Conservative on purpose: anything that is not recognisably an `api.` host
/// yields None and the row is simply not shown, rather than sending someone to
/// a hostname invented from a pattern that did not hold.
pub(crate) fn console_url_from_api(api_url: &str) -> Option<String> {
    let api_url = api_url.trim().trim_end_matches('/');
    if api_url.is_empty() {
        return None;
    }
    let (scheme, host) = api_url.split_once("://")?;
    // Only the leading label is replaced. A host that merely contains "api"
    // somewhere ("rapid.example") must not be rewritten.
    let rest = host.strip_prefix("api.")?;
    if rest.is_empty() {
        return None;
    }
    Some(format!("{scheme}://console.{rest}"))
}

fn built_or_git(state: &AppState, filename: &str, args: &[&str]) -> String {
    let v = std::fs::read_to_string(state.config.built_dir.join(filename))
        .unwrap_or_default()
        .trim()
        .to_string();
    if !v.is_empty() {
        return v;
    }
    std::process::Command::new("git")
        .args(args)
        .current_dir(&state.config.repo_path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub async fn handler(State(state): State<AppState>) -> Result<Json<StatusInfo>> {
    Ok(Json(StatusInfo {
        commit_hash: built_or_git(&state, "built-hash", &["rev-parse", "HEAD"]),
        commit_message: built_or_git(&state, "built-message", &["log", "-1", "--pretty=%s"]),
        commit_date: built_or_git(&state, "built-date", &["log", "-1", "--pretty=%cI"]),
        platform: state.config.platform.clone(),
        flake_target: state.config.flake_target.clone(),
        // Read here rather than held on `Config` because this is the only
        // consumer, and reading the file keeps it correct after an owner edits
        // config.toml without restarting the service.
        console_url: platform_api_url(&state.config.config_path)
            .as_deref()
            .and_then(console_url_from_api),
        error: None,
    }))
}

/// `[tunnel] platform_api_url`, or None if the file is missing or malformed.
/// Never an error: a box with no tunnel config still has a Settings page, it
/// simply has no console to link to.
fn platform_api_url(config_path: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let table = toml::from_str::<toml::Table>(&text).ok()?;
    Some(
        table
            .get("tunnel")?
            .get("platform_api_url")?
            .as_str()?
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::console_url_from_api;

    #[test]
    fn derives_the_console_host_from_the_api_host() {
        assert_eq!(
            console_url_from_api("https://api.demycode.ovh").as_deref(),
            Some("https://console.demycode.ovh")
        );
        // A trailing slash is normal in config and must not become a double one.
        assert_eq!(
            console_url_from_api("https://api.demycode.ovh/").as_deref(),
            Some("https://console.demycode.ovh")
        );
    }

    /// Only the leading label is replaced. A host that merely CONTAINS "api"
    /// must be left alone rather than rewritten into a hostname that does not
    /// exist — the link would 404 and teach the reader the page lies.
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

    /// Absent beats guessing. Every one of these yields no row at all rather
    /// than a button that goes nowhere.
    #[test]
    fn anything_unrecognisable_yields_no_link() {
        for bad in ["", "   ", "not a url", "api.demycode.ovh", "https://api."] {
            assert_eq!(console_url_from_api(bad), None, "{bad:?}");
        }
    }
}
