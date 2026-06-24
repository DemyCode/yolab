use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

use crate::config::Config;
use crate::error::Result;
use crate::routers::backups::ye_creds;
use crate::AppState;

// ── ConfigMap helpers ──────────────────────────────────────────────────────────

const CM_NAME: &str = "yolab-virtual-disks";
const CM_NS: &str = "default";

/// Read the assignment map from the ConfigMap (volume_id → node_hostname).
async fn read_assignments() -> std::collections::HashMap<i32, String> {
    let out = Command::new("kubectl")
        .args([
            "get", "configmap", CM_NAME, "-n", CM_NS,
            "-o", "jsonpath={.data.assignments}",
        ])
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });
    match out {
        Some(s) if !s.is_empty() => {
            serde_json::from_str(&s).unwrap_or_default()
        }
        _ => HashMap::new(),
    }
}

/// Write the assignment map to the ConfigMap (create or update).
async fn write_assignments(assignments: &HashMap<i32, String>) -> anyhow::Result<()> {
    let json = serde_json::to_string(assignments)?;
    let exists = Command::new("kubectl")
        .args(["get", "configmap", CM_NAME, "-n", CM_NS])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    if exists {
        let patch = serde_json::json!({
            "data": { "assignments": json }
        });
        let mut child = Command::new("kubectl")
            .args(["patch", "configmap", CM_NAME, "-n", CM_NS, "--type=merge", "-p", &patch.to_string()])
            .spawn()?;
        let status = child.wait().await?;
        if !status.success() {
            anyhow::bail!("kubectl patch configmap failed");
        }
    } else {
        let manifest = format!(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {CM_NAME}\n  namespace: {CM_NS}\ndata:\n  assignments: '{}'\n",
            json.replace('\'', "'\\''")
        );
        let mut child = Command::new("kubectl")
            .args(["apply", "-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(manifest.as_bytes()).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            anyhow::bail!("kubectl apply configmap failed");
        }
    }
    Ok(())
}

// ── HTTP types ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct VirtualDiskInfo {
    pub id: i32,
    pub host: String,
    pub port: i32,
    pub username: String,
    pub password: String,
    pub box_type: String,
    pub size_gb: i32,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_hostname: Option<String>,
    pub mounted: bool,
}

#[derive(Deserialize)]
pub struct CreateVirtualDiskRequest {
    pub box_type: String,
    pub node_hostname: String,
}

// ── VolumeInfo from external API ────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct VolumeInfo {
    id: i32,
    host: String,
    port: i32,
    username: String,
    password: String,
    box_type: String,
    size_gb: i32,
    created_at: String,
}

// ── HTTP handlers ──────────────────────────────────────────────────────────────

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn ye_post(url: &str, token: &str, path: &str, body: serde_json::Value) -> anyhow::Result<reqwest::Response> {
    let resp = http_client()
        .post(format!("{url}{path}"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("external API {status}: {text}");
    }
    Ok(resp)
}

async fn ye_get(url: &str, token: &str, path: &str) -> anyhow::Result<reqwest::Response> {
    let resp = http_client()
        .get(format!("{url}{path}"))
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("external API {status}: {text}");
    }
    Ok(resp)
}

async fn ye_delete(url: &str, token: &str, path: &str) -> anyhow::Result<reqwest::Response> {
    let resp = http_client()
        .delete(format!("{url}{path}"))
        .bearer_auth(token)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("external API {status}: {text}");
    }
    Ok(resp)
}

/// GET /api/virtual-disks
pub async fn get_virtual_disks(State(state): State<AppState>) -> Result<Json<Vec<VirtualDiskInfo>>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Ok(Json(vec![]));
    };
    let volumes: Vec<VolumeInfo> = ye_get(&url, &token, "/storage/volume").await?
        .json().await.map_err(|e| anyhow::anyhow!(e))?;

    let assignments = read_assignments().await;

    let state_path = state.config.built_dir.join("virtual-disks.json");
    let mounted_ids: Vec<i32> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str::<DiskState>(&s).ok())
        .map(|ds| ds.disks.iter().map(|d| d.volume_id).collect())
        .unwrap_or_default();

    let result: Vec<VirtualDiskInfo> = volumes
        .into_iter()
        .map(|v| {
            let node_hostname = assignments.get(&v.id).cloned();
            let mounted = mounted_ids.contains(&v.id);
            VirtualDiskInfo {
                id: v.id,
                host: v.host,
                port: v.port,
                username: v.username,
                password: v.password,
                box_type: v.box_type,
                size_gb: v.size_gb,
                created_at: v.created_at,
                node_hostname,
                mounted,
            }
        })
        .collect();

    Ok(Json(result))
}

