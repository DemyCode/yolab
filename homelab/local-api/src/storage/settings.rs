
use std::collections::BTreeMap;

use serde::{de::DeserializeOwned, Serialize};

use crate::exec::CmdError;
use crate::host::Host;

pub const DISKS: &str = "yolab/disks/";
pub const DISK_STATUS: &str = "yolab/disk-status/";
pub const STORAGE_POLICY: &str = "yolab/storage-policy";

pub async fn get<H: Host>(host: &H, key: &str) -> Result<Option<String>, CmdError> {
    match host.ceph(&["config-key", "get", key]).await {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e),
    }
}

pub async fn set<H: Host>(host: &H, key: &str, value: &str) -> Result<(), CmdError> {
    host.ceph(&["config-key", "set", key, value])
        .await
        .map(|_| ())
}

pub async fn dump<H: Host>(host: &H, prefix: &str) -> Result<BTreeMap<String, String>, CmdError> {
    let v = host.ceph_json(&["config-key", "dump", prefix]).await?;
    strip_prefix(&v, prefix).map_err(|detail| CmdError::parse("ceph config-key dump", detail))
}

fn strip_prefix(v: &serde_json::Value, prefix: &str) -> Result<BTreeMap<String, String>, String> {
    let object = v.as_object().ok_or("not a JSON object")?;
    object
        .iter()
        .filter_map(|(k, val)| k.strip_prefix(prefix).map(|rest| (rest, val)))
        .map(|(rest, val)| {
            val.as_str()
                .map(|s| (rest.to_string(), s.to_string()))
                .ok_or_else(|| format!("value of {prefix}{rest} is not a string"))
        })
        .collect()
}

pub async fn get_json<H: Host, T: DeserializeOwned>(
    host: &H,
    key: &str,
) -> Result<Option<T>, CmdError> {
    match get(host, key).await? {
        None => Ok(None),
        Some(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| CmdError::parse(format!("ceph config-key get {key}"), e)),
    }
}

pub async fn set_json<H: Host, T: Serialize>(
    host: &H,
    key: &str,
    value: &T,
) -> Result<(), CmdError> {
    let raw =
        serde_json::to_string(value).map_err(|e| CmdError::parse(format!("serialise {key}"), e))?;
    set(host, key, &raw).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[tokio::test]
    async fn a_missing_key_is_absent_and_an_unreachable_cluster_is_an_error() {
        let absent = FakeHost::new().fail(
            "ceph config-key get yolab/storage-policy",
            "Error ENOENT: key 'yolab/storage-policy' doesn't exist",
        );
        assert_eq!(get(&absent, STORAGE_POLICY).await.unwrap(), None);

        let down = FakeHost::new().fail(
            "ceph config-key get yolab/storage-policy",
            "error connecting to the cluster",
        );
        assert!(get(&down, STORAGE_POLICY).await.is_err());
    }

    #[tokio::test]
    async fn json_values_round_trip_and_junk_is_an_error() {
        let host = FakeHost::new()
            .ok("ceph config-key get yolab/storage-policy", r#"{"size":2}"#)
            .ok("ceph config-key set yolab/storage-policy", "");
        let v: serde_json::Value = get_json(&host, STORAGE_POLICY).await.unwrap().unwrap();
        assert_eq!(v["size"], 2);
        set_json(&host, STORAGE_POLICY, &serde_json::json!({"size": 3}))
            .await
            .unwrap();
        assert!(host.ran(r#"ceph config-key set yolab/storage-policy {"size":3}"#));

        let junk = FakeHost::new().ok("ceph config-key get yolab/storage-policy", "{nope");
        assert!(get_json::<_, serde_json::Value>(&junk, STORAGE_POLICY)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_dump_returns_the_keys_under_a_prefix_without_it() {
        let host = FakeHost::new().ok(
            "ceph config-key dump yolab/disks/",
            r#"{"yolab/disks/node1--dev-sda":"OFF","yolab/disks/serial-wwn-0x1":"ON"}"#,
        );
        let disks = dump(&host, DISKS).await.unwrap();
        assert_eq!(disks.get("node1--dev-sda").map(String::as_str), Some("OFF"));
        assert_eq!(disks.get("serial-wwn-0x1").map(String::as_str), Some("ON"));
        assert_eq!(disks.len(), 2);
    }

    #[test]
    fn a_dump_ignores_sibling_keys_and_rejects_the_wrong_shape() {
        let v = serde_json::json!({"yolab/disks/a": "ON", "yolab/disks-old/b": "OFF"});
        assert_eq!(strip_prefix(&v, DISKS).unwrap().len(), 1);
        assert!(strip_prefix(&serde_json::json!([]), DISKS).is_err());
        assert!(strip_prefix(&serde_json::json!({"yolab/disks/a": 1}), DISKS).is_err());
    }

    #[tokio::test]
    async fn an_empty_store_is_an_empty_map_not_an_error() {
        let host = FakeHost::new().ok("ceph config-key dump yolab/disk-status/", "{}");
        assert!(dump(&host, DISK_STATUS).await.unwrap().is_empty());
    }
}
