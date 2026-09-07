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

#[derive(Serialize)]
pub struct ConsoleLink {
    pub url: String,
}

/// Builds the console URL with the account token in the FRAGMENT.
///
/// The fragment is the entire point, and it is not interchangeable with a query
/// parameter. A `?token=` is sent to the server on every request, so it lands in
/// the console's access logs, in any proxy or CDN in between, and in the
/// `Referer` header of the next link the reader clicks from that page. A `#`
/// fragment is never transmitted: the browser keeps it client-side, the
/// console's own script reads `location.hash`, uses it, and clears it.
///
/// This does not make the token harmless — it still reaches the browser and its
/// session history — so the URL is built only when someone deliberately clicks
/// through, never rendered into the page ahead of time.
pub(crate) fn console_link_url(console: &str, token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        // No token is not an error: the console still exists, it will simply
        // ask for a login. A link that goes to the right place unauthenticated
        // beats no link at all.
        return Some(console.to_string());
    }
    Some(format!("{console}/#token={}", percent_encode(token)))
}

/// Percent-encodes everything outside RFC 3986's unreserved set.
///
/// Written here rather than pulled in as a crate: this is the only caller in the
/// tree, and a new dependency costs a lockfile change and a nix vendor hash for
/// ten lines. Deliberately strict — a token is opaque, and a stray `#` in one
/// would end the fragment early and silently truncate the credential the console
/// receives.
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

/// Fetched on click, not included in `/api/status`.
///
/// `/api/status` is polled continuously by every open tab, and putting a
/// credential in a response on that path would push it through the browser
/// cache and any logging on the way, thousands of times a day, for a link
/// almost nobody clicks. Here it leaves the box once per deliberate action.
pub async fn console_link(State(state): State<AppState>) -> Result<Json<ConsoleLink>> {
    let console = platform_api_url(&state.config.config_path)
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

    /// The token must land in the fragment. A query parameter would put an
    /// account-wide credential into the console's access logs and into the
    /// Referer of every outbound link on the page it lands on.
    #[test]
    fn the_token_goes_in_the_fragment_never_the_query() {
        let url = console_link_url("https://console.example", "tok-123").unwrap();
        assert!(url.contains("#token=tok-123"), "{url}");
        assert!(
            !url.contains("?token="),
            "a query parameter is logged: {url}"
        );
        // Nothing before the '#' may carry it.
        let (before, _) = url.split_once('#').unwrap();
        assert!(!before.contains("tok-123"), "{url}");
    }

    /// Tokens are opaque and may contain characters that would otherwise end
    /// the fragment or be misread as another parameter.
    #[test]
    fn a_token_with_awkward_characters_is_encoded() {
        let url = console_link_url("https://console.example", "a b&c#d").unwrap();
        assert!(!url.contains("a b"), "{url}");
        assert!(
            url.matches('#').count() == 1,
            "a stray # splits the fragment: {url}"
        );
    }

    /// No token still yields the plain console link. The reader gets sent to
    /// the right place and signs in there, which is strictly better than the
    /// row disappearing because a credential was missing.
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
