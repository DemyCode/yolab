use std::path::Path;
use std::process::Stdio;

use anyhow::Context;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::app::AppEvent;

const GIT_REMOTE: &str = "https://github.com/DemyCode/yolab.git";
const CODE_DIR: &str = "/tmp/yolab-install";

pub struct InstallParams {
    pub disk: String,
    pub timezone: String,
    pub password: String,
    pub root_ssh_key: String,
    pub account_token: String,
    pub server_addr: Option<String>,
    pub k3s_token: Option<String>,
    /// Ceph cluster fsid. Like `k3s_token`, the first node generates one and a
    /// joining node must be given the existing cluster's value — every node in
    /// a Ceph cluster shares a single fsid, and a node built with the wrong one
    /// cannot talk to the mons at all.
    pub ceph_fsid: Option<String>,
    pub boot_mode: String, // "uefi" or "bios"
}

// ── TOML config types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ConfigToml {
    homelab: HomelabSection,
    disk: DiskSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel: Option<TunnelSection>,
    swarm: SwarmSection,
    node: NodeSection,
    ceph: CephSection,
}

#[derive(Serialize)]
struct HomelabSection {
    hostname: String,
    timezone: String,
    locale: String,
    ssh_port: u16,
    root_ssh_key: String,
    git_remote: String,
    allowed_ssh_keys: Vec<String>,
    homelab_password_hash: String,
    boot_mode: String,
}

#[derive(Serialize)]
struct DiskSection {
    device: String,
    esp_size: String,
}

#[derive(Serialize)]
struct TunnelSection {
    enabled: bool,
    platform_api_url: String,
    account_token: String,
    tunnel_id: String,
    wg_private_key: String,
    wg_public_key: String,
    sub_ipv6: String,
    dns_url: String,
    wg_server_endpoint: String,
    wg_server_public_key: String,
}

#[derive(Serialize)]
struct SwarmSection {
    enabled: bool,
}

#[derive(Serialize)]
struct NodeSection {
    node_id: String,
    wg_private_key: String,
    wg_public_key: String,
    sub_ipv6_private: String,
    sub_ipv6_private_subnet: String,
    wg_server_endpoint: String,
    wg_server_public_key: String,
    k3s: K3sSection,
}

#[derive(Serialize)]
struct K3sSection {
    token: String,
    server_addr: String,
}

/// Ceph now runs as host daemons rather than Rook pods, so the cluster identity
/// has to exist before the first `nixos-rebuild` — the NixOS config throws
/// without it. It is written once at install time and must never change
/// afterwards: the fsid is baked into every OSD's BlueStore superblock, so a
/// node that rebuilds with a different one no longer recognises its own disks.
#[derive(Serialize)]
struct CephSection {
    fsid: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// A random RFC-4122 v4 UUID, the form Ceph requires for an fsid.
fn gen_ceph_fsid() -> String {
    use rand::Rng;
    let mut b: [u8; 16] = rand::thread_rng().gen();
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

fn gen_k3s_token() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..32).map(|_| rand::thread_rng().gen()).collect();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Assembles the `config.toml` this machine will be built from.
///
/// Split out of `do_install` because it is the installer's most consequential
/// artifact — every NixOS rebuild reads it, local-api reads the password hash
/// out of it to authenticate the owner, and the cluster reads the account token
/// out of it to authenticate node-to-node calls. A missing or misplaced field
/// here is not a failed install, it is a machine that boots into the wrong
/// configuration.
fn build_config(
    req: &InstallParams,
    tunnel: &crate::wireguard::TunnelResult,
    service_name: &str,
    password_hash: &str,
) -> ConfigToml {
    ConfigToml {
        homelab: HomelabSection {
            hostname: service_name.to_string(),
            timezone: req.timezone.clone(),
            locale: "en_US.UTF-8".into(),
            ssh_port: 22,
            root_ssh_key: req.root_ssh_key.clone(),
            git_remote: GIT_REMOTE.into(),
            allowed_ssh_keys: vec![],
            homelab_password_hash: password_hash.to_string(),
            boot_mode: req.boot_mode.clone(),
        },
        disk: DiskSection {
            device: req.disk.clone(),
            esp_size: "500M".into(),
        },
        tunnel: Some(TunnelSection {
            enabled: tunnel.enabled,
            platform_api_url: tunnel.platform_api_url.clone(),
            account_token: tunnel.account_token.clone(),
            tunnel_id: tunnel.tunnel_id.clone(),
            wg_private_key: tunnel.wg_private_key.clone(),
            wg_public_key: tunnel.wg_public_key.clone(),
            sub_ipv6: tunnel.sub_ipv6.clone(),
            dns_url: tunnel.dns_url.clone(),
            wg_server_endpoint: tunnel.wg_server_endpoint.clone(),
            wg_server_public_key: tunnel.wg_server_public_key.clone(),
        }),
        swarm: SwarmSection { enabled: false },
        node: NodeSection {
            node_id: tunnel.node_id.clone(),
            wg_private_key: tunnel.node_wg_private_key.clone(),
            wg_public_key: tunnel.node_wg_public_key.clone(),
            sub_ipv6_private: tunnel.sub_ipv6_private.clone(),
            sub_ipv6_private_subnet: tunnel.sub_ipv6_private_subnet.clone(),
            wg_server_endpoint: tunnel.node_wg_server_endpoint.clone(),
            wg_server_public_key: tunnel.node_wg_server_public_key.clone(),
            k3s: K3sSection {
                // A joining node is given the existing cluster's token; the first
                // node generates one.
                token: req.k3s_token.clone().unwrap_or_else(gen_k3s_token),
                server_addr: req.server_addr.clone().unwrap_or_default(),
            },
        },
        ceph: CephSection {
            // Same rule as the k3s token: generated by the first node, supplied
            // by the caller for a node joining an existing cluster.
            fsid: req.ceph_fsid.clone().unwrap_or_else(gen_ceph_fsid),
        },
    }
}

fn render_config_toml(
    req: &InstallParams,
    tunnel: &crate::wireguard::TunnelResult,
    service_name: &str,
    password_hash: &str,
) -> anyhow::Result<String> {
    Ok(toml::to_string(&build_config(req, tunnel, service_name, password_hash))?)
}

async fn hash_password(password: &str) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("openssl passwd")?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(password.as_bytes()).await?;
    }

    let out = child.wait_with_output().await?;
    anyhow::ensure!(out.status.success(), "openssl passwd failed");
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

