use std::collections::BTreeMap;

use serde_json::Value;

use crate::storage::overview::Osd;

pub(crate) const FULL_RATIO: f64 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spread {
    Disk,
    Machine,
}

pub(crate) fn spread_of(rules: &Value, detail: &Value, pool: &str) -> Spread {
    let rule_id = detail
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["pool_name"] == pool)
        .and_then(|p| p["crush_rule"].as_u64());
    let rule = rules
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| rule_id.is_some() && r["rule_id"].as_u64() == rule_id);
    let domain = rule.and_then(|r| {
        r["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["op"].as_str().is_some_and(|op| op.starts_with("choose")))
            .and_then(|s| s["type"].as_str())
    });
    match domain {
        Some("osd") => Spread::Disk,
        Some("host") => Spread::Machine,
        other => {
            tracing::warn!(
                "storage: {pool} spreads its copies over {other:?}; free space is counted per machine"
            );
            Spread::Machine
        }
    }
}

fn room(osd: &Osd) -> f64 {
    (osd.size_bytes as f64 * FULL_RATIO - osd.used_bytes as f64).max(0.0)
}

fn weight(osd: &Osd) -> f64 {
    osd.weight * osd.reweight
}

struct Domain {
    weight: f64,
    room: f64,
}

fn domains(osds: &[Osd], spread: Spread) -> Vec<Domain> {
    let mut grouped: BTreeMap<String, Vec<&Osd>> = BTreeMap::new();
    for osd in osds.iter().filter(|o| o.up && weight(o) > 0.0) {
        let key = match spread {
            Spread::Disk => osd.name.clone(),
            Spread::Machine => osd.host.clone(),
        };
        grouped.entry(key).or_default().push(osd);
    }
    grouped
        .into_values()
        .map(|disks| {
            let total: f64 = disks.iter().map(|&o| weight(o)).sum();
            let room = disks
                .iter()
                .map(|o| room(o) * total / weight(o))
                .fold(f64::INFINITY, f64::min);
            Domain {
                weight: total,
                room,
            }
        })
        .collect()
}

pub(crate) fn shares(weights: &[f64], copies: usize) -> Vec<f64> {
    let mut share = vec![0.0; weights.len()];
    let mut full = vec![false; weights.len()];
    loop {
        let left = copies as f64 - full.iter().filter(|f| **f).count() as f64;
        let open: f64 = weights
            .iter()
            .zip(&full)
            .filter(|(_, f)| !**f)
            .map(|(w, _)| w)
            .sum();
        if open <= 0.0 {
            break;
        }
        let scale = left / open;
        let mut capped = false;
        for (i, w) in weights.iter().enumerate() {
            if full[i] {
                share[i] = 1.0;
            } else if w * scale >= 1.0 {
                full[i] = true;
                share[i] = 1.0;
                capped = true;
            } else {
                share[i] = w * scale;
            }
        }
        if !capped {
            break;
        }
    }
    share
}

