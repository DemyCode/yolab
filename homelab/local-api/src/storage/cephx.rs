use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use kube::Client;
use serde_json::{json, Value};

use crate::error::Outcome;
use crate::host::Host;
use crate::routers::backup_common::Backend;
use crate::runtime::{Controller, Ctx, Requirement, Scope, Tick};

use super::csi_secrets::{NODE_CAPS, NODE_ENTITY, NODE_ENTITY_AES256K, PROVISIONER_ENTITY};
use super::StorageEnv;

pub const AES256K: &str = "aes256k";
pub const IMAGES_ID: &str = "yolab-images";
const IMAGES_ENTITY: &str = "client.yolab-images";
const ADMIN: &str = "client.admin";
const BOOTSTRAP_OSD: &str = "client.bootstrap-osd";
const NS: &str = "rook-ceph";
const RESTART_LEASE: &str = "yolab-ceph-key-restart";
const RESTART_LEASE_SECS: i64 = 600;
const FACTS_PREFIX: &str = "yolab/cephx/node/";
const PROBE: Duration = Duration::from_secs(60);

pub const UNUSED_CLIENTS: &[&str] = &[
    "client.bootstrap-mds",
    "client.bootstrap-mgr",
    "client.bootstrap-rbd",
    "client.bootstrap-rbd-mirror",
    "client.bootstrap-rgw",
    "client.crash",
    "client.ceph-exporter",
    "client.csi-rbd-node",
    "client.csi-rbd-provisioner",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Aes,
    Aes256k,
    Unknown,
}

pub fn key_type(key: &str) -> KeyType {
    let first = base64::engine::general_purpose::STANDARD
        .decode(key.trim())
        .ok()
        .and_then(|bytes| bytes.first().copied());
    match first {
        Some(1) => KeyType::Aes,
        Some(2) => KeyType::Aes256k,
        _ => KeyType::Unknown,
    }
}

pub fn kernel_speaks_aes256k(release: &str) -> bool {
    release
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 7)
}

