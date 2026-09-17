use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Reads `[tunnel] account_token` out of a config.toml at `path`. A free
/// function — not just a `Config` method — because `storage::bootstrap`'s
/// cluster-join path needs the same value before `local-api serve` (and its
/// `Config`) exists at all; both read it through here so there is one parser
/// rather than two that could drift. Empty on any failure to read or parse —
/// callers MUST treat empty as "no valid token" and never authorize on it.
pub fn read_account_token(config_path: &str) -> String {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return String::new();
    };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else {
        return String::new();
    };
    table
        .get("tunnel")
        .and_then(|t| t.get("account_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// This machine's own files (config.toml, hardware-configuration.nix): the
/// `yolab-machine` flake input every rebuild passes. `YOLAB_MACHINE_DIR`, set by
/// `yolab.machineDir`.
pub fn machine_dir() -> PathBuf {
    std::env::var("YOLAB_MACHINE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/yolab/machine"))
}

/// Writes `content` whole to a temporary file, root-only, and renames it over
/// `path`: a crash mid-write leaves the previous content, never half of the new
/// one. For config.toml and the other files a FORCE HEAL keeps on disk.
pub fn write_private_file(path: &std::path::Path, content: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = path.parent().context("a path without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open {}", tmp.display()))?;
    file.write_all(content)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// Which flake this machine builds itself from: a flake URL (e.g.
/// `github:DemyCode/yolab`) and the branch, tag or commit within it.
///
/// Kept on disk rather than in a checkout because the node no longer keeps one —
/// `nixos-rebuild` fetches the flake itself. That also makes the source
/// runtime-editable, the same shape a community catalog takes: a URL added at
/// runtime rather than something baked into the system closure.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Channel {
    pub url: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// The upstream this fleet builds from unless a node's channel says otherwise.
/// Overridable by env so a deployment (or a test) can point at a fork without a
/// rebuild — the same reason `YOLAB_OFFICIAL_CHART_REPO` is configurable.
pub fn default_flake_url() -> String {
    std::env::var("YOLAB_FLAKE_URL").unwrap_or_else(|_| "github:DemyCode/yolab".into())
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            url: default_flake_url(),
            ref_: "main".into(),
        }
    }
}

