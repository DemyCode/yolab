//! Typed shapes impl OsdDump {
f the Ceph JSON this crate acts on.
//!
//! Before this, every reader indexed a `serde_json::Value`:
//! `v["num_up_osds"].as_u64().unwrap_or(0)`. A renamed field, a Ceph release
//! that nests the list one level deeper, or a truncated answer all produced the
//! same silent zero — and zero is an ANSWER ("no OSDs up", "no pools", "cluster
//! empty") that callers act on.
//!
//! Here a field the decision depends on is required: if Ceph stops sending it
//! the parse fails, `?` propagates, and the caller does nothing this tick. Only
//! fields Ceph genuinely omits in normal operation carry `#[serde(default)]`,
//! and each says why.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Deserializer};

use crate::exec::{self, CmdError};

// ── ceph osd dump ────────────────────────────────────────────────────────────

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

/// Ceph encodes up/in as 0/1 integers.
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
    pub fn up(&self) -> BTreeSet<i64> {
        self.osds.iter().filter(|o| o.up).map(|o| o.osd).collect()
    }

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

// ── ceph pg dump pgs_brief ───────────────────────────────────────────────────

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

    /// Where the data lives: the acting set, or the up set when Ceph has not
    /// filled in acting. Empty when neither is known.
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

/// `pgs_brief` is a bare array on some releases and `{"pg_stats": [...]}` on
/// others. Both are accepted; anything else is a parse error.
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

/// Placement groups whose every holder is down, by pool name, and those holders.
///
/// Read while the OSDs are still `in`: that is when the acting set still names
/// where the data was. A group Ceph merely has no statistics for (`unknown`,
/// right after a mgr restart) still maps to its live holders, so it never counts.
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

// ── small answers ────────────────────────────────────────────────────────────

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

/// `ceph osd safe-to-destroy osd.N -f json` when Ceph agrees. When it does not,
/// ceph exits EBUSY instead, which `exec::classify` reports as `Failure::Busy`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SafeToDestroyReport {
    pub safe_to_destroy: Vec<i64>,
}

// ── ceph-volume lvm list ─────────────────────────────────────────────────────

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

/// OSD id → its volumes. ceph-volume prints `-->` progress lines before the
/// JSON, so anything before the first `{` is discarded. No `{` at all is a
/// parse error, never "no OSDs": the whole safety of the disk reconciler rests
/// on never reading a failed listing as an empty one.
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
