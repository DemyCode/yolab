use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::exec::{self, CmdError};

pub const LVS_ARGS: &[&str] = &[
    "--reportformat",
    "json",
    "--readonly",
    "-a",
    "--units",
    "b",
    "--nosuffix",
    "-o",
    "lv_tags,lv_path,lv_name,vg_name,lv_uuid,lv_size,devices",
];

#[derive(Deserialize)]
struct Report {
    report: Vec<Section>,
}

#[derive(Deserialize)]
struct Section {
    #[serde(default)]
    lv: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    lv_tags: String,
    lv_path: String,
    lv_name: String,
    vg_name: String,
    lv_uuid: String,
    lv_size: String,
    devices: String,
}

fn tags(raw: &str) -> BTreeMap<String, String> {
    raw.split(',')
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn devices(raw: &str) -> impl Iterator<Item = String> + '_ {
    raw.split(',')
        .map(|d| d.split_once('(').map_or(d, |(path, _)| path).trim())
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

struct Volume {
    row: Row,
    tags: BTreeMap<String, String>,
    devices: Vec<String>,
}

pub fn from_lvs(raw: &str) -> Result<String, CmdError> {
    let cmd = exec::render("lvs", LVS_ARGS);
    let report: Report = exec::parse_json(&cmd, raw)?;
    let mut volumes: BTreeMap<String, Volume> = BTreeMap::new();
    for row in report.report.into_iter().flat_map(|s| s.lv) {
        let tags = tags(&row.lv_tags);
        if !tags.contains_key("ceph.osd_id") {
            continue;
        }
        let more: Vec<String> = devices(&row.devices).collect();
        let v = volumes.entry(row.lv_uuid.clone()).or_insert_with(|| Volume {
            row,
            tags,
            devices: Vec::new(),
        });
        for d in more {
            if !v.devices.contains(&d) {
                v.devices.push(d);
            }
        }
    }
    let mut osds: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for v in volumes.into_values() {
        let id = v.tags["ceph.osd_id"].clone();
        let kind = v.tags.get("ceph.type").cloned().unwrap_or_default();
        osds.entry(id).or_default().push(json!({
            "devices": v.devices,
            "lv_name": v.row.lv_name,
            "lv_path": v.row.lv_path,
            "lv_size": v.row.lv_size,
            "lv_tags": v.row.lv_tags,
            "lv_uuid": v.row.lv_uuid,
            "name": v.row.lv_name,
            "path": v.row.lv_path,
            "tags": v.tags,
            "type": kind,
            "vg_name": v.row.vg_name,
        }));
    }
    serde_json::to_string(&osds).map_err(|e| CmdError::parse(cmd, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE1_LVS: &str = r#"  {
      "report": [
          {
              "lv": [
                  {"lv_tags":"ceph.block_device=/dev/ceph-ce76/osd-block-5564,ceph.cephx_lockbox_secret=,ceph.cluster_fsid=8c1f4018-f20b-4149-919c-7c96aec52197,ceph.osd_fsid=5564,ceph.osd_id=2,ceph.type=block", "lv_path":"/dev/ceph-ce76/osd-block-5564", "lv_name":"osd-block-5564", "vg_name":"ceph-ce76", "lv_uuid":"83Bs", "lv_size":"1000169537536", "devices":"/dev/sdb(0)"},
                  {"lv_tags":"ceph.block_device=/dev/pool/ceph,ceph.cephx_lockbox_secret=,ceph.cluster_fsid=a926fc81-09ec-4c90-a08a-52837b5e7be7,ceph.osd_fsid=474c,ceph.osd_id=0,ceph.type=block", "lv_path":"/dev/pool/ceph", "lv_name":"ceph", "vg_name":"pool", "lv_uuid":"tU6d", "lv_size":"191629361152", "devices":"/dev/sda2(15360)"},
                  {"lv_tags":"", "lv_path":"/dev/pool/root", "lv_name":"root", "vg_name":"pool", "lv_uuid":"FgSd", "lv_size":"64424509440", "devices":"/dev/sda2(0)"}
              ]
          }
      ]
      ,
      "log": [
      ]
  }"#;

    fn listed(raw: &str) -> Value {
        serde_json::from_str(&from_lvs(raw).unwrap()).unwrap()
    }

    #[test]
    fn node1s_osds_come_out_as_ceph_volume_lists_them() {
        let v = listed(NODE1_LVS);
        assert_eq!(
            v["0"],
            json!([{
                "devices": ["/dev/sda2"],
                "lv_name": "ceph",
                "lv_path": "/dev/pool/ceph",
                "lv_size": "191629361152",
                "lv_tags": "ceph.block_device=/dev/pool/ceph,ceph.cephx_lockbox_secret=,ceph.cluster_fsid=a926fc81-09ec-4c90-a08a-52837b5e7be7,ceph.osd_fsid=474c,ceph.osd_id=0,ceph.type=block",
                "lv_uuid": "tU6d",
                "name": "ceph",
                "path": "/dev/pool/ceph",
                "tags": {
                    "ceph.block_device": "/dev/pool/ceph",
                    "ceph.cephx_lockbox_secret": "",
                    "ceph.cluster_fsid": "a926fc81-09ec-4c90-a08a-52837b5e7be7",
                    "ceph.osd_fsid": "474c",
                    "ceph.osd_id": "0",
                    "ceph.type": "block"
                },
                "type": "block",
                "vg_name": "pool"
            }])
        );
        assert_eq!(v["2"][0]["devices"], json!(["/dev/sdb"]));
        assert_eq!(
            v["2"][0]["tags"]["ceph.cluster_fsid"],
            "8c1f4018-f20b-4149-919c-7c96aec52197"
        );
    }

    #[test]
    fn volumes_without_ceph_tags_are_left_out() {
        let v = listed(NODE1_LVS);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn the_existing_parsers_read_the_converted_listing() {
        let raw = from_lvs(NODE1_LVS).unwrap();
        let by_id = crate::ceph::model::parse_lvm_list(&raw).unwrap();
        assert_eq!(by_id.keys().copied().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(by_id[&0][0].kind, "block");
        assert_eq!(by_id[&2][0].devices, vec!["/dev/sdb".to_string()]);
    }

    #[test]
    fn a_volume_spread_over_several_disks_is_one_entry_with_every_disk() {
        let raw = r#"{"report":[{"lv":[
            {"lv_tags":"ceph.osd_id=4,ceph.type=block","lv_path":"/dev/vg/osd","lv_name":"osd","vg_name":"vg","lv_uuid":"u1","lv_size":"10","devices":"/dev/sdc(0)"},
            {"lv_tags":"ceph.osd_id=4,ceph.type=block","lv_path":"/dev/vg/osd","lv_name":"osd","vg_name":"vg","lv_uuid":"u1","lv_size":"10","devices":"/dev/sdd(0),/dev/sdc(512)"}
        ]}]}"#;
        let v = listed(raw);
        assert_eq!(v["4"].as_array().unwrap().len(), 1);
        assert_eq!(v["4"][0]["devices"], json!(["/dev/sdc", "/dev/sdd"]));
    }

    #[test]
    fn a_db_volume_is_listed_beside_its_block_volume() {
        let raw = r#"{"report":[{"lv":[
            {"lv_tags":"ceph.osd_id=1,ceph.type=block","lv_path":"/dev/vg/b","lv_name":"b","vg_name":"vg","lv_uuid":"b","lv_size":"10","devices":"/dev/sdb(0)"},
            {"lv_tags":"ceph.osd_id=1,ceph.type=db","lv_path":"/dev/vg/d","lv_name":"d","vg_name":"vg","lv_uuid":"d","lv_size":"5","devices":"/dev/nvme0n1(0)"}
        ]}]}"#;
        let v = listed(raw);
        let kinds: Vec<&str> = v["1"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["block", "db"]);
    }

    #[test]
    fn no_volumes_is_an_empty_listing() {
        assert_eq!(from_lvs(r#"{"report":[{"lv":[]}]}"#).unwrap(), "{}");
    }

    #[test]
    fn output_that_is_not_an_lvs_report_is_an_error() {
        assert!(from_lvs("").is_err());
        assert!(from_lvs(r#"{"lv":[]}"#).is_err());
        assert!(from_lvs(r#"{"report":[{"lv":[{"lv_path":"/dev/x"}]}]}"#).is_err());
    }
}
