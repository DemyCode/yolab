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
        let repo_path = std::env::var("YOLAB_REPO_PATH")
            .unwrap_or_else(|_| "/etc/nixos".into());
        let built_dir = PathBuf::from("/var/lib/yolab");
        Self {
            config_path: std::env::var("YOLAB_CONFIG")
                .unwrap_or_else(|_| format!("{repo_path}/homelab/ignored/config.toml")),
            platform: std::env::var("YOLAB_PLATFORM")
                .unwrap_or_else(|_| "nixos".into()),
            flake_target: std::env::var("YOLAB_FLAKE_TARGET")
                .unwrap_or_else(|_| "yolab".into()),
            node_ipv6: std::env::var("YOLAB_NODE_IPV6")
                .unwrap_or_else(|_| "::1".into()),
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
}