fn running_kernel(root: &Path) -> String {
    std::fs::read_to_string(root.join("proc/sys/kernel/osrelease"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn keyring_key(text: &str, entity: &str) -> Option<String> {
    let header = format!("[{entity}]");
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == header;
            continue;
        }
        if inside {
            if let Some(value) = key_line_value(t) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn key_line_value(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("key")?.trim_start();
    Some(rest.strip_prefix('=')?.trim())
}

pub fn keyring_with_key(text: &str, entity: &str, key: &str) -> String {
    let header = format!("[{entity}]");
    let key_line = format!("\tkey = {key}");
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut placed = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if inside && !placed {
                out.push(key_line.clone());
                placed = true;
            }
            inside = t == header;
            out.push(line.to_string());
            continue;
        }
        if inside && key_line_value(t).is_some() {
            if !placed {
                out.push(key_line.clone());
                placed = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if inside && !placed {
        out.push(key_line.clone());
        placed = true;
    }
    if !placed {
        out.push(header);
        out.push(key_line);
    }
    out.join("\n") + "\n"
}

async fn write_keyring<H: Host>(host: &H, path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = path.with_extension("yolab-new");
    std::fs::write(&staged, text).with_context(|| format!("write {}", staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
    let staged_s = staged.to_string_lossy().into_owned();
    let owned = host
        .run_cmd("chown", &["ceph:ceph", staged_s.as_str()])
        .await?;
    if !owned.success {
        return Err(anyhow!(
            "chown ceph:ceph {staged_s}: {}",
            owned.stderr.trim()
        ));
    }
    std::fs::rename(&staged, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEntry {
    pub key: String,
    pub pending: Option<String>,
}

impl AuthEntry {
    pub fn target(&self) -> &str {
        self.pending.as_deref().unwrap_or(&self.key)
    }
}

fn parse_auth_entry(v: &Value) -> Option<AuthEntry> {
    let e = v.as_array()?.first()?;
    Some(AuthEntry {
        key: e["key"].as_str()?.to_string(),
        pending: e["pending_key"]
            .as_str()
            .filter(|k| !k.is_empty())
            .map(str::to_string),
    })
}

async fn auth_entry<H: Host>(host: &H, entity: &str) -> Result<Option<AuthEntry>> {
    match host.ceph_json(&["auth", "get", entity]).await {
        Ok(v) => Ok(parse_auth_entry(&v)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {entity}")),
    }
}

fn listed_keys(auth_ls: &Value) -> Vec<(String, String)> {
    auth_ls["auth_dump"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some((
                e["entity"].as_str()?.to_string(),
                e["key"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn is_daemon(entity: &str) -> bool {
    ["osd.", "mgr.", "mds."]
        .iter()
        .any(|p| entity.starts_with(p))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Daemon {
    Mon,
    Mgr,
    Mds,
    Osd(u32),
}

impl Daemon {
    pub fn entity(&self, node: &str) -> String {
        match self {
            Daemon::Mon => "mon.".into(),
            Daemon::Mgr => format!("mgr.{node}"),
            Daemon::Mds => format!("mds.{node}"),
            Daemon::Osd(n) => format!("osd.{n}"),
        }
    }

    fn dir(&self, root: &Path, node: &str) -> PathBuf {
        let base = root.join("var/lib/ceph");
        match self {
            Daemon::Mon => base.join(format!("mon/ceph-{node}")),
            Daemon::Mgr => base.join(format!("mgr/ceph-{node}")),
            Daemon::Mds => base.join(format!("mds/ceph-{node}")),
            Daemon::Osd(n) => base.join(format!("osd/ceph-{n}")),
        }
    }

    fn keyring(&self, root: &Path, node: &str) -> PathBuf {
        self.dir(root, node).join("keyring")
    }

    pub fn unit(&self, node: &str) -> String {
        match self {
            Daemon::Mon => format!("ceph-mon-{node}.service"),
            Daemon::Mgr => format!("ceph-mgr-{node}.service"),
            Daemon::Mds => format!("ceph-mds-{node}.service"),
            Daemon::Osd(n) => format!("yolab-ceph-osd@{n}.service"),
        }
    }
}

pub fn local_daemons(root: &Path, node: &str) -> Vec<Daemon> {
    let mut out: Vec<Daemon> = [Daemon::Mon, Daemon::Mgr, Daemon::Mds]
        .into_iter()
        .filter(|d| d.keyring(root, node).exists())
        .collect();
    let mut osds: Vec<u32> = std::fs::read_dir(root.join("var/lib/ceph/osd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("ceph-")?.parse().ok())
        .filter(|n| osd_block(root, *n).is_some())
        .collect();
    osds.sort_unstable();
    out.extend(osds.into_iter().map(Daemon::Osd));
    out
}

fn osd_block(root: &Path, n: u32) -> Option<PathBuf> {
    std::fs::canonicalize(root.join(format!("var/lib/ceph/osd/ceph-{n}/block"))).ok()
}

fn label_osd_key(show_label: &Value) -> Option<String> {
    show_label.as_object()?.values().next()?["osd_key"]
        .as_str()
        .map(str::to_string)
}

async fn local_key<H: Host>(
    host: &H,
    root: &Path,
    node: &str,
    d: Daemon,
) -> Result<Option<String>> {
    match d {
        Daemon::Osd(n) => {
            let Some(dev) = osd_block(root, n) else {
                return Ok(None);
            };
            let dev_s = dev.to_string_lossy().into_owned();
            let out = host
                .run_cmd_bounded(
                    "ceph-bluestore-tool",
                    &["show-label", "--dev", dev_s.as_str()],
                    PROBE,
                )
                .await?;
            if !out.success {
                return Err(anyhow!("read the label of {dev_s}: {}", out.stderr.trim()));
            }
            let label: Value = serde_json::from_str(&out.stdout)
                .with_context(|| format!("parse the label of {dev_s}"))?;
            Ok(label_osd_key(&label))
        }
        _ => Ok(std::fs::read_to_string(d.keyring(root, node))
            .ok()
            .and_then(|text| keyring_key(&text, &d.entity(node)))),
    }
}

fn stopped_marker(root: &Path, n: u32) -> PathBuf {
    root.join(format!("var/lib/yolab/cephx/stopped-osd.{n}"))
}

async fn resume_stopped<H: Host>(host: &H, root: &Path) -> Result<()> {
    let dir = root.join("var/lib/yolab/cephx");
    let markers: Vec<u32> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("stopped-osd.")?
                .parse()
                .ok()
        })
        .collect();
    for n in markers {
        let unit = Daemon::Osd(n).unit("");
        let out = host.systemctl(&["start", unit.as_str()]).await?;
        if !out.success {
            return Err(anyhow!("start {unit} again: {}", out.stderr.trim()));
        }
        std::fs::remove_file(stopped_marker(root, n))?;
    }
    Ok(())
}

async fn adopt<H: Host>(host: &H, root: &Path, node: &str, d: Daemon, want: &str) -> Result<()> {
    let entity = d.entity(node);
    match d {
        Daemon::Mon | Daemon::Mgr | Daemon::Mds => {
            let path = d.keyring(root, node);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            write_keyring(host, &path, &keyring_with_key(&text, &entity, want)).await?;
            if d != Daemon::Mon {
                let unit = d.unit(node);
                let out = host.systemctl(&["restart", unit.as_str()]).await?;
                if !out.success {
                    return Err(anyhow!("restart {unit}: {}", out.stderr.trim()));
                }
            }
        }
        Daemon::Osd(n) => {
            let dev =
                osd_block(root, n).ok_or_else(|| anyhow!("osd.{n} has no block device here"))?;
            let dev_s = dev.to_string_lossy().into_owned();
            let unit = d.unit(node);
            let marker = stopped_marker(root, n);
            if let Some(parent) = marker.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&marker, "")?;
            let stopped = host.systemctl(&["stop", unit.as_str()]).await?;
            if !stopped.success {
                return Err(anyhow!("stop {unit}: {}", stopped.stderr.trim()));
            }
            let labelled = host
                .run_cmd_bounded(
                    "ceph-bluestore-tool",
                    &[
                        "set-label-key",
                        "--dev",
                        dev_s.as_str(),
                        "-k",
                        "osd_key",
                        "-v",
                        want,
                    ],
                    PROBE,
                )
                .await?;
            let started = host.systemctl(&["start", unit.as_str()]).await?;
            if !labelled.success {
                return Err(anyhow!(
                    "write osd.{n}'s new key to {dev_s}: {}",
                    labelled.stderr.trim()
                ));
            }
            if !started.success {
                return Err(anyhow!("start {unit}: {}", started.stderr.trim()));
            }
            std::fs::remove_file(&marker)?;
        }
    }
    tracing::info!("cephx: {entity} on {node} now uses its new key");
    Ok(())
}

pub fn all_pgs_active(pg_stat: &Value) -> bool {
    let summary = &pg_stat["pg_summary"];
    let Some(total) = summary["num_pgs"].as_u64() else {
        return false;
    };
    let states = summary["num_pg_by_state"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let active: u64 = states
        .iter()
        .filter(|s| {
            s["name"]
                .as_str()
                .is_some_and(|n| n.split('+').any(|p| p == "active"))
        })
        .filter_map(|s| s["num"].as_u64())
        .sum();
    active == total
}

pub fn redundancy_impossible(osd_ls: &Value, pools: &Value) -> bool {
    let osds = osd_ls.as_array().map(|a| a.len()).unwrap_or(0);
    let single_copy = pools
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| p["size"].as_u64() == Some(1));
    osds <= 1 || single_copy
}

async fn may_restart<H: Host>(host: &H, d: Daemon) -> Result<Option<String>> {
    let pgs = host.ceph_json(&["pg", "stat"]).await?;
    if !all_pgs_active(&pgs) {
        return Ok(Some("some placement groups are not active yet".into()));
    }
    if let Daemon::Osd(n) = d {
        let id = n.to_string();
        let ok = host
            .ceph_json(&["osd", "ok-to-stop", id.as_str()])
            .await
            .map(|v| v["ok_to_stop"] == true)
            .unwrap_or(false);
        if !ok {
            let osds = host.ceph_json(&["osd", "ls"]).await?;
            let pools = host.ceph_json(&["osd", "pool", "ls", "detail"]).await?;
            if !redundancy_impossible(&osds, &pools) {
                return Ok(Some(format!("osd.{n} is not ok to stop yet")));
            }
        }
    }
    Ok(None)
}

fn lease_ref() -> Value {
    crate::k8s::reference("coordination.k8s.io/v1", "Lease", NS, RESTART_LEASE)
}

fn lease(holder: &str, now: DateTime<Utc>, resource_version: Option<&str>) -> Value {
    let mut metadata = json!({ "name": RESTART_LEASE, "namespace": NS });
    if let Some(rv) = resource_version {
        metadata["resourceVersion"] = json!(rv);
    }
    json!({
        "apiVersion": "coordination.k8s.io/v1",
        "kind": "Lease",
        "metadata": metadata,
        "spec": {
            "holderIdentity": holder,
            "leaseDurationSeconds": RESTART_LEASE_SECS,
            "renewTime": now.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        },
    })
}

pub fn lease_free_for(existing: &Value, me: &str, now: DateTime<Utc>) -> bool {
    let spec = &existing["spec"];
    if spec["holderIdentity"].as_str() == Some(me) {
        return true;
    }
    let renewed = spec["renewTime"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let secs = spec["leaseDurationSeconds"]
        .as_i64()
        .unwrap_or(RESTART_LEASE_SECS);
    renewed.is_none_or(|t| t + chrono::Duration::seconds(secs) < now)
}

async fn take_restart_lease(client: &Client, me: &str) -> Result<bool> {
    let now = Utc::now();
    match crate::k8s::get(client, &lease_ref()).await? {
        None => match crate::k8s::create(client, &lease(me, now, None)).await {
            Ok(()) => Ok(true),
            Err(e) if crate::k8s::refused_with(&e, 409) => Ok(false),
            Err(e) => Err(e),
        },
        Some(existing) if lease_free_for(&existing, me, now) => {
            let rv = existing["metadata"]["resourceVersion"].as_str();
            match crate::k8s::replace(client, &lease(me, now, rv)).await {
                Ok(()) => Ok(true),
                Err(e) if crate::k8s::refused_with(&e, 409) => Ok(false),
                Err(e) => Err(e),
            }
        }
        Some(_) => Ok(false),
    }
}

async fn release_restart_lease(client: &Client, me: &str) -> Result<()> {
    if let Some(existing) = crate::k8s::get(client, &lease_ref()).await? {
        if existing["spec"]["holderIdentity"].as_str() == Some(me) {
            crate::k8s::delete_if_present(client, &lease_ref()).await?;
        }
    }
    Ok(())
}

pub fn admin_kernel_mapped(root: &Path) -> bool {
    std::fs::read_dir(root.join("sys/bus/rbd/devices"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("config_info")).ok())
        .any(|info| info.split([' ', ',']).any(|opt| opt.trim() == "name=admin"))
}

async fn publish_facts<H: Host>(host: &H, root: &Path, node: &str) -> Result<()> {
    let key = format!("{FACTS_PREFIX}{node}");
    let facts = json!({ "admin_kernel_mapped": admin_kernel_mapped(root) }).to_string();
    host.ceph(&["config-key", "set", key.as_str(), facts.as_str()])
        .await?;
    Ok(())
}

async fn sync_bootstrap_osd<H: Host>(host: &H, root: &Path) -> Result<()> {
    let path = super::bootstrap::bootstrap_osd_keyring_path(root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Some(entry) = auth_entry(host, BOOTSTRAP_OSD).await? else {
        return Ok(());
    };
    if keyring_key(&text, BOOTSTRAP_OSD).as_deref() != Some(entry.key.as_str()) {
        write_keyring(
            host,
            &path,
            &keyring_with_key(&text, BOOTSTRAP_OSD, &entry.key),
        )
        .await?;
        tracing::info!("cephx: {BOOTSTRAP_OSD} keyring follows its rotated key");
    }
    Ok(())
}

pub async fn converge_node<H: Host>(b: &Backend<H>, root: &Path, node: &str) -> Result<Tick> {
    let host = &b.host;
    resume_stopped(host, root).await?;
    publish_facts(host, root, node)
        .await
        .warn_on_err("cephx: publish this machine's kernel facts");
    sync_bootstrap_osd(host, root)
        .await
        .warn_on_err("cephx: follow the bootstrap-osd key");

    let mut restarts = Vec::new();
    for d in local_daemons(root, node) {
        let entity = d.entity(node);
        let Some(entry) = auth_entry(host, &entity).await? else {
            continue;
        };
        let have = match local_key(host, root, node, d).await {
            Ok(Some(k)) => k,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("cephx: cannot read the key {entity} uses on this machine: {e:#}");
                continue;
            }
        };
        if have == entry.target() {
            continue;
        }
        if d == Daemon::Mon {
            adopt(host, root, node, d, entry.target()).await?;
        } else {
            restarts.push((d, entry.target().to_string()));
        }
    }

    let Some((d, want)) = restarts.into_iter().next() else {
        release_restart_lease(&b.kube, node).await?;
        return Ok(Tick::Done);
    };
    let entity = d.entity(node);
    if let Some(why) = may_restart(host, d).await? {
        return Ok(Tick::NotYet(format!(
            "{entity} needs a restart for its new key; {why}"
        )));
    }
    if !take_restart_lease(&b.kube, node).await? {
        return Ok(Tick::NotYet(format!(
            "{entity} needs a restart for its new key; another machine is restarting a Ceph daemon"
        )));
    }
    adopt(host, root, node, d, &want).await?;
    Ok(Tick::RequeueAfter(Duration::from_secs(30)))
}

fn denied(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("permission denied") || s.contains("errno 13")
}

async fn admin_denied<H: Host>(host: &H) -> bool {
    match host
        .run_cmd_bounded("ceph", &["--connect-timeout", "20", "-s"], PROBE)
        .await
    {
        Ok(out) => !out.success && denied(&out.stderr),
        Err(e) => denied(&e.to_string()),
    }
}

pub async fn heal_admin_keyring<H: Host>(
    host: &H,
    root: &Path,
    env: &StorageEnv,
    peers: &[String],
) -> Result<Tick> {
    if !admin_denied(host).await {
        return Ok(Tick::Done);
    }
    let token = crate::config::read_account_token(&env.config_path);
    if token.is_empty() {
        return Ok(Tick::NotYet("Ceph refuses this machine's admin key and there is no cluster token to fetch the current one".into()));
    }
    for peer in peers {
        let bundle = match super::bootstrap::fetch_join_bundle(peer, env.api_port, &token).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("cephx: [{peer}] could not hand over the admin key: {e:#}");
                continue;
            }
        };
        if super::bootstrap::validate_join_fsid(&bundle.fsid, &env.fsid, peer).is_err() {
            continue;
        }
        let Some(key) = keyring_key(&bundle.admin_keyring, ADMIN) else {
            continue;
        };
        let path = super::bootstrap::admin_keyring_path(root);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        write_keyring(host, &path, &keyring_with_key(&text, ADMIN, &key)).await?;
        tracing::warn!(
            "cephx: Ceph refused this machine's admin key; took the current one from [{peer}]"
        );
        return Ok(Tick::Done);
    }
    Ok(Tick::NotYet(
        "Ceph refuses this machine's admin key and no other machine handed over the current one"
            .into(),
    ))
}

pub async fn images_identity<H: Host>(host: &H, root: &Path, pool: &str) -> Option<PathBuf> {
    if !kernel_speaks_aes256k(&running_kernel(root)) {
        return None;
    }
    let path = root.join(format!("etc/ceph/ceph.{IMAGES_ENTITY}.keyring"));
    let have = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| keyring_key(&text, IMAGES_ENTITY));
    if have
        .as_deref()
        .is_some_and(|k| key_type(k) == KeyType::Aes256k)
    {
        return Some(path);
    }
    let caps = format!("profile rbd pool={pool}");
    let minted = host
        .ceph(&[
            "auth",
            "get-or-create",
            IMAGES_ENTITY,
            "mon",
            "profile rbd",
            "osd",
            caps.as_str(),
            "--key_type",
            AES256K,
        ])
        .await;
    let text = match minted {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("cephx: no {IMAGES_ENTITY} key ({e}); mapping the images disk as admin");
            return None;
        }
    };
    let key = keyring_key(&text, IMAGES_ENTITY)?;
    if key_type(&key) != KeyType::Aes256k {
        tracing::warn!(
            "cephx: {IMAGES_ENTITY} holds an old key type; mapping the images disk as admin"
        );
        return None;
    }
    match write_keyring(host, &path, &keyring_with_key("", IMAGES_ENTITY, &key)).await {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("cephx: could not store the {IMAGES_ENTITY} key ({e:#})");
            None
        }
    }
}

fn cipher<'a>(monmap: &'a Value, field: &str) -> Option<&'a str> {
    monmap[field]["name"].as_str()
}

fn monmap_speaks_aes256k(monmap: &Value) -> bool {
    monmap["features"]["persistent"]
        .as_array()
        .is_some_and(|f| f.iter().any(|x| x == "cephx_auth_aes256k"))
}

fn allowed_ciphers(monmap: &Value) -> Vec<String> {
    monmap["auth_allowed_ciphers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect()
}

fn node_ready(nodes: &[Value], name: &str) -> bool {
    nodes.iter().any(|n| {
        n["metadata"]["name"] == name
            && n["status"]["conditions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c["type"] == "Ready" && c["status"] == "True")
    })
}

pub fn every_kernel_speaks_aes256k(nodes: &[Value]) -> Result<(), String> {
    if nodes.is_empty() {
        return Err("no machine has reported its kernel yet".into());
    }
    for n in nodes {
        let release = n["status"]["nodeInfo"]["kernelVersion"]
            .as_str()
            .unwrap_or("");
        if !kernel_speaks_aes256k(release) {
            let name = n["metadata"]["name"].as_str().unwrap_or("a machine");
            return Err(format!(
                "{name} runs kernel {release:?}; kernel Ceph clients need 7.0 or newer for aes256k keys"
            ));
        }
    }
    Ok(())
}

async fn daemon_host<H: Host>(host: &H, entity: &str) -> Result<Option<String>> {
    if let Some(id) = entity.strip_prefix("osd.") {
        let meta = host.ceph_json(&["osd", "metadata", id]).await?;
        return Ok(meta["hostname"].as_str().map(str::to_string));
    }
    Ok(entity.split_once('.').map(|(_, n)| n.to_string()))
}

async fn full_quorum<H: Host>(host: &H) -> Result<bool> {
    let q = host.ceph_json(&["quorum_status"]).await?;
    let quorum = q["quorum_names"].as_array().map(|a| a.len()).unwrap_or(0);
    let mons = q["monmap"]["mons"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(usize::MAX);
    Ok(quorum == mons)
}

async fn facts_of<H: Host>(host: &H, node: &str) -> Option<Value> {
    let key = format!("{FACTS_PREFIX}{node}");
    let raw = host.ceph(&["config-key", "get", key.as_str()]).await.ok()?;
    serde_json::from_str(raw.trim()).ok()
}

async fn sessions_per_active_mds<H: Host>(host: &H) -> Result<Vec<Value>> {
    let status = host.ceph_json(&["fs", "status"]).await?;
    let mut out = Vec::new();
    for mds in status["mdsmap"].as_array().into_iter().flatten() {
        if mds["state"] != "active" {
            continue;
        }
        let Some(name) = mds["name"].as_str() else {
            continue;
        };
        let target = format!("mds.{name}");
        out.push(
            host.ceph_json(&["tell", target.as_str(), "session", "ls"])
                .await
                .unwrap_or(Value::Null),
        );
    }
    Ok(out)
}

async fn node_secret_moved(client: &Client) -> Result<bool> {
    let data = crate::k8s::secret_data(client, NS, "rook-csi-cephfs-node").await?;
    let wanted = NODE_ENTITY_AES256K
        .strip_prefix("client.")
        .unwrap_or(NODE_ENTITY_AES256K);
    Ok(data.is_some_and(|d| d.get("adminID").map(String::as_str) == Some(wanted)))
}

async fn rotate_admin<H: Host>(host: &H, root: &Path) -> Result<Tick> {
    host.ceph(&["auth", "get-or-create-pending", ADMIN]).await?;
    let entry = auth_entry(host, ADMIN)
        .await?
        .ok_or_else(|| anyhow!("{ADMIN} vanished"))?;
    let pending = entry
        .pending
        .ok_or_else(|| anyhow!("{ADMIN} has no pending key after asking for one"))?;
    if key_type(&pending) != KeyType::Aes256k {
        return Ok(Tick::NotYet(format!(
            "{ADMIN}'s pending key is not aes256k yet"
        )));
    }
    let path = super::bootstrap::admin_keyring_path(root);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    write_keyring(host, &path, &keyring_with_key(&text, ADMIN, &pending)).await?;
    host.ceph(&["-s"]).await?;
    Ok(Tick::NotYet(format!(
        "{ADMIN} took its aes256k key here; every other machine fetches it from a peer"
    )))
}

pub async fn rotation_step<H: Host>(b: &Backend<H>, root: &Path) -> Result<Tick> {
    let (host, client) = (&b.host, &b.kube);
    let monmap = host.ceph_json(&["mon", "dump"]).await?;
    if !monmap_speaks_aes256k(&monmap) {
        return Ok(Tick::Idle("the monitors predate aes256k keys".into()));
    }
    if cipher(&monmap, "auth_preferred_cipher") != Some(AES256K) {
        host.ceph(&["mon", "set", "auth_preferred_cipher", AES256K])
            .await?;
        return Ok(Tick::NotYet("new keys are aes256k from now on".into()));
    }

    let listed = listed_keys(&host.ceph_json(&["auth", "ls"]).await?);
    let daemons: Vec<&(String, String)> = listed.iter().filter(|(e, _)| is_daemon(e)).collect();
    for (entity, _) in &daemons {
        if let Some(entry) = auth_entry(host, entity).await? {
            if entry.pending.is_some() {
                return Ok(Tick::NotYet(format!(
                    "{entity} is restarting onto its new key"
                )));
            }
        }
    }
    let nodes = crate::k8s::nodes(client).await?;
    if let Some((entity, _)) = daemons
        .iter()
        .find(|(_, key)| key_type(key) == KeyType::Aes)
    {
        let Some(owner) = daemon_host(host, entity).await? else {
            return Ok(Tick::NotYet(format!(
                "cannot tell which machine runs {entity}"
            )));
        };
        if !node_ready(&nodes, &owner) {
            return Ok(Tick::NotYet(format!("{entity} waits for {owner} to be up")));
        }
        host.ceph(&["auth", "get-or-create-pending", entity.as_str()])
            .await?;
        return Ok(Tick::NotYet(format!("{entity} gets an aes256k key")));
    }

    if let Some(mon) = auth_entry(host, "mon.").await? {
        if key_type(&mon.key) == KeyType::Aes {
            if !full_quorum(host).await? {
                return Ok(Tick::NotYet(
                    "mon. rotates only with every monitor in quorum".into(),
                ));
            }
            host.ceph(&["auth", "rotate", "mon.", "--key_type", AES256K])
                .await?;
            return Ok(Tick::NotYet("mon. took an aes256k key".into()));
        }
    }

    if let Err(why) = every_kernel_speaks_aes256k(&nodes) {
        return Ok(Tick::Idle(why));
    }
    if !super::ceph_csi::speaks_aes256k(client).await? {
        return Ok(Tick::Idle(
            "waiting for the CSI driver that speaks aes256k to roll out".into(),
        ));
    }

    if cipher(&monmap, "auth_service_cipher") != Some(AES256K) {
        host.ceph(&["mon", "set", "auth_service_cipher", AES256K])
            .await?;
        return Ok(Tick::NotYet(
            "service tickets are aes256k from now on".into(),
        ));
    }
    let creatable = host
        .ceph(&["config", "get", "mon", "mon_auth_allow_insecure_key"])
        .await?;
    if creatable.trim() != "false" {
        host.ceph(&[
            "config",
            "set",
            "mon",
            "mon_auth_allow_insecure_key",
            "false",
        ])
        .await?;
        return Ok(Tick::NotYet(
            "no new aes key can be created any more".into(),
        ));
    }

    let has = |entity: &str| listed.iter().any(|(e, _)| e == entity);
    let aes = |entity: &str| {
        listed
            .iter()
            .any(|(e, k)| e == entity && key_type(k) == KeyType::Aes)
    };

    if aes(NODE_ENTITY) && !has(NODE_ENTITY_AES256K) {
        let mut args = vec!["auth", "get-or-create", NODE_ENTITY_AES256K];
        args.extend_from_slice(NODE_CAPS);
        args.extend(["--key_type", AES256K]);
        host.ceph(&args).await?;
        crate::runtime::wake("csi-secrets");
        return Ok(Tick::NotYet(format!(
            "new CephFS mounts move to {NODE_ENTITY_AES256K}"
        )));
    }
    if has(NODE_ENTITY) && has(NODE_ENTITY_AES256K) {
        if !node_secret_moved(client).await? {
            crate::runtime::wake("csi-secrets");
            return Ok(Tick::NotYet(format!(
                "waiting for the CSI secret to name {NODE_ENTITY_AES256K}"
            )));
        }
        let sessions = sessions_per_active_mds(host).await?;
        match crate::ceph::destructive::Unmounted::by_any_session(NODE_ENTITY, &sessions) {
            Some(proof) => {
                crate::ceph::destructive::retire_unmounted(host, proof).await?;
                return Ok(Tick::NotYet(format!("{NODE_ENTITY} retired")));
            }
            None => {
                return Ok(Tick::Idle(format!(
                    "{NODE_ENTITY} still holds CephFS mounts made before the switch; it retires once they are remounted (a reboot does it)"
                )))
            }
        }
    }

    if aes(PROVISIONER_ENTITY) {
        host.ceph(&["auth", "rotate", PROVISIONER_ENTITY, "--key_type", AES256K])
            .await?;
        crate::runtime::wake("csi-secrets");
        return Ok(Tick::NotYet(format!(
            "{PROVISIONER_ENTITY} took an aes256k key"
        )));
    }
    for entity in UNUSED_CLIENTS.iter().copied().chain([BOOTSTRAP_OSD]) {
        if aes(entity) {
            host.ceph(&["auth", "rotate", entity, "--key_type", AES256K])
                .await?;
            return Ok(Tick::NotYet(format!("{entity} took an aes256k key")));
        }
    }

    if aes(ADMIN) {
        for n in &nodes {
            let name = n["metadata"]["name"].as_str().unwrap_or_default();
            match facts_of(host, name).await {
                Some(f) if f["admin_kernel_mapped"] == false => {}
                Some(_) => {
                    return Ok(Tick::Idle(format!(
                        "{name}'s kernel still maps a disk as {ADMIN}; it switches to {IMAGES_ENTITY} at its next boot"
                    )))
                }
                None => return Ok(Tick::NotYet(format!("{name} has not reported its kernel mappings yet"))),
            }
        }
        return rotate_admin(host, root).await;
    }

    if let Some((entity, _)) = listed.iter().find(|(_, k)| key_type(k) == KeyType::Aes) {
        return Ok(Tick::Idle(format!(
            "{entity} still has an aes key; yolab does not know who uses it, so it is left for a person to rotate or remove"
        )));
    }
    if allowed_ciphers(&monmap) != [AES256K] {
        host.ceph(&["mon", "set", "auth_allowed_ciphers", AES256K])
            .await?;
        return Ok(Tick::NotYet(
            "only aes256k keys authenticate from now on".into(),
        ));
    }
    Ok(Tick::Done)
}

pub struct CephxLocalController;

impl Controller for CephxLocalController {
    fn name(&self) -> &'static str {
        "cephx-local"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph, Requirement::KubeApi]
    }
    async fn reconcile(&self, ctx: &Ctx) -> Result<Tick> {
        converge_node(&Backend::real().await?, Path::new("/"), &ctx.node).await
    }
}

pub struct AdminKeyringController {
    pub env: StorageEnv,
}

impl Controller for AdminKeyringController {
    fn name(&self) -> &'static str {
        "ceph-admin-keyring"
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        let client = crate::k8s::client().await?;
        let nodes = crate::k8s::nodes(&client).await?;
        let me = std::env::var("YOLAB_NODE_IPV6").unwrap_or_default();
        let peers = crate::k8s::peer_ipv6(&nodes, &me);
        heal_admin_keyring(&crate::host::HOST, Path::new("/"), &self.env, &peers).await
    }
}

pub struct CephxRotationController;

impl Controller for CephxRotationController {
    fn name(&self) -> &'static str {
        "cephx-rotation"
    }
    fn scope(&self) -> Scope {
        Scope::Cluster
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }
    fn requires(&self) -> &'static [Requirement] {
        &[Requirement::Ceph, Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &Ctx) -> Result<Tick> {
        rotation_step(&Backend::real().await?, Path::new("/")).await
    }
}

#[cfg(test)]
mod tests;
