use std::cmp::Ordering;

pub(crate) const OFFSET_STEP: f64 = 0.02;
pub(crate) const ENUMERATE_MAX: usize = 50_000;
pub(crate) const WORK_MAX: f64 = 2e8;

pub(crate) fn doubling_cost(times: &[i64]) -> f64 {
    let mut newest_first = times.to_vec();
    newest_first.sort_unstable_by_key(|t| std::cmp::Reverse(*t));
    let off: Vec<f64> = newest_first
        .windows(2)
        .enumerate()
        .map(|(k, w)| ((w[0] - w[1]).max(1) as f64).log2() - (k + 1) as f64)
        .collect();
    if off.is_empty() {
        return 0.0;
    }
    let mean = off.iter().sum::<f64>() / off.len() as f64;
    off.iter().map(|v| (v - mean).powi(2)).sum()
}

#[derive(Debug, Clone)]
struct Scored {
    cost: f64,
    oldest: i64,
    picked: Vec<usize>,
}

fn scored(times: &[i64], picked: Vec<usize>) -> Scored {
    let chosen: Vec<i64> = picked.iter().map(|&i| times[i]).collect();
    Scored {
        cost: doubling_cost(&chosen),
        oldest: chosen.iter().copied().min().unwrap_or(i64::MAX),
        picked,
    }
}

fn better(a: &Scored, b: &Scored) -> Ordering {
    let by_cost = if (a.cost - b.cost).abs() <= 1e-9 {
        Ordering::Equal
    } else {
        a.cost.total_cmp(&b.cost)
    };
    by_cost
        .then(a.oldest.cmp(&b.oldest))
        .then_with(|| b.picked.cmp(&a.picked))
}

fn combinations(n: usize, k: usize) -> usize {
    let mut c: u128 = 1;
    for i in 0..k.min(n) {
        c = c * (n - i) as u128 / (i + 1) as u128;
        if c > ENUMERATE_MAX as u128 {
            return usize::MAX;
        }
    }
    c as usize
}