/// POST /api/virtual-disks
pub async fn create_virtual_disk(
    State(state): State<AppState>,
    Json(req): Json<CreateVirtualDiskRequest>,
) -> Result<Json<VirtualDiskInfo>> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured").into());
    };

    let vol: VolumeInfo = ye_post(&url, &token, "/storage/volume", serde_json::json!({ "box_type": req.box_type })).await?
        .json().await.map_err(|e| anyhow::anyhow!(e))?;

    let mut assignments = read_assignments().await;
    assignments.insert(vol.id, req.node_hostname.clone());
    write_assignments(&assignments).await.map_err(|e| anyhow::anyhow!(e))?;

    Ok(Json(VirtualDiskInfo {
        id: vol.id,
        host: vol.host,
        port: vol.port,
        username: vol.username,
        password: vol.password,
        box_type: vol.box_type,
        size_gb: vol.size_gb,
        created_at: vol.created_at,
        node_hostname: Some(req.node_hostname),
        mounted: false,
    }))
}

/// DELETE /api/virtual-disks/:id
pub async fn delete_virtual_disk(
    State(state): State<AppState>,
    Path(volume_id): Path<i32>,
) -> Result<axum::http::StatusCode> {
    let Some((url, token)) = ye_creds(&state.config) else {
        return Err(anyhow::anyhow!("platform API not configured").into());
    };

    ye_delete(&url, &token, &format!("/storage/volume/{volume_id}")).await?;

    let mut assignments = read_assignments().await;
    assignments.remove(&volume_id);
    write_assignments(&assignments).await.map_err(|e| anyhow::anyhow!(e))?;

    Ok(axum::http::StatusCode::OK)
}

// ── Persisted local state ──────────────────────────────────────────────────────

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

// ── Shell helpers ──────────────────────────────────────────────────────────────

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

// ── Core provisioning logic ────────────────────────────────────────────────────

async fn fetch_volumes(url: &str, token: &str) -> anyhow::Result<Vec<VolumeInfo>> {
    Ok(ye_get(url, token, "/storage/volume").await?.json().await?)
}

async fn provision(vol: &VolumeInfo) -> anyhow::Result<AttachedDisk> {
    let mount_point = format!("/var/lib/yolab/vdisk-{}", vol.id);
    tokio::fs::create_dir_all(&mount_point).await?;

    if !is_mounted(&mount_point).await {
        mount_sshfs(&vol.host, vol.port, &vol.username, &vol.password, &mount_point).await?;
    }

    let img = ensure_img(&mount_point, vol.size_gb).await?;

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

// ── Background task ────────────────────────────────────────────────────────────

pub async fn run(config: Arc<Config>) {
    let state_path = config.built_dir.join("virtual-disks.json");
    let mut disk_state = DiskState::load(&state_path);

    for disk in disk_state.disks.clone() {
        restore(&disk).await;
    }

    let mut interval = time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;

        let Some((url, token)) = ye_creds(&config) else {
            continue;
        };

        let volumes = match fetch_volumes(&url, &token).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("virtual-disk poll: {e}");
                continue;
            }
        };

        let assignments = read_assignments().await;

        let my_volumes: Vec<&VolumeInfo> = volumes
            .iter()
            .filter(|vol| {
                assignments
                    .get(&vol.id)
                    .map(|node| node == &config.hostname)
                    .unwrap_or(false)
            })
            .collect();

        for vol in &my_volumes {
            if !disk_state.disks.iter().any(|d| d.volume_id == vol.id) {
                match provision(vol).await {
                    Ok(disk) => {
                        disk_state.disks.push(disk);
                        disk_state.save(&state_path);
                    }
                    Err(e) => tracing::warn!("virtual-disk provision {}: {e}", vol.id),
                }
            }
        }

        let assigned_ids: Vec<i32> = my_volumes.iter().map(|v| v.id).collect();
        let remote_ids: Vec<i32> = volumes.iter().map(|v| v.id).collect();
        let mut removed: Vec<i32> = Vec::new();

        for disk in &disk_state.disks {
            if !remote_ids.contains(&disk.volume_id) {
                tracing::info!("virtual-disk {}: volume deleted, detaching", disk.volume_id);
                detach_loop(&disk.loop_device).await;
                unmount_sshfs(&disk.mount_point).await;
                removed.push(disk.volume_id);
            } else if !assigned_ids.contains(&disk.volume_id) {
                tracing::info!("virtual-disk {}: reassigned away from this node, detaching", disk.volume_id);
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
