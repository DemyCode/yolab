use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub repo_path: String,
    pub config_path: String,
    pub platform: String,
    pub flake_target: String,
    pub node_ipv6: String,
    pub hostname: String,
    pub port: u16,
    pub rebuild_log: PathBuf,
    pub rebuild_pid: PathBuf,
    pub built_dir: PathBuf,
    pub osd_img_path: PathBuf,
    pub channel_file: PathBuf,
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
            hostname: std::env::var("YOLAB_HOSTNAME")
                .unwrap_or_else(|_| hostname::get().unwrap_or_else(|_| "localhost".into()).to_string_lossy().to_string()),
            port: std::env::var("YOLAB_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3001),
            rebuild_log: PathBuf::from("/var/log/yolab-rebuild.log"),
            rebuild_pid: PathBuf::from("/run/yolab-rebuild.pid"),
            channel_file: built_dir.join("channel.json"),
            osd_img_path: PathBuf::from("/var/lib/rook/system-osd.img"),
            built_dir,
            repo_path,
        }
    }

    pub fn catalog_dir(&self) -> PathBuf {
        PathBuf::from(&self.repo_path).join("apps/catalog")
    }


}