impl Channel {
    /// The flake reference `nixos-rebuild --flake` takes: the URL, plus the ref
    /// when one is set. `github:o/r` + `main` → `github:o/r/main`.
    pub fn flake(&self) -> String {
        if self.ref_.is_empty() {
            self.url.clone()
        } else {
            format!("{}/{}", self.url.trim_end_matches('/'), self.ref_)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// This machine's own files, outside the repo: the `yolab-machine` flake
    /// input every rebuild passes (see flake.nix).
    pub machine_dir: String,
    pub config_path: String,
    pub platform: String,
    pub flake_target: String,
    pub node_ipv6: String,
    pub port: u16,
    pub rebuild_log: PathBuf,
    pub rebuild_pid: PathBuf,
    pub built_dir: PathBuf,
    pub channel_file: PathBuf,
    /// Whether the /api/terminal/exec root shell is available. Defaults to on
    /// (the UI's Terminal page relies on it); set YOLAB_TERMINAL_ENABLED=0 to
    /// disable the endpoint entirely.
    pub terminal_enabled: bool,
}

impl Config {
    pub fn from_env() -> Self {
        let built_dir = PathBuf::from("/var/lib/yolab");
        let machine_dir = machine_dir().to_string_lossy().into_owned();
        Self {
            config_path: std::env::var("YOLAB_CONFIG")
                .unwrap_or_else(|_| format!("{machine_dir}/config.toml")),
            machine_dir,
            platform: std::env::var("YOLAB_PLATFORM").unwrap_or_else(|_| "nixos".into()),
            flake_target: std::env::var("YOLAB_FLAKE_TARGET").unwrap_or_else(|_| "yolab".into()),
            node_ipv6: std::env::var("YOLAB_NODE_IPV6").unwrap_or_else(|_| "::1".into()),
            port: std::env::var("YOLAB_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3001),
            rebuild_log: PathBuf::from("/var/log/yolab-rebuild.log"),
            rebuild_pid: PathBuf::from("/run/yolab-rebuild.pid"),
            channel_file: built_dir.join("channel.json"),
            terminal_enabled: std::env::var("YOLAB_TERMINAL_ENABLED")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            built_dir,
        }
    }

    /// Where the official catalog's charts live: the synced cache, not a
    /// directory beside the source. The node reads every chart — the official
    /// catalog included — from what the marketplace pulled, so there is no
    /// bundled copy to fall back to or drift from.
    pub fn catalog_dir(&self) -> PathBuf {
        crate::charts::official_dir()
    }

    /// The channel file, or its default when unreadable. A half-written or
    /// hand-edited file falls back wholesale rather than mixing a parsed field
    /// with a defaulted one.
    pub fn channel(&self) -> Channel {
        let Some(v) = std::fs::read_to_string(&self.channel_file)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return Channel::default();
        };
        let Some(ref_) = v.get("ref").and_then(|r| r.as_str()) else {
            return Channel::default();
        };
        // `url` absent is the one lenient case: it lets a channel file written
        // before the URL moved here keep its pinned ref under the default source.
        // Present but the wrong type is not — that is a corrupt file.
        let url = match v.get("url") {
            Some(u) => match u.as_str() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Channel::default(),
            },
            None => default_flake_url(),
        };
        if ref_.is_empty() {
            return Channel::default();
        }
        Channel {
            url,
            ref_: ref_.to_string(),
        }
    }

    pub fn write_channel(&self, ch: &Channel) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.built_dir)?;
        let v = serde_json::json!({"url": ch.url, "ref": ch.ref_});
        std::fs::write(&self.channel_file, v.to_string())?;
        Ok(())
    }

    /// The flake `nixos-rebuild` builds this machine from.
    pub fn flake_ref(&self) -> String {
        self.channel().flake()
    }

    /// The shared secret used to authenticate node→node API calls.
    ///
    /// Every node in a cluster is provisioned with the same platform
    /// `account_token` (in `[tunnel]` of config.toml), so it doubles as a
    /// pre-shared key for the mesh. Returns an empty string if unreadable —
    /// callers MUST treat empty as "no valid token" and never authorize on it.
    pub fn cluster_token(&self) -> String {
        read_account_token(&self.config_path)
    }

    /// The parsed `config.toml`, or `None` when it cannot be read or parsed.
    /// Several modules used to re-read and re-parse this file themselves.
    pub fn toml(&self) -> Option<toml::Table> {
        let text = std::fs::read_to_string(&self.config_path).ok()?;
        toml::from_str(&text).ok()
    }

    /// The `[tunnel]` table, or `None` when it is missing.
    pub fn tunnel_table(&self) -> Option<toml::Table> {
        self.toml()?.get("tunnel")?.as_table().cloned()
    }

    /// A Config pointing at a throwaway `config.toml`, for tests that need to
    /// exercise password/token reads without touching the real one.
    #[cfg(test)]
    pub fn for_test(config_path: &std::path::Path) -> Self {
        Self {
            machine_dir: "/nonexistent-machine".into(),
            config_path: config_path.to_string_lossy().into_owned(),
            platform: "test".into(),
            flake_target: "yolab".into(),
            node_ipv6: "::1".into(),
            port: 3001,
            rebuild_log: PathBuf::from("/nonexistent/rebuild.log"),
            rebuild_pid: PathBuf::from("/nonexistent/rebuild.pid"),
            built_dir: PathBuf::from("/nonexistent/built"),
            channel_file: PathBuf::from("/nonexistent/channel.json"),
            terminal_enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(body: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        let cfg = Config::for_test(&path);
        (dir, cfg)
    }

    #[test]
    fn cluster_token_reads_the_account_token() {
        let (_d, cfg) = config_with("[tunnel]\naccount_token = \"tok-abc123\"\n");
        assert_eq!(cfg.cluster_token(), "tok-abc123");
    }

    /// Every "is this caller allowed?" check funnels into comparing against this
    /// string, so the failure modes all have to produce something that can never
    /// match — never a partial or defaulted value.
    #[test]
    fn cluster_token_is_empty_when_it_cannot_be_read() {
        let missing = Config::for_test(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(missing.cluster_token(), "");

        let (_d, no_section) = config_with("[homelab]\nhostname = \"yolab\"\n");
        assert_eq!(no_section.cluster_token(), "");

        let (_d, no_key) = config_with("[tunnel]\nenabled = true\n");
        assert_eq!(no_key.cluster_token(), "");

        let (_d, wrong_type) = config_with("[tunnel]\naccount_token = 42\n");
        assert_eq!(wrong_type.cluster_token(), "");

        let (_d, not_toml) = config_with("this is not valid toml {{{");
        assert_eq!(not_toml.cluster_token(), "");
    }

    #[test]
    fn catalog_dir_is_the_synced_official_cache() {
        let cfg = Config::for_test(std::path::Path::new("/tmp/config.toml"));
        assert_eq!(cfg.catalog_dir(), crate::charts::official_dir());
    }

    #[test]
    fn a_channel_composes_the_flake_from_url_and_ref() {
        let ch = Channel {
            url: "github:DemyCode/yolab".into(),
            ref_: "main".into(),
        };
        assert_eq!(ch.flake(), "github:DemyCode/yolab/main");
        // A trailing slash on the URL must not double up.
        let ch = Channel {
            url: "github:DemyCode/yolab/".into(),
            ref_: "v2.1.0".into(),
        };
        assert_eq!(ch.flake(), "github:DemyCode/yolab/v2.1.0");
    }

    /// A ref already baked into the URL (or none at all) is used as-is.
    #[test]
    fn a_channel_without_a_ref_uses_the_url_alone() {
        let ch = Channel {
            url: "github:DemyCode/yolab/main".into(),
            ref_: "".into(),
        };
        assert_eq!(ch.flake(), "github:DemyCode/yolab/main");
    }

    /// A Config whose channel file lives in a throwaway directory, so a test can
    /// write and read one without touching the real `/var/lib/yolab`.
    fn channel_cfg(dir: &tempfile::TempDir) -> Config {
        let mut cfg = Config::for_test(&dir.path().join("config.toml"));
        cfg.built_dir = dir.path().join("built");
        cfg.channel_file = cfg.built_dir.join("channel.json");
        cfg
    }

    #[test]
    fn an_absent_channel_file_reads_as_the_default_source() {
        let dir = tempfile::tempdir().unwrap();
        let ch = channel_cfg(&dir).channel();
        assert_eq!(ch.url, default_flake_url());
        assert_eq!(ch.ref_, "main");
    }

    #[test]
    fn a_written_channel_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = channel_cfg(&dir);
        let written = Channel {
            url: "github:someone/fork".into(),
            ref_: "v2.1.0".into(),
        };
        cfg.write_channel(&written).unwrap();
        assert_eq!(cfg.channel(), written);
        assert_eq!(cfg.flake_ref(), "github:someone/fork/v2.1.0");
    }

    /// A half-written or hand-edited file must fall back wholesale rather than
    /// mix a parsed URL with a defaulted ref — that combination points at a ref
    /// that may not exist in that source.
    #[test]
    fn a_malformed_channel_file_falls_back_completely() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = channel_cfg(&dir);
        std::fs::create_dir_all(&cfg.built_dir).unwrap();

        for body in [
            "",
            "not json at all",
            r#"{"url": "github:o/r"}"#,    // ref missing
            r#"{"ref": "v2"}"#,            // url missing → default url, ref kept
            r#"{"url": 5, "ref": "v2"}"#,  // wrong type
            r#"{"url": "", "ref": "v2"}"#, // empty url
            r#"{"url": "github:o/r", "ref": 5}"#,
            r#"{"url": "github:o/r", "ref": ""}"#,
            "[]",
        ] {
            std::fs::write(&cfg.channel_file, body).unwrap();
            let ch = cfg.channel();
            if body == r#"{"ref": "v2"}"# {
                // The legacy shape: no url, but a ref worth keeping.
                assert_eq!(
                    (ch.url.as_str(), ch.ref_.as_str()),
                    (default_flake_url().as_str(), "v2"),
                    "body: {body}"
                );
                continue;
            }
            assert_eq!(
                (ch.url.as_str(), ch.ref_.as_str()),
                (default_flake_url().as_str(), "main"),
                "body: {body}"
            );
        }
    }
}
