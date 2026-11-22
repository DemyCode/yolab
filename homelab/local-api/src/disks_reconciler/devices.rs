use std::io::Read;

use super::*;

pub(super) const BLUESTORE_MAGIC: &[u8] = b"bluestore block device\n";
pub(super) const CEPH_FSID_KEY: &[u8] = b"\x09\x00\x00\x00ceph_fsid";

pub(super) fn is_user_disk(name: &str) -> bool {
    const VIRTUAL_PREFIXES: [&str; 6] = ["rbd", "loop", "zram", "zd", "md", "dm-"];
    !VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DiskFlags {
    pub has_partitions: bool,
    pub mounted: bool,
}

pub(crate) fn parse_disk_flags(dev: &Value) -> DiskFlags {
    fn mounted_anywhere(n: &Value) -> bool {
        let own = match &n["mountpoints"] {
            Value::Array(a) => a.iter().any(|m| m.as_str().is_some_and(|s| !s.is_empty())),
            v => v.as_str().is_some_and(|s| !s.is_empty()),
        };
        own || n["children"]
            .as_array()
            .is_some_and(|c| c.iter().any(mounted_anywhere))
    }

    DiskFlags {
        has_partitions: dev["children"]
            .as_array()
            .is_some_and(|c| c.iter().any(|ch| ch["type"].as_str() == Some("part"))),
        mounted: mounted_anywhere(dev),
    }
}

pub(super) async fn scan_devices<H: Host>(host: &H) -> Result<Vec<(String, DiskFlags)>> {
    let out = host
        .run_cmd("lsblk", &["-J", "-o", "NAME,TYPE,MOUNTPOINTS"])
        .await?;
    if !out.success {
        anyhow::bail!("lsblk failed: {}", out.stderr.trim());
    }
    let json = serde_json::from_slice::<Value>(out.stdout.as_bytes()).context("parse lsblk -J")?;
    let devs = json["blockdevices"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("lsblk -J has no blockdevices list"))?;
    let mut devices = Vec::new();
    for dev in devs {
        if dev["type"].as_str() != Some("disk") {
            continue;
        }
        let name = dev["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        if !is_user_disk(&name) {
            continue;
        }
        let flags = parse_disk_flags(dev);
        if flags.mounted {
            continue;
        }
        devices.push((name, flags));
    }
    devices.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(devices)
}

pub(super) fn read_bluestore_header(device: &str) -> std::io::Result<[u8; 4096]> {
    let path = if device.starts_with('/') {
        device.to_string()
    } else {
        format!("/dev/{device}")
    };
    let mut buf = [0u8; 4096];
    let mut f = std::fs::File::open(path)?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub(super) fn bluestore_fsid(device: &str) -> std::io::Result<Option<String>> {
    Ok(fsid_in_label(&read_bluestore_header(device)?))
}

pub(super) fn device_ownership(device: &str, our_fsid: &str) -> Ownership {
    match bluestore_fsid(device) {
        Ok(fsid) => Ownership::read(fsid.as_deref(), our_fsid),
        Err(e) => {
            tracing::warn!("{device}: could not read its label, so it is left alone: {e}");
            Ownership::Unknown
        }
    }
}

pub(super) fn fsid_in_label(buf: &[u8; 4096]) -> Option<String> {
    if !buf.starts_with(BLUESTORE_MAGIC) {
        return None;
    }
    let pos = buf
        .windows(CEPH_FSID_KEY.len())
        .position(|w| w == CEPH_FSID_KEY)?;
    let vs = pos + CEPH_FSID_KEY.len();
    if vs + 40 > buf.len() {
        return None;
    }
    if u32::from_le_bytes(buf[vs..vs + 4].try_into().ok()?) != 36 {
        return None;
    }
    let fsid = std::str::from_utf8(&buf[vs + 4..vs + 40]).ok()?;
    is_uuid(fsid).then(|| fsid.to_string())
}

pub(super) fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(&l, p)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) const ID_PREFIXES: [&str; 6] = ["wwn-", "nvme-eui.", "nvme-", "ata-", "scsi-", "usb-"];
pub(super) const BY_ID_DIR: &str = "/dev/disk/by-id";

pub(super) fn disk_id(device: &str) -> String {
    disk_id_from(device, stable_id_for(device).as_deref())
}

pub(super) fn stable_id_for(device: &str) -> Option<String> {
    let target = std::fs::canonicalize(format!("/dev/{device}")).ok()?;
    let mut best: Option<(usize, String)> = None;
    for entry in std::fs::read_dir(BY_ID_DIR).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("-part") || name.contains("-part") {
            continue;
        }
        let Some(rank) = ID_PREFIXES.iter().position(|p| name.starts_with(p)) else {
            continue;
        };
        if std::fs::canonicalize(entry.path()).ok().as_deref() != Some(target.as_path()) {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, name));
        }
    }
    best.map(|(_, name)| name)
}

pub(super) fn disk_id_from(device: &str, stable: Option<&str>) -> String {
    if let Some(serial) = stable {
        let s = serial.trim();
        if !s.is_empty() {
            let safe: String = s
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
                .to_lowercase();
            return format!("serial-{}", safe.trim_matches('-'));
        }
    }
    format!("dev-{device}")
}

pub(super) fn disk_meta(device: &str, our_fsid: &str, flags: DiskFlags) -> Disk {
    let model = std::fs::read_to_string(format!("/sys/block/{device}/device/model"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let size_bytes: u64 = std::fs::read_to_string(format!("/sys/block/{device}/size"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        * 512;
    Disk {
        device: device.to_string(),
        model,
        size_bytes,
        is_loop: false,
        is_virtual: false,
        ownership: device_ownership(device, our_fsid),
        has_partitions: flags.has_partitions,
        mounted: flags.mounted,
        osd_id: None,
        progress: None,
    }
}
