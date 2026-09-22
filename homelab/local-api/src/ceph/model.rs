use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Deserializer};

use crate::exec::{self, CmdError};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct OsdDump {
    pub osds: Vec<OsdDumpEntry>,
    pub pools: Vec<PoolEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct OsdDumpEntry {
    pub osd: i64,
    #[serde(deserialize_with = "flag")]
    pub up: bool,
    #[serde(rename = "in", deserialize_with = "flag")]
    pub is_in: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PoolEntry {
    pub pool: i64,
    pub pool_name: String,
    #[serde(default)]
    pub size: u32,
    #[serde(default)]
    pub min_size: u32,
}

fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let n = i64::deserialize(d)?;
    match n {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(serde::de::Error::custom(format!(
            "expected 0 or 1, got {other}"
        ))),
    }
}

impl OsdDump {
    pub fn down(&self) -> BTreeSet<i64> {
        self.osds.iter().filter(|o| !o.up).map(|o| o.osd).collect()
    }

    pub fn pool_names(&self) -> HashMap<i64, String> {
        self.pools
            .iter()
            .map(|p| (p.pool, p.pool_name.clone()))
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PgBrief {
    pub pgid: String,
    pub state: String,
    #[serde(default)]
    pub up: Vec<i64>,
    #[serde(default)]
    pub acting: Vec<i64>,
}

impl PgBrief {
    pub fn pool(&self) -> Option<i64> {
        pool_of(&self.pgid)
    }

    pub fn holders(&self) -> &[i64] {
        if self.acting.is_empty() {
            &self.up
        } else {
            &self.acting
        }
    }
}

pub fn pool_of(pgid: &str) -> Option<i64> {
    pgid.split('.').next()?.parse().ok()
}

pub fn parse_pgs_brief(cmd: &str, raw: &str) -> Result<Vec<PgBrief>, CmdError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Wrapped { pg_stats: Vec<PgBrief> },
        Bare(Vec<PgBrief>),
    }
    match exec::parse_json::<Shape>(cmd, raw)? {
        Shape::Wrapped { pg_stats } => Ok(pg_stats),
        Shape::Bare(v) => Ok(v),
    }
}

pub type PgsByPool = BTreeMap<String, BTreeSet<String>>;

pub fn lost_pgs(dump: &OsdDump, pgs: &[PgBrief]) -> (PgsByPool, BTreeSet<i64>) {
    let down = dump.down();
    let names = dump.pool_names();
    let mut lost = PgsByPool::new();
    let mut holders = BTreeSet::new();
    if down.is_empty() {
        return (lost, holders);
    }
    for pg in pgs {
        let on = pg.holders();
        if on.is_empty() || !on.iter().all(|id| down.contains(id)) {
            continue;
        }
        let Some(pool) = pg.pool().and_then(|id| names.get(&id)) else {
            continue;
        };
        lost.entry(pool.clone())
            .or_default()
            .insert(pg.pgid.clone());
        holders.extend(on.iter().copied());
    }
    (lost, holders)
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct OsdStat {
    pub num_osds: u64,
    pub num_up_osds: u64,
    pub num_in_osds: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FsEntry {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SafeToDestroyReport {
    pub safe_to_destroy: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LvmVolume {
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(default, rename = "type")]
    pub kind: String,
}

impl LvmVolume {
    #[cfg(test)]
    pub fn cluster_fsid(&self) -> Option<&str> {
        self.tags.get("ceph.cluster_fsid").map(String::as_str)
    }
}

pub fn parse_lvm_list(raw: &str) -> Result<BTreeMap<i64, Vec<LvmVolume>>, CmdError> {
    const CMD: &str = "ceph-volume lvm list --format json";
    let start = raw
        .find('{')
        .ok_or_else(|| CmdError::parse(CMD, "no JSON object in output"))?;
    let map: BTreeMap<String, Vec<LvmVolume>> = exec::parse_json(CMD, &raw[start..])?;
    map.into_iter()
        .map(|(k, v)| {
            k.parse::<i64>()
                .map(|id| (id, v))
                .map_err(|_| CmdError::parse(CMD, format!("OSD key {k:?} is not a number")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"{
        "flags": "noout,sortbitwise",
        "osds": [{"osd":0,"up":1,"in":1},{"osd":1,"up":0,"in":1}],
        "pools": [{"pool":1,"pool_name":".mgr","size":1,"min_size":1},
                  {"pool":3,"pool_name":"yolab-fs-data0","size":2,"min_size":1}]
    }"#;

    #[test]
    fn osd_dump_parses_up_in_and_pools() {
        let d: OsdDump = serde_json::from_str(DUMP).unwrap();
        assert_eq!(d.down(), BTreeSet::from([1]));
    }

    #[test]
    fn an_osd_dump_without_osds_is_an_error_not_an_empty_cluster() {
        assert!(serde_json::from_str::<OsdDump>(r#"{"pools": []}"#).is_err());
    }

    #[test]
    fn an_up_flag_that_is_not_zero_or_one_is_refused() {
        let raw = r#"{"osds":[{"osd":0,"up":7,"in":1}],"pools":[]}"#;
        assert!(serde_json::from_str::<OsdDump>(raw).is_err());
    }

    #[test]
    fn pgs_brief_accepts_both_shapes() {
        let bare = r#"[{"pgid":"1.0","state":"active+clean","up":[0],"acting":[0]}]"#;
        let wrapped =
            r#"{"pg_stats":[{"pgid":"1.0","state":"active+clean","up":[0],"acting":[0]}]}"#;
        assert_eq!(parse_pgs_brief("x", bare).unwrap().len(), 1);
        assert_eq!(parse_pgs_brief("x", wrapped).unwrap().len(), 1);
        assert!(parse_pgs_brief("x", r#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn a_group_is_lost_only_when_every_holder_is_down() {
        let d: OsdDump = serde_json::from_str(DUMP).unwrap();
        let pgs = parse_pgs_brief(
            "x",
            r#"[
              {"pgid":"3.0","state":"down","up":[1],"acting":[1]},
              {"pgid":"3.1","state":"active+degraded","up":[0,1],"acting":[0,1]},
              {"pgid":"1.0","state":"unknown","up":[0],"acting":[0]},
              {"pgid":"3.2","state":"unknown","up":[],"acting":[]}
            ]"#,
        )
        .unwrap();
        let (lost, holders) = lost_pgs(&d, &pgs);
        assert_eq!(
            lost.get("yolab-fs-data0").cloned(),
            Some(BTreeSet::from(["3.0".to_string()]))
        );
        assert_eq!(lost.len(), 1);
        assert_eq!(holders, BTreeSet::from([1]));
    }

    #[test]
    fn lvm_list_skips_progress_lines_but_never_reads_garbage_as_empty() {
        let raw = "--> some progress\n{\"3\":[{\"devices\":[\"/dev/sdb\"],\"tags\":{\"ceph.cluster_fsid\":\"abc\"}}]}";
        let m = parse_lvm_list(raw).unwrap();
        assert_eq!(m[&3][0].cluster_fsid(), Some("abc"));
        assert!(parse_lvm_list("").is_err());
        assert!(parse_lvm_list("--> no json here").is_err());
        assert!(parse_lvm_list("{\"x\": []}").is_err());
        assert!(parse_lvm_list("{}").unwrap().is_empty());
    }
}
