use serde::{Deserialize, Serialize};

use crate::exec::CmdError;
use crate::host::Host;
use crate::storage::settings;

pub const RECORDS: &str = "yolab/vdisks/";
pub const KEYS: &str = "yolab/vdisk-keys/";
pub const FORMATTED: &str = "yolab/vdisk-formatted/";
pub const STATUS: &str = "yolab/vdisk-status/";

const DISK_ID_PREFIX: &str = "vdisk-";
const ALIGN: u64 = 4 << 20;
const USABLE_PERCENT: u64 = 97;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VdiskRecord {
    pub id: i64,
    pub node: String,
    pub box_type: String,
    pub size_gb: u64,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub removing: bool,
    #[serde(default)]
    pub osd_id: Option<i64>,
}

impl VdiskRecord {
    pub fn disk_id(&self) -> String {
        disk_id(self.id)
    }

    pub fn is_provisioned(&self) -> bool {
        !self.host.is_empty() && !self.username.is_empty() && !self.password.is_empty()
    }

    pub fn share(&self) -> String {
        format!("//{}/backup", self.host)
    }

    pub fn file_bytes(&self) -> u64 {
        file_bytes(self.size_gb)
    }
}

pub fn disk_id(id: i64) -> String {
    format!("{DISK_ID_PREFIX}{id}")
}

pub fn file_bytes(size_gb: u64) -> u64 {
    let usable = size_gb * 1_000_000_000 / 100 * USABLE_PERCENT;
    usable / ALIGN * ALIGN
}

pub fn crypt_name(id: i64) -> String {
    format!("yolab-vdisk-{id}")
}

pub fn crypt_dev(id: i64) -> String {
    format!("/dev/mapper/{}", crypt_name(id))
}

pub fn vg_name(id: i64) -> String {
    format!("yolab_vdisk_{id}")
}

pub fn osd_dm_name(id: i64) -> String {
    format!("{}-osd", vg_name(id))
}

pub fn osd_dev(id: i64) -> String {
    format!("/dev/mapper/{}", osd_dm_name(id))
}

pub fn id_of_osd_dev(path: &str) -> Option<i64> {
    let name = path.rsplit('/').next()?;
    name.strip_prefix("yolab_vdisk_")?
        .strip_suffix("-osd")?
        .parse::<i64>()
        .ok()
}

pub fn crush_bucket(id: i64) -> String {
    disk_id(id)
}

pub fn crush_location(id: i64) -> String {
    format!("root=default host={}", crush_bucket(id))
}

fn record_key(id: i64) -> String {
    format!("{RECORDS}{id}")
}

pub async fn list<H: Host>(host: &H) -> Result<Vec<VdiskRecord>, CmdError> {
    let raw = settings::dump(host, RECORDS).await?;
    raw.into_iter()
        .map(|(key, value)| {
            serde_json::from_str::<VdiskRecord>(&value)
                .map_err(|e| CmdError::parse(format!("config-key {RECORDS}{key}"), e))
        })
        .collect()
}

pub async fn get<H: Host>(host: &H, id: i64) -> Result<Option<VdiskRecord>, CmdError> {
    match settings::get(host, &record_key(id)).await? {
        None => Ok(None),
        Some(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| CmdError::parse(format!("config-key {}", record_key(id)), e)),
    }
}

pub async fn save<H: Host>(host: &H, record: &VdiskRecord) -> Result<(), CmdError> {
    let raw = serde_json::to_string(record)
        .map_err(|e| CmdError::parse(format!("config-key {}", record_key(record.id)), e))?;
    settings::set(host, &record_key(record.id), &raw).await
}

async fn remove_key<H: Host>(host: &H, key: &str) -> Result<(), CmdError> {
    match host.ceph(&["config-key", "rm", key]).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e),
    }
}

pub async fn forget<H: Host>(host: &H, id: i64) -> Result<(), CmdError> {
    remove_key(host, &format!("{KEYS}{id}")).await?;
    remove_key(host, &format!("{FORMATTED}{id}")).await?;
    remove_key(host, &format!("{STATUS}{id}")).await?;
    remove_key(host, &record_key(id)).await
}

pub async fn is_formatted<H: Host>(host: &H, id: i64) -> Result<bool, CmdError> {
    Ok(settings::get(host, &format!("{FORMATTED}{id}"))
        .await?
        .is_some())
}

pub async fn mark_formatted<H: Host>(host: &H, id: i64) -> Result<(), CmdError> {
    settings::set(host, &format!("{FORMATTED}{id}"), "1").await
}

pub async fn key<H: Host>(host: &H, id: i64) -> Result<Option<String>, CmdError> {
    settings::get(host, &format!("{KEYS}{id}")).await
}

pub async fn store_key<H: Host>(host: &H, id: i64, key: &str) -> Result<(), CmdError> {
    settings::set(host, &format!("{KEYS}{id}"), key).await
}

pub async fn statuses<H: Host>(
    host: &H,
) -> Result<std::collections::BTreeMap<String, String>, CmdError> {
    settings::dump(host, STATUS).await
}

pub async fn set_status<H: Host>(host: &H, id: i64, message: &str) -> Result<(), CmdError> {
    settings::set(host, &format!("{STATUS}{id}"), message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_virtual_disks_id_names_its_disk_and_its_crush_host_alike() {
        assert_eq!(disk_id(42), "vdisk-42");
        assert_eq!(crush_bucket(42), disk_id(42));
    }

    #[test]
    fn the_file_leaves_headroom_on_the_box_and_is_aligned_for_4k_sectors() {
        let bytes = file_bytes(900);
        assert!(bytes < 900 * 1_000_000_000);
        assert!(bytes > 850 * 1_000_000_000);
        assert_eq!(bytes % 4096, 0);
        assert_eq!(bytes % (4 << 20), 0);
    }

    #[test]
    fn the_osd_device_name_leads_back_to_its_virtual_disk() {
        assert_eq!(osd_dev(9), "/dev/mapper/yolab_vdisk_9-osd");
        assert_eq!(id_of_osd_dev(&osd_dev(9)), Some(9));
        assert_eq!(id_of_osd_dev("/dev/yolab_vdisk_9/osd"), None);
        assert_eq!(id_of_osd_dev("/dev/mapper/pool-ceph"), None);
    }

    #[test]
    fn a_virtual_disk_is_its_own_crush_host_so_a_copy_can_stay_home() {
        assert_eq!(crush_location(3), "root=default host=vdisk-3");
    }

    #[test]
    fn a_record_written_before_the_box_was_ready_still_reads() {
        let r: VdiskRecord =
            serde_json::from_str(r#"{"id":4,"node":"node1","box_type":"bx11","size_gb":900}"#)
                .unwrap();
        assert!(!r.is_provisioned());
        assert!(!r.removing);
        assert_eq!(r.osd_id, None);
    }

    #[test]
    fn the_share_is_the_boxes_backup_share() {
        let r = VdiskRecord {
            id: 1,
            node: "n".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: "u1.your-storagebox.de".into(),
            username: "u1".into(),
            password: "p".into(),
            removing: false,
            osd_id: None,
        };
        assert_eq!(r.share(), "//u1.your-storagebox.de/backup");
        assert!(r.is_provisioned());
    }
}