async fn stream_command(
    program: &str,
    args: &[&str],
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> anyhow::Result<()> {
    let _ = tx.send(AppEvent::Log(format!("$ {program} {}", args.join(" "))));

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tx1 = tx.clone();
    let tx2 = tx.clone();

    let t1 = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx1.send(AppEvent::Log(line));
        }
    });
    let t2 = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx2.send(AppEvent::Log(line));
        }
    });

    let _ = tokio::join!(t1, t2);
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "{program} failed with {status}");
    Ok(())
}

// Runs a command, streams its stderr to the log, and returns captured stdout.
async fn capture_command(
    program: &str,
    args: &[&str],
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> anyhow::Result<Vec<u8>> {
    let _ = tx.send(AppEvent::Log(format!("$ {program} {}", args.join(" "))));

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;

    let mut stdout_handle = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tx2 = tx.clone();

    let t_stderr = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx2.send(AppEvent::Log(line));
        }
    });

    let mut stdout_bytes = Vec::new();
    stdout_handle.read_to_end(&mut stdout_bytes).await?;
    let _ = t_stderr.await;

    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "{program} failed with {status}");
    Ok(stdout_bytes)
}

// ── Main install runner ───────────────────────────────────────────────────────

pub async fn run_install(req: InstallParams, tx: mpsc::UnboundedSender<AppEvent>) {
    match do_install(&req, &tx).await {
        Ok(url) => {
            let _ = tx.send(AppEvent::InstallComplete { url });
        }
        Err(e) => {
            let _ = tx.send(AppEvent::Log(format!("ERROR: {e:#}")));
            let _ = tx.send(AppEvent::Failed(format!("{e:#}")));
        }
    }
}