pub(crate) fn free_for_new_data(osds: &[Osd], copies: u32, spread: Spread) -> u64 {
    let copies = copies.max(1) as usize;
    let domains = domains(osds, spread);
    if domains.len() < copies {
        return 0;
    }
    let weights: Vec<f64> = domains.iter().map(|d| d.weight).collect();
    let free = domains
        .iter()
        .zip(shares(&weights, copies))
        .filter(|(_, share)| *share > 0.0)
        .map(|(d, share)| d.room / share)
        .fold(f64::INFINITY, f64::min);
    if free.is_finite() {
        free.floor() as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GB: u64 = 1_000_000_000;

    fn disk(name: &str, host: &str, size: u64, used: u64) -> Osd {
        Osd {
            id: 0,
            name: name.into(),
            host: host.into(),
            class: String::new(),
            size_bytes: size,
            used_bytes: used,
            avail_bytes: size - used,
            utilization: 0.0,
            var: 0.0,
            pgs: 0,
            up: true,
            weight: size as f64 / 1e12,
            reweight: 1.0,
        }
    }

    #[test]
    fn two_copies_on_two_disks_fit_only_what_the_smaller_disk_has_left() {
        let osds = [
            disk("osd.0", "node1", 192 * GB, 43 * GB),
            disk("osd.1", "node1", 1000 * GB, 43 * GB),
        ];
        let free = free_for_new_data(&osds, 2, Spread::Disk) as f64;
        let smaller_left = 192.0 * GB as f64 * FULL_RATIO - 43.0 * GB as f64;
        assert!(
            (free - smaller_left).abs() < 2.0,
            "{free} vs {smaller_left}"
        );
    }

    #[test]
    fn many_equal_disks_give_what_ceph_itself_estimates() {
        let osds = [
            disk("osd.0", "a", 100 * GB, 0),
            disk("osd.1", "b", 100 * GB, 0),
            disk("osd.2", "c", 100 * GB, 0),
        ];
        let ceph = (95 * GB) as f64 / (1.0 / 3.0) / 2.0;
        let free = free_for_new_data(&osds, 2, Spread::Disk) as f64;
        assert!((free - ceph).abs() < 2.0, "{free} vs {ceph}");
    }

    #[test]
    fn one_copy_fills_every_disk_in_proportion_to_its_size() {
        let osds = [
            disk("osd.0", "a", 100 * GB, 0),
            disk("osd.1", "a", 900 * GB, 0),
        ];
        let free = free_for_new_data(&osds, 1, Spread::Disk) as f64;
        assert!((free - 950.0 * GB as f64).abs() < 2.0, "{free}");
    }

    #[test]
    fn more_copies_than_places_for_them_leaves_no_room() {
        let osds = [
            disk("osd.0", "node1", 100 * GB, 0),
            disk("osd.1", "node1", 100 * GB, 0),
        ];
        assert_eq!(free_for_new_data(&osds, 2, Spread::Machine), 0);
    }

    #[test]
    fn inside_one_machine_its_disks_fill_by_their_size() {
        let osds = [
            disk("osd.0", "node1", 100 * GB, 0),
            disk("osd.1", "node1", 300 * GB, 0),
            disk("osd.2", "node2", 400 * GB, 0),
        ];
        let free = free_for_new_data(&osds, 2, Spread::Machine) as f64;
        assert!((free - 380.0 * GB as f64).abs() < 2.0, "{free}");
    }

    #[test]
    fn a_disk_that_is_down_or_out_holds_nothing_new() {
        let mut down = disk("osd.2", "node3", 1000 * GB, 0);
        down.up = false;
        let mut out = disk("osd.3", "node4", 1000 * GB, 0);
        out.reweight = 0.0;
        let osds = [
            disk("osd.0", "node1", 100 * GB, 0),
            disk("osd.1", "node2", 100 * GB, 0),
            down,
            out,
        ];
        let free = free_for_new_data(&osds, 2, Spread::Machine) as f64;
        assert!((free - 95.0 * GB as f64).abs() < 2.0, "{free}");
    }

    #[test]
    fn a_disk_already_past_the_full_line_has_no_room() {
        let osds = [
            disk("osd.0", "a", 100 * GB, 99 * GB),
            disk("osd.1", "b", 100 * GB, 0),
        ];
        assert_eq!(free_for_new_data(&osds, 2, Spread::Disk), 0);
    }

    #[test]
    fn a_big_place_takes_at_most_one_copy_and_the_rest_spread_over_the_others() {
        let s = shares(&[8.0, 1.0, 1.0], 2);
        assert_eq!(s, vec![1.0, 0.5, 0.5]);
    }

    #[test]
    fn shares_add_up_to_the_number_of_copies() {
        let s = shares(&[5.0, 3.0, 1.0, 1.0], 3);
        assert!((s.iter().sum::<f64>() - 3.0).abs() < 1e-9, "{s:?}");
        assert!(s.iter().all(|x| *x <= 1.0), "{s:?}");
    }

    fn rules() -> Value {
        json!([
            {"rule_id": 0, "rule_name": "replicated_rule", "steps": [
                {"op": "take", "item": -1, "item_name": "default"},
                {"op": "chooseleaf_firstn", "num": 0, "type": "host"},
                {"op": "emit"}
            ]},
            {"rule_id": 1, "rule_name": "replicated_osd", "steps": [
                {"op": "take", "item": -1, "item_name": "default"},
                {"op": "choose_firstn", "num": 0, "type": "osd"},
                {"op": "emit"}
            ]}
        ])
    }

    #[test]
    fn copies_spread_over_what_the_pools_rule_chooses() {
        let per_disk = json!([{"pool_name": "data", "crush_rule": 1}]);
        let per_machine = json!([{"pool_name": "data", "crush_rule": 0}]);
        assert_eq!(spread_of(&rules(), &per_disk, "data"), Spread::Disk);
        assert_eq!(spread_of(&rules(), &per_machine, "data"), Spread::Machine);
    }

    #[test]
    fn an_unknown_rule_counts_per_machine_which_never_promises_more() {
        let unknown = json!([{"pool_name": "data", "crush_rule": 9}]);
        assert_eq!(spread_of(&rules(), &unknown, "data"), Spread::Machine);
    }
}