fn every_removal(removable: &[usize], k: usize) -> Vec<Vec<usize>> {
    if k == 0 {
        return vec![Vec::new()];
    }
    if k > removable.len() {
        return Vec::new();
    }
    let mut every = Vec::new();
    let mut idx: Vec<usize> = (0..k).collect();
    loop {
        every.push(idx.iter().map(|&i| removable[i]).collect());
        let mut i = k;
        loop {
            i -= 1;
            if idx[i] != i + removable.len() - k {
                break;
            }
            if i == 0 {
                return every;
            }
        }
        idx[i] += 1;
        for j in (i + 1)..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

pub(crate) fn compact(forced: &[bool], m: usize) -> Option<Vec<usize>> {
    let must = forced.iter().filter(|f| **f).count();
    let mut spare = m.checked_sub(must)?;
    let picked: Vec<usize> = (0..forced.len())
        .filter(|&i| {
            if forced[i] {
                return true;
            }
            if spare == 0 {
                return false;
            }
            spare -= 1;
            true
        })
        .collect();
    (picked.len() == m).then_some(picked)
}

fn shaped(times: &[i64], forced: &[bool], m: usize, take: usize) -> Vec<Vec<usize>> {
    let n = times.len();
    let removable: Vec<usize> = (0..n).filter(|&i| !forced[i]).collect();
    let k = n - m;
    if combinations(removable.len(), k) <= ENUMERATE_MAX {
        let mut all: Vec<Scored> = Vec::new();
        for gone in every_removal(&removable, k) {
            let picked: Vec<usize> = (0..n).filter(|i| !gone.contains(i)).collect();
            all.push(scored(times, picked));
        }
        all.sort_by(better);
        return all.into_iter().take(take).map(|s| s.picked).collect();
    }
    if search_work(times, m) > WORK_MAX {
        return Vec::new();
    }
    best_subset(times, forced, m).into_iter().collect()
}

pub(crate) fn ranked(times: &[i64], forced: &[bool], m: usize, take: usize) -> Vec<Vec<usize>> {
    let n = times.len();
    if m >= n {
        return vec![(0..n).collect()];
    }
    let Some(newest) = compact(forced, m) else {
        return Vec::new();
    };
    if m == 0 {
        return Vec::new();
    }
    let mut out = if take == 0 {
        Vec::new()
    } else {
        shaped(times, forced, m, take)
    };
    if !out.contains(&newest) {
        out.push(newest);
    }
    out
}

fn search_work(times: &[i64], m: usize) -> f64 {
    let n = times.len();
    if n < 2 {
        return 0.0;
    }
    let span = ((times[0] - times[n - 1]).max(1) as f64).log2();
    let smallest = times
        .windows(2)
        .map(|w| ((w[0] - w[1]).max(1) as f64).log2())
        .fold(f64::INFINITY, f64::min);
    let steps = (span - smallest + m as f64) / OFFSET_STEP + 1.0;
    steps * m as f64 * (n * n) as f64
}

fn subset_for_offset(
    log_gap: &[Vec<f64>],
    next_forced: &[usize],
    m: usize,
    a: f64,
) -> Option<Vec<usize>> {
    let n = next_forced.len();
    let mut dp = vec![f64::INFINITY; n];
    dp[0] = 0.0;
    let mut back: Vec<Vec<usize>> = Vec::with_capacity(m);
    for k in 1..m {
        let mut nd = vec![f64::INFINITY; n];
        let mut bk = vec![usize::MAX; n];
        for i in 0..n {
            if !dp[i].is_finite() {
                continue;
            }
            for j in (i + 1)..=next_forced[i].min(n - 1) {
                let r = log_gap[i][j] - k as f64 - a;
                let v = dp[i] + r * r;
                if v < nd[j] {
                    nd[j] = v;
                    bk[j] = i;
                }
            }
        }
        back.push(bk);
        dp = nd;
    }
    (0..n)
        .filter(|&j| next_forced[j] == n && dp[j].is_finite())
        .min_by(|&x, &y| dp[x].total_cmp(&dp[y]))
        .map(|end| {
            let mut j = end;
            let mut picked = vec![j];
            for k in (1..m).rev() {
                j = back[k - 1][j];
                picked.push(j);
            }
            picked.reverse();
            picked
        })
}

pub(crate) fn best_subset(times: &[i64], forced: &[bool], m: usize) -> Option<Vec<usize>> {
    let n = times.len();
    if m >= n {
        return Some((0..n).collect());
    }
    let newest_forced = forced.first().copied().unwrap_or(false);
    if m == 0 || !newest_forced || forced.iter().filter(|f| **f).count() > m {
        return None;
    }
    if m == 1 {
        return Some(vec![0]);
    }
    let mut next_forced = vec![n; n];
    let mut nearest = n;
    for i in (0..n).rev() {
        next_forced[i] = nearest;
        if forced[i] {
            nearest = i;
        }
    }
    let log_gap: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            (0..n)
                .map(|j| {
                    if j > i {
                        ((times[i] - times[j]).max(1) as f64).log2()
                    } else {
                        0.0
                    }
                })
                .collect()
        })
        .collect();
    let smallest = (1..n)
        .map(|i| log_gap[i - 1][i])
        .fold(f64::INFINITY, f64::min);
    let lo = smallest - m as f64;
    let hi = log_gap[0][n - 1];
    let steps = ((hi - lo) / OFFSET_STEP).ceil().max(0.0) as usize;
    (0..=steps)
        .map(|s| lo + s as f64 * OFFSET_STEP)
        .filter_map(|a| subset_for_offset(&log_gap, &next_forced, m, a))
        .map(|picked| scored(times, picked))
        .min_by(better)
        .map(|s| s.picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn nights(ages: &[i64]) -> Vec<i64> {
        ages.iter().map(|a| -a * DAY).collect()
    }

    fn only_newest(n: usize) -> Vec<bool> {
        (0..n).map(|i| i == 0).collect()
    }

    fn ages_kept(ages: &[i64], picked: &[usize]) -> Vec<i64> {
        picked.iter().map(|&i| ages[i]).collect()
    }

    #[test]
    fn perfect_spacing_doubles_every_gap_whatever_the_unit() {
        assert!(doubling_cost(&[0, -1, -3, -7, -15]) < 1e-12);
        assert!(doubling_cost(&[0, -60, -180, -420]) < 1e-12);
        assert!(
            doubling_cost(&[0, -1, -2, -3]) > 0.1,
            "equal gaps are not doubling"
        );
        assert_eq!(
            doubling_cost(&[0, -5]),
            0.0,
            "one gap has nothing to double"
        );
    }

    #[test]
    fn the_curve_fits_all_gaps_at_once_not_only_the_first_two() {
        let first_gap_off = doubling_cost(&nights(&[0, 3, 5, 9, 17]));
        let last_gap_off = doubling_cost(&nights(&[0, 1, 3, 7, 10]));
        assert!(first_gap_off > 0.0 && last_gap_off > 0.0);
        let shifted = doubling_cost(&nights(&[0, 2, 6, 14, 30]));
        assert!(
            shifted < 1e-12,
            "any scale fits: 2, 4, 8, 16 nights is perfect too"
        );
    }

    #[test]
    fn deleting_one_of_four_nightly_backups_leaves_gaps_of_one_and_two() {
        let ages = [0, 1, 2, 3];
        let best = &ranked(&nights(&ages), &only_newest(4), 3, 1)[0];
        assert_eq!(ages_kept(&ages, best), vec![0, 1, 3]);
    }

    #[test]
    fn hours_and_nights_give_the_same_choice() {
        let hours: Vec<i64> = [0i64, 1, 2, 3].iter().map(|a| -a * 3600).collect();
        assert_eq!(ranked(&hours, &only_newest(4), 3, 1)[0], vec![0, 1, 3]);
    }

    #[test]
    fn backups_already_doubling_lose_their_oldest() {
        let ages = [0, 1, 3, 7];
        let best = &ranked(&nights(&ages), &only_newest(4), 3, 1)[0];
        assert_eq!(ages_kept(&ages, best), vec![0, 1, 3]);
    }

    #[test]
    fn candidates_come_best_fit_first_and_all_of_them_are_offered() {
        let ages = [0, 1, 2, 3, 4];
        let times = nights(&ages);
        let all = ranked(&times, &only_newest(5), 4, usize::MAX);
        assert_eq!(all.len(), 4, "any of the four older backups can go");
        let costs: Vec<f64> = all
            .iter()
            .map(|p| doubling_cost(&p.iter().map(|&i| times[i]).collect::<Vec<_>>()))
            .collect();
        assert!(costs.windows(2).all(|w| w[0] <= w[1] + 1e-9));
    }

    #[test]
    fn the_newest_kept_and_older_style_backups_are_in_every_candidate() {
        let ages = [0, 1, 2, 3, 4, 5, 6];
        let forced = vec![true, false, false, true, false, false, true];
        for m in 3..7 {
            for picked in ranked(&nights(&ages), &forced, m, usize::MAX) {
                assert!(picked.contains(&0) && picked.contains(&3) && picked.contains(&6));
                assert_eq!(picked.len(), m);
            }
        }
        assert!(
            ranked(&nights(&ages), &forced, 2, 5).is_empty(),
            "three must stay"
        );
    }

    #[test]
    fn with_two_backups_left_the_one_reaching_furthest_back_stays() {
        let ages = [0, 1, 2, 5, 9];
        let best = &ranked(&nights(&ages), &only_newest(5), 2, 1)[0];
        assert_eq!(ages_kept(&ages, best), vec![0, 9]);
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, below: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % below
        }
    }

    fn brute_force(times: &[i64], forced: &[bool], m: usize) -> f64 {
        let n = times.len();
        let mut best = f64::INFINITY;
        for mask in 0u32..(1 << n) {
            if mask.count_ones() as usize != m {
                continue;
            }
            if (0..n).any(|i| forced[i] && mask & (1 << i) == 0) {
                continue;
            }
            let chosen: Vec<i64> = (0..n)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| times[i])
                .collect();
            best = best.min(doubling_cost(&chosen));
        }
        best
    }

    #[test]
    fn the_exact_search_finds_the_best_set_that_trying_every_set_finds() {
        let mut rng = Lcg(7);
        for _ in 0..40 {
            let n = 9;
            let mut age = 0;
            let mut ages = vec![0];
            for _ in 1..n {
                age += 1 + rng.next(20) as i64;
                ages.push(age);
            }
            let times = nights(&ages);
            let forced: Vec<bool> = (0..n).map(|i| i == 0 || rng.next(6) == 0).collect();
            let must = forced.iter().filter(|f| **f).count();
            for m in must.max(2)..n {
                let found = best_subset(&times, &forced, m).expect("a set exists");
                let cost = doubling_cost(&found.iter().map(|&i| times[i]).collect::<Vec<_>>());
                let bound = (m - 1) as f64 * (OFFSET_STEP / 2.0).powi(2) + 1e-9;
                assert!(
                    cost <= brute_force(&times, &forced, m) + bound,
                    "{ages:?} m={m}"
                );
                assert!((0..n).all(|i| !forced[i] || found.contains(&i)));
            }
        }
    }

    #[test]
    fn too_many_combinations_fall_back_to_the_exact_search_then_the_newest() {
        let ages: Vec<i64> = (0..60).collect();
        let picked = ranked(&nights(&ages), &only_newest(60), 8, 5);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].len(), 8);
        assert_eq!(picked[0][0], 0);
        assert_eq!(picked[1], (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn a_search_too_big_to_finish_offers_only_the_newest() {
        let ages: Vec<i64> = (0..400).collect();
        let picked = ranked(&nights(&ages), &only_newest(400), 200, 5);
        assert_eq!(picked, vec![(0..200).collect::<Vec<_>>()]);
    }

    #[test]
    fn the_newest_set_keeps_everything_that_must_stay_then_the_newest_others() {
        let forced = vec![true, false, false, true, false];
        assert_eq!(compact(&forced, 3), Some(vec![0, 1, 3]));
        assert_eq!(compact(&forced, 2), Some(vec![0, 3]));
        assert_eq!(compact(&forced, 1), None);
        assert_eq!(compact(&forced, 5), Some(vec![0, 1, 2, 3, 4]));
    }

    fn removals(removable: &[usize], k: usize) -> Vec<Vec<usize>> {
        every_removal(removable, k)
    }

    #[test]
    fn removing_nothing_is_one_way_and_removing_more_than_there_is_is_none() {
        assert_eq!(removals(&[3, 4], 0), vec![Vec::<usize>::new()]);
        assert_eq!(removals(&[3], 2), Vec::<Vec<usize>>::new());
        assert_eq!(removals(&[3, 4], 2), vec![vec![3, 4]]);
    }

    #[test]
    fn keeping_as_many_as_there_are_keeps_everything() {
        let times = nights(&[0, 1]);
        assert_eq!(ranked(&times, &only_newest(2), 2, 1), vec![vec![0, 1]]);
        assert_eq!(best_subset(&times, &only_newest(2), 3), Some(vec![0, 1]));
    }

    #[test]
    fn keeping_none_offers_nothing() {
        let times = nights(&[0, 1]);
        assert!(ranked(&times, &[false, false], 0, 1).is_empty());
        assert_eq!(best_subset(&times, &only_newest(2), 0), None);
    }

    #[test]
    fn asking_for_no_shaped_candidates_offers_only_the_newest() {
        let times = nights(&[0, 1, 2]);
        assert_eq!(ranked(&times, &only_newest(3), 2, 0), vec![vec![0, 1]]);
    }

    #[test]
    fn one_backup_has_no_search_to_do() {
        assert_eq!(search_work(&[0], 1), 0.0);
    }

    #[test]
    fn the_exact_search_needs_the_newest_kept_and_room_for_everything_that_must_stay() {
        let times = nights(&[0, 1, 2, 3]);
        assert_eq!(best_subset(&times, &[false, true, false, false], 2), None);
        assert_eq!(best_subset(&times, &[true, true, true, false], 2), None);
        assert_eq!(best_subset(&times, &only_newest(4), 1), Some(vec![0]));
    }

    #[test]
    fn the_newest_set_is_always_the_last_candidate_offered() {
        let ages = [0, 1, 2, 3, 4, 5];
        let all = ranked(&nights(&ages), &only_newest(6), 3, 2);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], vec![0, 1, 2]);
    }
}
