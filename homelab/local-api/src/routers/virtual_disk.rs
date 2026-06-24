use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

use crate::config::Config;
use crate::routers::backups::ye_creds;

// ── API types ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct VolumeInfo {
    id: i32,
    host: String,
    port: i32,
    username: String,
    password: String,
    size_gb: i32,
    node_id: Option<i32>,
}

// ── Persisted state ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct DiskState {
    disks: Vec<AttachedDisk>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AttachedDisk {
    volume_id: i32,
    host: String,
    port: i32,
    username: String,
    password: String,
    size_gb: i32,
    mount_point: String,
    loop_device: String,
}

impl DiskState {
    fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &PathBuf) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

// ── Shell helpers ─────────────────────────────────────────────────────────────

async fn is_mounted(mount_point: &str) -> bool {
    Command::new("mountpoint")
        .arg("-q")
        .arg(mount_point)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn mount_sshfs(
    host: &str,
    port: i32,
    username: &str,
    password: &str,
    mount_point: &str,
) -> anyhow::Result<()> {
    let mut child = Command::new("sshfs")
        .arg(format!("{username}@{host}:/"))
        .arg(mount_point)
        .arg("-p")
        .arg(port.to_string())
        .arg("-o")
        .arg("password_stdin,StrictHostKeyChecking=no,reconnect,ServerAliveInterval=15,ServerAliveCountMax=3")
        .stdin(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(password.as_bytes()).await?;
    }

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("sshfs failed for {username}@{host}:{port}");
    }
    Ok(())
}

async fn ensure_img(mount_point: &str, size_gb: i32) -> anyhow::Result<String> {
    let img = format!("{mount_point}/yolab-osd.img");
    if !tokio::fs::try_exists(&img).await.unwrap_or(false) {
        let status = Command::new("truncate")
            .arg("-s")
            .arg(format!("{size_gb}G"))
            .arg(&img)
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("truncate failed for {img}");
        }
        tracing::info!("created sparse image {img} ({size_gb} GB)");
    }
    Ok(img)
}

async fn attach_loop(img: &str) -> anyhow::Result<String> {
    // Try with direct I/O first for better Ceph performance.
    let out = Command::new("losetup")
        .args(["--direct-io=on", "-f", img, "--show"])
        .output()
        .await?;

    let out = if out.status.success() {
        out
    } else {
        Command::new("losetup")
            .args(["-f", img, "--show"])
            .output()
            .await?
    };

    if !out.status.success() {
        anyhow::bail!("losetup failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn loop_device_for(img: &str) -> Option<String> {
    let out = Command::new("losetup")
        .args(["-j", img])
        .output()
        .await
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    // Output format: /dev/loopN: [dev]:ino (/path/to/img)
    line.lines().next().map(|l| l.split(':').next().unwrap_or("").to_string())
}

async fn detach_loop(loop_device: &str) {
    let _ = Command::new("losetup")
        .args(["-d", loop_device])
        .status()
        .await;
}

async fn unmount_sshfs(mount_point: &str) {
    let ok = Command::new("fusermount3")
        .args(["-u", mount_point])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = Command::new("umount").arg(mount_point).status().await;
    }
}

// ── Config reader ─────────────────────────────────────────────────────────────

async fn read_node_id(config_path: &str) -> Option<i32> {
    let text = tokio::fs::read_to_string(config_path).await.ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let ye = table.get("yolab_external")?.as_table()?;
    ye.get("node_id")?.as_integer().map(|n| n as i32)
}

// ── Core logic ────────────────────────────────────────────────────────────────

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn fetch_volumes(url: &str, token: &str) -> anyhow::Result<Vec<VolumeInfo>> {
    Ok(http_client()
        .get(format!("{url}/storage/volume"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<VolumeInfo>>()
        .await?)
}

async fn provision(vol: &VolumeInfo) -> anyhow::Result<AttachedDisk> {
    let mount_point = format!("/var/lib/yolab/vdisk-{}", vol.id);
    tokio::fs::create_dir_all(&mount_point).await?;

    if !is_mounted(&mount_point).await {
        mount_sshfs(&vol.host, vol.port, &vol.username, &vol.password, &mount_point).await?;
    }

    let img = ensure_img(&mount_point, vol.size_gb).await?;

    // If already looped (e.g. we crashed after mount but before saving state), reuse it.
    let loop_device = if let Some(dev) = loop_device_for(&img).await {
        dev
    } else {
        attach_loop(&img).await?
    };

    tracing::info!(
        "virtual disk {}: {} → loop {}",
        vol.id,
        mount_point,
        loop_device
    );

    Ok(AttachedDisk {
        volume_id: vol.id,
        host: vol.host.clone(),
        port: vol.port,
        username: vol.username.clone(),
        password: vol.password.clone(),
        size_gb: vol.size_gb,
        mount_point,
        loop_device,
    })
}

async fn restore(disk: &AttachedDisk) {
    if !is_mounted(&disk.mount_point).await {
        if let Err(e) = mount_sshfs(
            &disk.host,
            disk.port,
            &disk.username,
            &disk.password,
            &disk.mount_point,
        )
        .await
        {
            tracing::warn!("restore: disk {}: mount failed: {e}", disk.volume_id);
            return;
        }
    }

    let img = format!("{}/yolab-osd.img", disk.mount_point);
    if loop_device_for(&img).await.is_none() {
        match attach_loop(&img).await {
            Ok(dev) => tracing::info!("restore: disk {} → loop {dev}", disk.volume_id),
            Err(e) => tracing::warn!("restore: disk {}: losetup failed: {e}", disk.volume_id),
        }
    }
}

// ── Background task entry point ───────────────────────────────────────────────

pub async fn run(config: Arc<Config>) {
    let state_path = config.built_dir.join("virtual-disks.json");
    let mut disk_state = DiskState::load(&state_path);

    // Attempt to restore mounts from previous state before first poll.
    for disk in disk_state.disks.clone() {
        restore(&disk).await;
    }

    let mut interval = time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;

        let Some((url, token)) = ye_creds(&config) else {
            continue;
        };
        let Some(our_node_id) = read_node_id(&config.config_path).await else {
            continue;
        };

        let volumes = match fetch_volumes(&url, &token).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("virtual-disk poll: {e}");
                continue;
            }
        };

        let ours: Vec<&VolumeInfo> = volumes
            .iter()
            .filter(|v| v.node_id == Some(our_node_id))
            .collect();

        // Provision newly assigned disks.
        for vol in &ours {
            if !disk_state.disks.iter().any(|d| d.volume_id == vol.id) {
                match provision(vol).await {
                    Ok(disk) => {
                        disk_state.disks.push(disk);
                        disk_state.save(&state_path);
                    }
                    Err(e) => {
                        tracing::warn!("virtual-disk provision {}: {e}", vol.id);
                    }
                }
            }
        }

        // Detach disks no longer assigned here.
        let ours_ids: Vec<i32> = ours.iter().map(|v| v.id).collect();
        let mut removed: Vec<i32> = Vec::new();
        for disk in &disk_state.disks {
            if !ours_ids.contains(&disk.volume_id) {
                tracing::info!("virtual-disk {}: detaching", disk.volume_id);
                detach_loop(&disk.loop_device).await;
                unmount_sshfs(&disk.mount_point).await;
                removed.push(disk.volume_id);
            }
        }
        if !removed.is_empty() {
            disk_state.disks.retain(|d| !removed.contains(&d.volume_id));
            disk_state.save(&state_path);
        }
    }
}
