use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub repo_path: String,
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
        let repo_path = std::env::var("YOLAB_REPO_PATH").unwrap_or_else(|_| "/etc/nixos".into());
        let built_dir = PathBuf::from("/var/lib/yolab");
        Self {
            config_path: std::env::var("YOLAB_CONFIG")
                .unwrap_or_else(|_| format!("{repo_path}/homelab/ignored/config.toml")),
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
            repo_path,
        }
    }

    pub fn catalog_dir(&self) -> PathBuf {
        PathBuf::from(&self.repo_path).join("apps/catalog")
    }

    /// The shared secret used to authenticate node→node API calls.
    ///
    /// Every node in a cluster is provisioned with the same platform
    /// `account_token` (in `[tunnel]` of config.toml), so it doubles as a
    /// pre-shared key for the mesh. Returns an empty string if unreadable —
    /// callers MUST treat empty as "no valid token" and never authorize on it.
    pub fn cluster_token(&self) -> String {
        let Ok(text) = std::fs::read_to_string(&self.config_path) else {
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

    /// A Config pointing at a throwaway `config.toml`, for tests that need to
    /// exercise password/token reads without touching the real one.
    #[cfg(test)]
    pub fn for_test(config_path: &std::path::Path) -> Self {
        Self {
            repo_path: "/nonexistent-repo".into(),
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
    fn catalog_dir_hangs_off_the_repo_path() {
        let cfg = Config::for_test(std::path::Path::new("/tmp/config.toml"));
        assert_eq!(
            cfg.catalog_dir(),
            PathBuf::from("/nonexistent-repo/apps/catalog")
        );
    }
}