async fn do_install(
    req: &InstallParams,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> anyhow::Result<String> {
    macro_rules! log {
        ($($arg:tt)*) => { let _ = tx.send(AppEvent::Log(format!($($arg)*))); };
    }

    // ── Register WireGuard tunnel ─────────────────────────────────────────────
    log!("Registering WireGuard tunnel…");
    let service_name = crate::wireguard::next_node_name(&req.account_token)
        .await
        .context("next_node_name")?;
    // service_name (e.g. "node1", "node2") is used as the hostname — unique by construction.
    log!("This machine will be: {service_name}");

    let tunnel = crate::wireguard::register_and_bring_up_tunnel(&req.account_token, &service_name)
        .await
        .context("register tunnel")?;
    log!("✓ Tunnel up — {}", tunnel.dns_url);

    // ── Hash password ─────────────────────────────────────────────────────────
    log!("Hashing password…");
    let password_hash = hash_password(&req.password).await.context("hash_password")?;

    // ── Clone repository ──────────────────────────────────────────────────────
    if Path::new(CODE_DIR).exists() {
        tokio::fs::remove_dir_all(CODE_DIR).await?;
    }
    log!("Cloning repository…");
    stream_command("git", &["clone", GIT_REMOTE, CODE_DIR], tx).await?;
    log!("✓ Repository cloned");

    // ── Write config.toml ─────────────────────────────────────────────────────
    log!("Writing config.toml…");
    let ignored_dir = format!("{CODE_DIR}/homelab/ignored");
    tokio::fs::create_dir_all(&ignored_dir).await?;

    let toml_str = render_config_toml(req, &tunnel, &service_name, &password_hash)
        .context("serialize config")?;
    tokio::fs::write(format!("{ignored_dir}/config.toml"), toml_str).await?;
    log!("✓ Config written");

    // ── Generate hardware config ──────────────────────────────────────────────
    log!("Generating hardware configuration…");
    let hw_nix = capture_command(
        "nixos-generate-config",
        &["--no-filesystems", "--show-hardware-config"],
        tx,
    )
    .await
    .context("nixos-generate-config")?;
    tokio::fs::write(format!("{ignored_dir}/hardware-configuration.nix"), hw_nix).await?;
    log!("✓ Hardware config generated");

    // ── Partition disk ────────────────────────────────────────────────────────
    log!("Partitioning {} with disko…", req.disk);
    let disk_config = format!("{CODE_DIR}/homelab/nixos/disk-config.nix");
    stream_command(
        "disko",
        &[
            "--yes-wipe-all-disks",
            "--mode",
            "destroy,format,mount",
            &disk_config,
        ],
        tx,
    )
    .await?;
    log!("✓ Disk partitioned and mounted");

    // ── Install NixOS ─────────────────────────────────────────────────────────
    log!("Installing NixOS — this takes several minutes…");
    let flake_ref = format!("path:{CODE_DIR}#yolab");
    stream_command(
        "nixos-install",
        &[
            "--flake", &flake_ref,
            "--no-root-password",
            "--log-format", "raw",
            "-v"
        ],
        tx,
    )
    .await?;
    log!("✓ NixOS installed");

    // ── Copy repo to installed system ─────────────────────────────────────────
    log!("Copying repository to installed system…");
    let src = format!("{CODE_DIR}/");
    stream_command("rsync", &["-a", &src, "/mnt/etc/nixos"], tx).await?;
    log!("✓ Complete — remove the USB and reboot");

    Ok(tunnel.dns_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireguard::TunnelResult;

    fn tunnel() -> TunnelResult {
        TunnelResult {
            enabled: true,
            platform_api_url: "https://api.demycode.ovh".into(),
            account_token: "acct-tok-123".into(),
            tunnel_id: "42".into(),
            wg_private_key: "tunnel-priv".into(),
            wg_public_key: "tunnel-pub".into(),
            sub_ipv6: "2001:db8::1".into(),
            dns_url: "https://node1.demycode.ovh".into(),
            wg_server_endpoint: "1.2.3.4:51820".into(),
            wg_server_public_key: "server-pub".into(),
            node_id: "7".into(),
            node_wg_private_key: "node-priv".into(),
            node_wg_public_key: "node-pub".into(),
            sub_ipv6_private: "fd00:42::5".into(),
            sub_ipv6_private_subnet: "fd00:42::/112".into(),
            node_wg_server_endpoint: "1.2.3.4:51821".into(),
            node_wg_server_public_key: "node-server-pub".into(),
        }
    }

    fn params() -> InstallParams {
        InstallParams {
            disk: "/dev/nvme0n1".into(),
            timezone: "Europe/Paris".into(),
            password: "secret".into(),
            root_ssh_key: "ssh-ed25519 AAAA... user@host".into(),
            account_token: "acct-tok-123".into(),
            server_addr: None,
            k3s_token: None,
            ceph_fsid: None,
            boot_mode: "uefi".into(),
        }
    }

    /// Parses the rendered TOML back, which is what NixOS and local-api actually do.
    fn rendered(req: &InstallParams) -> toml::Table {
        let text = render_config_toml(req, &tunnel(), "node1", "$6$salt$hash").unwrap();
        toml::from_str(&text).expect("installer must emit parseable TOML")
    }

    // ── gen_k3s_token ─────────────────────────────────────────────────────────

    #[test]
    fn a_generated_k3s_token_is_256_bits_of_hex() {
        let t = gen_k3s_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// This token is the cluster's join credential. A constant or predictable
    /// value would let anyone who can reach the API join the cluster.
    #[test]
    fn generated_k3s_tokens_differ() {
        assert_ne!(gen_k3s_token(), gen_k3s_token());
    }

    // ── gen_ceph_fsid ─────────────────────────────────────────────────────────

    /// Ceph parses the fsid strictly and refuses to start on a malformed one,
    /// and disks_reconciler compares it byte-for-byte against the UUID in each
    /// disk's BlueStore superblock to tell our disks from another cluster's.
    #[test]
    fn a_generated_ceph_fsid_is_a_v4_uuid() {
        let f = gen_ceph_fsid();
        let parts: Vec<&str> = f.split('-').collect();
        assert_eq!(parts.len(), 5, "{f}");
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{f}"
        );
        assert!(parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit())), "{f}");
        assert!(parts[2].starts_with('4'), "version nibble must be 4: {f}");
        assert!(
            ['8', '9', 'a', 'b'].contains(&parts[3].chars().next().unwrap()),
            "RFC 4122 variant nibble must be 8/9/a/b: {f}"
        );
    }

    /// Two clusters sharing an fsid would each treat the other's OSDs as their
    /// own — the exact condition `classify` refuses to act on.
    #[test]
    fn generated_ceph_fsids_differ() {
        assert_ne!(gen_ceph_fsid(), gen_ceph_fsid());
    }

    /// The NixOS config throws without this key, so a missing section is not a
    /// degraded install — it is a machine that cannot rebuild at all.
    #[test]
    fn config_toml_carries_a_ceph_fsid() {
        let t = rendered(&params());
        let fsid = t["ceph"]["fsid"].as_str().expect("[ceph] fsid must be written");
        assert!(!fsid.is_empty());
    }

    // ── Config rendering ──────────────────────────────────────────────────────

    #[test]
    fn the_rendered_config_has_every_section_the_nixos_modules_read() {
        let cfg = rendered(&params());
        for section in ["homelab", "disk", "tunnel", "swarm", "node"] {
            assert!(cfg.contains_key(section), "missing [{section}]");
        }
        assert!(cfg["node"].as_table().unwrap().contains_key("k3s"));
    }

    #[test]
    fn the_hostname_is_the_name_assigned_by_the_platform() {
        // Not the disk, not a constant: it must match the DNS record just created.
        assert_eq!(rendered(&params())["homelab"]["hostname"].as_str(), Some("node1"));
    }

    #[test]
    fn the_users_choices_reach_the_config_verbatim() {
        let cfg = rendered(&params());
        assert_eq!(cfg["homelab"]["timezone"].as_str(), Some("Europe/Paris"));
        assert_eq!(cfg["homelab"]["boot_mode"].as_str(), Some("uefi"));
        assert_eq!(cfg["homelab"]["root_ssh_key"].as_str(), Some("ssh-ed25519 AAAA... user@host"));
        assert_eq!(cfg["disk"]["device"].as_str(), Some("/dev/nvme0n1"));
    }

    /// local-api reads this hash to authenticate the owner. The plaintext must
    /// never appear anywhere in the file.
    #[test]
    fn only_the_password_hash_is_written_never_the_password() {
        let text = render_config_toml(&params(), &tunnel(), "node1", "$6$salt$hash").unwrap();
        assert!(text.contains("$6$salt$hash"));
        assert!(
            !text.contains("secret"),
            "the plaintext password must not be written to config.toml"
        );
    }

    /// `Config::cluster_token()` in local-api reads exactly this path to
    /// authenticate node-to-node calls; if it moves, inter-node auth silently
    /// fails closed.
    #[test]
    fn the_account_token_lands_where_local_api_looks_for_it() {
        let cfg = rendered(&params());
        assert_eq!(cfg["tunnel"]["account_token"].as_str(), Some("acct-tok-123"));
    }

    #[test]
    fn both_wireguard_keypairs_are_recorded_without_being_swapped() {
        let cfg = rendered(&params());
        // wg0 — the public tunnel.
        assert_eq!(cfg["tunnel"]["wg_private_key"].as_str(), Some("tunnel-priv"));
        assert_eq!(cfg["tunnel"]["wg_public_key"].as_str(), Some("tunnel-pub"));
        assert_eq!(cfg["tunnel"]["sub_ipv6"].as_str(), Some("2001:db8::1"));
        // wg1 — the private node mesh. Distinct keys, distinct endpoint.
        assert_eq!(cfg["node"]["wg_private_key"].as_str(), Some("node-priv"));
        assert_eq!(cfg["node"]["wg_public_key"].as_str(), Some("node-pub"));
        assert_eq!(cfg["node"]["sub_ipv6_private"].as_str(), Some("fd00:42::5"));
        assert_eq!(cfg["node"]["wg_server_endpoint"].as_str(), Some("1.2.3.4:51821"));
    }

    // ── First node vs joining node ────────────────────────────────────────────

    /// The first machine has no cluster to join: it invents a token and leaves
    /// server_addr empty, which is what tells the NixOS module to run k3s with
    /// --cluster-init.
    #[test]
    fn the_first_node_generates_its_own_k3s_token_and_no_server_address() {
        let cfg = rendered(&params());
        let k3s = &cfg["node"]["k3s"];
        assert_eq!(k3s["server_addr"].as_str(), Some(""));
        assert_eq!(k3s["token"].as_str().unwrap().len(), 64);
    }

    /// A joining node must reuse the existing cluster's token exactly — generating
    /// a fresh one is an unrecoverable join failure.
    #[test]
    fn a_joining_node_keeps_the_token_it_was_given() {
        let mut req = params();
        req.k3s_token = Some("existing-cluster-token".into());
        req.server_addr = Some("https://[fd00:42::1]:6443".into());

        let cfg = rendered(&req);
        let k3s = &cfg["node"]["k3s"];
        assert_eq!(k3s["token"].as_str(), Some("existing-cluster-token"));
        assert_eq!(k3s["server_addr"].as_str(), Some("https://[fd00:42::1]:6443"));
    }

    #[test]
    fn two_installs_of_the_first_node_never_share_a_k3s_token() {
        let a = rendered(&params());
        let b = rendered(&params());
        assert_ne!(
            a["node"]["k3s"]["token"].as_str(),
            b["node"]["k3s"]["token"].as_str()
        );
    }

    // ── Defaults and edge cases ───────────────────────────────────────────────

    #[test]
    fn defaults_that_the_ui_never_asks_about_are_still_set() {
        let cfg = rendered(&params());
        assert_eq!(cfg["homelab"]["locale"].as_str(), Some("en_US.UTF-8"));
        assert_eq!(cfg["homelab"]["ssh_port"].as_integer(), Some(22));
        assert_eq!(cfg["disk"]["esp_size"].as_str(), Some("500M"));
        assert_eq!(cfg["swarm"]["enabled"].as_bool(), Some(false));
        assert_eq!(cfg["tunnel"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn a_bios_install_records_bios_not_the_uefi_default() {
        let mut req = params();
        req.boot_mode = "bios".into();
        assert_eq!(rendered(&req)["homelab"]["boot_mode"].as_str(), Some("bios"));
    }

    /// Skipping the SSH key is allowed; it must render as an empty string rather
    /// than dropping the key and leaving the NixOS module reading a missing field.
    #[test]
    fn an_empty_ssh_key_is_still_written_as_a_field() {
        let mut req = params();
        req.root_ssh_key = String::new();
        let cfg = rendered(&req);
        assert_eq!(cfg["homelab"]["root_ssh_key"].as_str(), Some(""));
        assert!(cfg["homelab"]["allowed_ssh_keys"].as_array().unwrap().is_empty());
    }

    /// TOML has no escape for a raw newline in a basic string. If a pasted SSH key
    /// or timezone carried one through unescaped, the file would fail to parse and
    /// the machine would not build — so the round-trip has to hold.
    #[test]
    fn values_containing_awkward_characters_survive_the_round_trip() {
        let mut req = params();
        req.root_ssh_key = "ssh-ed25519 AAAA\nsecond line \"quoted\" \\ backslash".into();
        req.timezone = "America/Argentina/Buenos_Aires".into();

        let cfg = rendered(&req);
        assert_eq!(
            cfg["homelab"]["root_ssh_key"].as_str(),
            Some("ssh-ed25519 AAAA\nsecond line \"quoted\" \\ backslash")
        );
        assert_eq!(cfg["homelab"]["timezone"].as_str(), Some("America/Argentina/Buenos_Aires"));
    }
}
