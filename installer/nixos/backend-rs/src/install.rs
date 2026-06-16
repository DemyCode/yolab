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
    pub hostname: String,
    pub timezone: String,
    pub password: String,
    pub root_ssh_key: String,
    pub account_token: String,
    pub server_addr: Option<String>,
    pub k3s_token: Option<String>,
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
    node_id: String,
    wg_private_key: String,
    wg_public_key: String,
    sub_ipv6: String,
    sub_ipv6_private: String,
    sub_ipv6_private_subnet: String,
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
    k3s: K3sSection,
}

#[derive(Serialize)]
struct K3sSection {
    token: String,
    server_addr: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn gen_k3s_token() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..32).map(|_| rand::thread_rng().gen()).collect();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    log!("Node will be registered as: {service_name}");

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

    let tunnel_section = TunnelSection {
        enabled: tunnel.enabled,
        platform_api_url: tunnel.platform_api_url.clone(),
        account_token: tunnel.account_token.clone(),
        tunnel_id: tunnel.tunnel_id.clone(),
        node_id: tunnel.node_id.clone(),
        wg_private_key: tunnel.wg_private_key.clone(),
        wg_public_key: tunnel.wg_public_key.clone(),
        sub_ipv6: tunnel.sub_ipv6.clone(),
        sub_ipv6_private: tunnel.sub_ipv6_private.clone(),
        sub_ipv6_private_subnet: tunnel.sub_ipv6_private_subnet.clone(),
        dns_url: tunnel.dns_url.clone(),
        wg_server_endpoint: tunnel.wg_server_endpoint.clone(),
        wg_server_public_key: tunnel.wg_server_public_key.clone(),
    };

    let config = ConfigToml {
        homelab: HomelabSection {
            hostname: req.hostname.clone(),
            timezone: req.timezone.clone(),
            locale: "en_US.UTF-8".into(),
            ssh_port: 22,
            root_ssh_key: req.root_ssh_key.clone(),
            git_remote: GIT_REMOTE.into(),
            allowed_ssh_keys: vec![],
            homelab_password_hash: password_hash,
            boot_mode: req.boot_mode.clone(),
        },
        disk: DiskSection {
            device: req.disk.clone(),
            esp_size: "500M".into(),
        },
        tunnel: Some(tunnel_section),
        swarm: SwarmSection { enabled: false },
        node: NodeSection {
            node_id: uuid::Uuid::new_v4().to_string(),
            k3s: K3sSection {
                token: req.k3s_token.clone().unwrap_or_else(gen_k3s_token),
                server_addr: req.server_addr.clone().unwrap_or_default(),
            },
        },
    };

    let toml_str = toml::to_string(&config).context("serialize config")?;
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
            "-v",
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
