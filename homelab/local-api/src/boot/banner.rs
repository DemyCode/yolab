//! Generate `/run/issue` with a QR code and management URL before tty1 shows
//! the login prompt. `agetty --issue-file` displays it.

use std::path::Path;

use anyhow::Result;

use crate::host::Host;

/// `[tunnel] dns_url` from config.toml, parsed properly rather than by
/// regex — `None` for a missing file, unreadable TOML, or an absent/non-string
/// key, all of which mean the same thing here: not configured yet.
fn read_dns_url(config_path: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    table
        .get("tunnel")?
        .get("dns_url")?
        .as_str()
        .map(str::to_string)
}

/// Pure formatting, separate from the qrencode shell-out that produces `qr`.
fn format_banner(dns_url: Option<&str>, qr: Option<&str>) -> String {
    let mut out = String::from("\n");
    match dns_url.filter(|s| !s.is_empty()) {
        Some(url) => {
            if let Some(qr) = qr {
                out.push_str(qr);
            }
            out.push_str(&format!("\n  YoLab Management: {url}\n\n"));
        }
        None => out.push_str("  YoLab — not yet configured\n\n"),
    }
    out
}

pub async fn run<H: Host>(host: &H, config_path: &str, issue_path: &Path) -> Result<()> {
    let dns_url = read_dns_url(config_path);
    let qr = match dns_url.as_deref() {
        Some(url) if !url.is_empty() => host
            .run_cmd("qrencode", &["-t", "UTF8", "-m", "1", url])
            .await
            .ok()
            .filter(|o| o.success)
            .map(|o| o.stdout),
        _ => None,
    };
    std::fs::write(issue_path, format_banner(dns_url.as_deref(), qr.as_deref()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    fn config_with(body: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[test]
    fn reads_the_configured_dns_url() {
        let (_d, path) = config_with("[tunnel]\ndns_url = \"https://example.yolab.dev\"\n");
        assert_eq!(
            read_dns_url(&path).as_deref(),
            Some("https://example.yolab.dev")
        );
    }

    #[test]
    fn is_none_when_unconfigured_missing_or_malformed() {
        assert_eq!(read_dns_url("/nonexistent/config.toml"), None);
        let (_d, empty) = config_with("[homelab]\nhostname = \"x\"\n");
        assert_eq!(read_dns_url(&empty), None);
        let (_d, bad) = config_with("not valid toml {{{");
        assert_eq!(read_dns_url(&bad), None);
    }

    #[test]
    fn format_banner_shows_not_configured_without_a_url() {
        let out = format_banner(None, None);
        assert!(out.contains("not yet configured"));
    }

    #[test]
    fn format_banner_shows_the_url_and_qr_art_when_configured() {
        let out = format_banner(Some("https://example.yolab.dev"), Some("[qr art]"));
        assert!(out.contains("[qr art]"));
        assert!(out.contains("YoLab Management: https://example.yolab.dev"));
    }

    #[test]
    fn format_banner_still_shows_the_url_if_qrencode_failed() {
        let out = format_banner(Some("https://example.yolab.dev"), None);
        assert!(out.contains("YoLab Management: https://example.yolab.dev"));
    }

    #[tokio::test]
    async fn writes_the_url_and_qr_art_when_configured() {
        let (_d, config_path) = config_with("[tunnel]\ndns_url = \"https://example.yolab.dev\"\n");
        let host = FakeHost::new().ok(
            "qrencode -t UTF8 -m 1 https://example.yolab.dev",
            "[qr art]",
        );
        let issue_dir = tempfile::tempdir().unwrap();
        let issue_path = issue_dir.path().join("issue");

        run(&host, &config_path, &issue_path).await.unwrap();

        let content = std::fs::read_to_string(&issue_path).unwrap();
        assert!(content.contains("[qr art]"));
        assert!(content.contains("YoLab Management: https://example.yolab.dev"));
    }

    #[tokio::test]
    async fn writes_not_configured_when_config_is_missing() {
        let host = FakeHost::new();
        let issue_dir = tempfile::tempdir().unwrap();
        let issue_path = issue_dir.path().join("issue");

        run(&host, "/nonexistent/config.toml", &issue_path)
            .await
            .unwrap();

        let content = std::fs::read_to_string(&issue_path).unwrap();
        assert!(content.contains("not yet configured"));
        assert!(
            host.calls().is_empty(),
            "qrencode must not run without a URL"
        );
    }
}
