use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backup::common::*;
use crate::error::Outcome;
use crate::host::Host;
use crate::records::Store;
use crate::routers::folder_backup::{self, FOLDERS_TAG};

pub(crate) const SETTINGS_REPO: &str = "cluster-backup";
pub(crate) const DEFAULT_BUDGET: u64 = 10_000_000_000;
const PRUNE_TIMEOUT: Duration = Duration::from_secs(3600);
const STATS_TIMEOUT: Duration = Duration::from_secs(900);

pub(crate) const SIZES: Store = Store {
    name: "yolab-backup-sizes",
    namespace: "kube-system",
    key: "services",
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct ServiceSize {
    pub size_bytes: u64,
    pub newest_bytes: u64,
    pub budget_bytes: u64,
    pub over_budget: bool,
    #[serde(default)]
    pub added: BTreeMap<String, u64>,
    pub updated_at: String,
}

pub(crate) type Sizes = BTreeMap<String, ServiceSize>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Point {
    pub snapshot_id: String,
    pub time: String,
    pub run: Option<String>,
}

impl Point {
    pub(crate) fn legacy(&self) -> bool {
        self.run.is_none()
    }
}

fn tags_of(s: &Value) -> Vec<String> {
    s["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect()
}

pub(crate) fn namespace_tag(namespace: &str) -> String {
    format!("namespace:{namespace}")
}

pub(crate) fn points_of(snapshots: &Value) -> Vec<Point> {
    let mut points: Vec<(chrono::DateTime<Utc>, Point)> = snapshots
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let tags = tags_of(s);
            if tags.iter().any(|t| t == "scope:cluster") {
                return None;
            }
            let raw = s["time"].as_str()?;
            let time = chrono::DateTime::parse_from_rfc3339(raw)
                .ok()?
                .with_timezone(&Utc);
            Some((
                time,
                Point {
                    snapshot_id: s["id"].as_str()?.to_string(),
                    time: raw.to_string(),
                    run: folder_backup::run_of(&tags).map(str::to_string),
                },
            ))
        })
        .collect();
    points.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    points.into_iter().map(|(_, p)| p).collect()
}

pub(crate) type FolderSnapshots = BTreeMap<String, Vec<String>>;

pub(crate) fn folder_snapshots_of(snapshots: &Value) -> FolderSnapshots {
    let mut by_run: FolderSnapshots = BTreeMap::new();
    for s in snapshots.as_array().into_iter().flatten() {
        let tags = tags_of(s);
        let (Some(run), Some(id)) = (folder_backup::run_of(&tags), s["id"].as_str()) else {
            continue;
        };
        by_run
            .entry(run.to_string())
            .or_default()
            .push(id.to_string());
    }
    by_run
}

pub(crate) fn orphans(points: &[Point], folders: &FolderSnapshots) -> Vec<String> {
    let runs: HashSet<&str> = points.iter().filter_map(|p| p.run.as_deref()).collect();
    folders
        .iter()
        .filter(|(run, _)| !runs.contains(run.as_str()))
        .flat_map(|(_, ids)| ids.iter().cloned())
        .collect()
}

pub(crate) fn still_there(ids: Vec<String>, there: &HashSet<String>) -> Vec<String> {
    ids.into_iter().filter(|id| there.contains(id)).collect()
}

fn seconds(point: &Point) -> anyhow::Result<i64> {
    Ok(chrono::DateTime::parse_from_rfc3339(&point.time)
        .map_err(|e| {
            anyhow::anyhow!(
                "backup {} has time {:?}: {e}",
                point.snapshot_id,
                point.time
            )
        })?
        .timestamp())
}

pub(crate) const CANDIDATES_PER_COUNT: usize = 6;
pub(crate) const FILL_PERCENT: u64 = 90;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fit {
    pub drop: Vec<String>,
    pub size_after: u64,
    pub over: bool,
}

struct Sizer<'a, F> {
    order: &'a [(&'a Point, i64)],
    measure: F,
    known: BTreeMap<Vec<usize>, u64>,
}

impl<F, Fut> Sizer<'_, F>
where
    F: FnMut(Vec<Point>) -> Fut,
    Fut: Future<Output = anyhow::Result<u64>>,
{
    async fn size(&mut self, picked: &[usize]) -> anyhow::Result<u64> {
        if let Some(size) = self.known.get(picked) {
            return Ok(*size);
        }
        let kept: Vec<Point> = picked
            .iter()
            .map(|&i| self.order[i].0)
            .filter(|p| !p.legacy())
            .cloned()
            .collect();
        let size = (self.measure)(kept).await?;
        self.known.insert(picked.to_vec(), size);
        Ok(size)
    }

    async fn first_fit(
        &mut self,
        times: &[i64],
        forced: &[bool],
        m: usize,
        target: u64,
    ) -> anyhow::Result<Option<(Vec<usize>, u64)>> {
        for picked in crate::backup::spacing::ranked(times, forced, m, CANDIDATES_PER_COUNT) {
            let size = self.size(&picked).await?;
            if size <= target {
                return Ok(Some((picked, size)));
            }
        }
        Ok(None)
    }
}

pub(crate) async fn fit<F, Fut>(points: &[Point], budget: u64, measure: F) -> anyhow::Result<Fit>
where
    F: FnMut(Vec<Point>) -> Fut,
    Fut: Future<Output = anyhow::Result<u64>>,
{
    let mut order: Vec<(&Point, i64)> = points
        .iter()
        .map(|p| Ok((p, seconds(p)?)))
        .collect::<anyhow::Result<_>>()?;
    order.sort_by_key(|a| std::cmp::Reverse(a.1));
    let n = order.len();
    let times: Vec<i64> = order.iter().map(|(_, t)| *t).collect();
    let forced: Vec<bool> = order
        .iter()
        .enumerate()
        .map(|(i, (p, _))| i == 0 || p.legacy())
        .collect();
    let mut sizer = Sizer {
        order: &order,
        measure,
        known: BTreeMap::new(),
    };
    let all: Vec<usize> = (0..n).collect();
    let whole = sizer.size(&all).await?;
    let keep_all = Fit {
        drop: Vec::new(),
        size_after: whole,
        over: whole > budget,
    };
    if whole <= budget {
        return Ok(keep_all);
    }
    let floor: Vec<usize> = (0..n).filter(|&i| forced[i]).collect();
    let floor_size = sizer.size(&floor).await?;
    if floor_size > budget {
        return Ok(Fit {
            drop: (0..n)
                .filter(|i| !forced[*i])
                .map(|i| order[i].0.snapshot_id.clone())
                .collect(),
            size_after: floor_size,
            over: true,
        });
    }
    let filled = budget / 100 * FILL_PERCENT + budget % 100 * FILL_PERCENT / 100;
    let target = if floor_size <= filled { filled } else { budget };

    let mut chosen = (floor.clone(), floor_size);
    match sizer.first_fit(&times, &forced, n - 1, target).await? {
        Some(found) => chosen = found,
        None => {
            let (mut lo, mut hi) = (floor.len(), n - 2);
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                match sizer.first_fit(&times, &forced, mid, target).await? {
                    Some(found) => {
                        lo = mid;
                        chosen = found;
                    }
                    None => hi = mid - 1,
                }
            }
            for m in (lo + 1)..=(n - 2) {
                match sizer.first_fit(&times, &forced, m, target).await? {
                    Some(found) => chosen = found,
                    None => break,
                }
            }
        }
    }
    Ok(Fit {
        drop: (0..n)
            .filter(|i| !chosen.0.contains(i))
            .map(|i| order[i].0.snapshot_id.clone())
            .collect(),
        size_after: chosen.1,
        over: chosen.1 > budget,
    })
}

pub(crate) fn folder_ids(points: &[Point], folders: &FolderSnapshots) -> Vec<String> {
    points
        .iter()
        .filter_map(|p| p.run.as_deref())
        .filter_map(|run| folders.get(run))
        .flatten()
        .cloned()
        .collect()
}

pub(crate) fn raw_size(stats: &Value) -> anyhow::Result<u64> {
    stats["total_size"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("restic stats did not say how big the backup is"))
}

pub(crate) struct Service<'a, H: Host> {
    pub host: &'a H,
    pub cfg: &'a BackupConfig,
    pub namespace: &'a str,
}

impl<H: Host> Service<'_, H> {
    fn store(&self) -> String {
        folder_backup::store(self.namespace)
    }

    async fn restic(&self, repo: &str, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let out = restic_with(
            self.host,
            &self.cfg.restic_repo(repo),
            self.cfg,
            args,
            timeout,
        )
        .await?;
        if !out.success {
            anyhow::bail!(
                "restic {}: {}",
                args.first().unwrap_or(&""),
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    async fn listed(&self, repo: &str, tag: Option<&str>) -> anyhow::Result<Value> {
        let mut args = vec!["snapshots", "--no-lock", "--json"];
        if let Some(tag) = tag {
            args.extend(["--tag", tag]);
        }
        let out = restic_with(
            self.host,
            &self.cfg.restic_repo(repo),
            self.cfg,
            &args,
            RESTIC_TIMEOUT,
        )
        .await?;
        if !out.success {
            if repo_missing(&out.stderr) {
                return Ok(Value::Array(vec![]));
            }
            anyhow::bail!("restic snapshots: {}", out.stderr.trim());
        }
        Ok(serde_json::from_str(&out.stdout)?)
    }

    pub(crate) async fn points(&self) -> anyhow::Result<Vec<Point>> {
        Ok(points_of(
            &self
                .listed(SETTINGS_REPO, Some(&namespace_tag(self.namespace)))
                .await?,
        ))
    }

    pub(crate) async fn folders(&self) -> anyhow::Result<FolderSnapshots> {
        Ok(folder_snapshots_of(
            &self.listed(&self.store(), Some(FOLDERS_TAG)).await?,
        ))
    }

    async fn existing(&self, repo: &str) -> anyhow::Result<HashSet<String>> {
        Ok(self
            .listed(repo, None)
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["id"].as_str().map(str::to_string))
            .collect())
    }

    pub(crate) async fn measure(&self, ids: &[String]) -> anyhow::Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut args = vec!["stats", "--no-lock", "--mode", "raw-data", "--json"];
        args.extend(ids.iter().map(String::as_str));
        raw_size(&serde_json::from_str(
            &self.restic(&self.store(), &args, STATS_TIMEOUT).await?,
        )?)
    }

    async fn measure_points(
        &self,
        points: &[Point],
        folders: &FolderSnapshots,
    ) -> anyhow::Result<u64> {
        self.measure(&folder_ids(points, folders)).await
    }

    async fn forget(&self, repo: &str, ids: &[String]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut args = vec!["forget"];
        args.extend(ids.iter().map(String::as_str));
        self.restic(repo, &args, RESTIC_TIMEOUT).await.map(|_| ())
    }

    pub(crate) async fn delete(
        &self,
        points: &[Point],
        folders: &FolderSnapshots,
    ) -> anyhow::Result<BTreeSet<String>> {
        let mut touched = BTreeSet::new();
        for point in points {
            for (repo, ids) in crate::backup::restore::older_volume_copies(
                self.host,
                self.cfg,
                &point.snapshot_id,
                self.namespace,
            )
            .await?
            {
                let there = self.existing(&repo).await?;
                let ids = still_there(ids, &there);
                if !ids.is_empty() {
                    self.forget(&repo, &ids).await?;
                    touched.insert(repo);
                }
            }
            self.forget(SETTINGS_REPO, std::slice::from_ref(&point.snapshot_id))
                .await?;
            touched.insert(SETTINGS_REPO.to_string());
            if let Some(ids) = point.run.as_deref().and_then(|r| folders.get(r)) {
                self.forget(&self.store(), ids).await?;
                touched.insert(self.store());
            }
        }
        Ok(touched)
    }

    pub(crate) async fn drop_orphans(
        &self,
        points: &[Point],
        folders: &FolderSnapshots,
    ) -> anyhow::Result<bool> {
        let left = orphans(points, folders);
        self.forget(&self.store(), &left).await?;
        Ok(!left.is_empty())
    }

    pub(crate) async fn frees(&self, snapshot_id: &str) -> anyhow::Result<Option<u64>> {
        let points = self.points().await?;
        let Some(point) = points.iter().find(|p| p.snapshot_id == snapshot_id) else {
            anyhow::bail!("{snapshot_id} is not a backup of {}", self.namespace);
        };
        if point.legacy() {
            return Ok(None);
        }
        let folders = self.folders().await?;
        let now = self.measure_points(&points, &folders).await?;
        let rest: Vec<Point> = points
            .iter()
            .filter(|p| p.snapshot_id != snapshot_id)
            .cloned()
            .collect();
        let after = self.measure_points(&rest, &folders).await?;
        Ok(Some(now.saturating_sub(after)))
    }

    pub(crate) async fn sized(
        &self,
        budget: Option<u64>,
        previous: Option<&ServiceSize>,
    ) -> anyhow::Result<ServiceSize> {
        let points = self.points().await?;
        let folders = self.folders().await?;
        let fresh: Vec<Point> = points.iter().filter(|p| !p.legacy()).cloned().collect();
        let size = self.measure_points(&fresh, &folders).await?;
        let newest = match fresh.first() {
            Some(p) => {
                self.measure_points(std::slice::from_ref(p), &folders)
                    .await?
            }
            None => 0,
        };
        let budget_bytes = budget.unwrap_or(DEFAULT_BUDGET);
        let mut known: BTreeMap<String, u64> =
            previous.map(|p| p.added.clone()).unwrap_or_default();
        known.retain(|id, _| points.iter().any(|p| &p.snapshot_id == id));
        Ok(ServiceSize {
            size_bytes: size,
            newest_bytes: newest,
            budget_bytes,
            over_budget: size > budget_bytes,
            added: known,
            updated_at: Utc::now().to_rfc3339(),
        })
    }
}

fn repo_missing(stderr: &str) -> bool {
    stderr.contains("unable to open config file") || stderr.contains("does not exist")
}

static PRUNING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub(crate) async fn prune<H: Host>(host: &H, cfg: &BackupConfig, repo: &str) {
    {
        let mut busy = PRUNING
            .lock()
            .expect("the prune registry is never poisoned");
        if !busy.insert(repo.to_string()) {
            tracing::info!("backup: {repo} is already being cleaned up");
            return;
        }
    }
    cfg.unlock(host, repo).await;
    let out = restic_with(host, &cfg.restic_repo(repo), cfg, &["prune"], PRUNE_TIMEOUT).await;
    match out {
        Ok(o) if o.success => tracing::info!("backup: cleaned up {repo}"),
        Ok(o) => tracing::warn!(
            "backup: clean-up of {repo} failed, the next one retries: {}",
            o.stderr.trim()
        ),
        Err(e) => {
            tracing::warn!("backup: clean-up of {repo} could not run, the next one retries: {e}")
        }
    }
    PRUNING
        .lock()
        .expect("the prune registry is never poisoned")
        .remove(repo);
}

pub(crate) async fn read_sizes(client: &kube::Client) -> anyhow::Result<Sizes> {
    Ok(SIZES.read(client).await?)
}

pub(crate) async fn record_size(client: &kube::Client, namespace: &str, size: Option<ServiceSize>) {
    SIZES
        .update(client, |all: &mut Sizes| match &size {
            Some(s) => {
                all.insert(namespace.to_string(), s.clone());
            }
            None => {
                all.remove(namespace);
            }
        })
        .await
        .warn_on_err(format!("{namespace}: record the size of its backups"));
}

pub(crate) async fn budget_of(
    client: &kube::Client,
    namespace: &str,
) -> anyhow::Result<Option<u64>> {
    Ok(crate::routers::apps::read_definition_opt(client, namespace)
        .await?
        .and_then(|d| d.backup.budget_bytes))
}

pub(crate) struct AfterRun {
    pub size: ServiceSize,
    pub dropped: usize,
}

static THINNING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

struct Thinning(String);

impl Thinning {
    fn claim(namespace: &str) -> anyhow::Result<Self> {
        let mut busy = THINNING
            .lock()
            .expect("the thinning registry is never poisoned");
        if !busy.insert(namespace.to_string()) {
            anyhow::bail!("{namespace} is already being pruned; try again once it is done");
        }
        Ok(Thinning(namespace.to_string()))
    }
}

impl Drop for Thinning {
    fn drop(&mut self) {
        THINNING
            .lock()
            .expect("the thinning registry is never poisoned")
            .remove(&self.0);
    }
}

pub(crate) async fn after_run<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    namespace: &str,
    new_point: &str,
) -> anyhow::Result<AfterRun> {
    let _claim = Thinning::claim(namespace)?;
    let svc = Service {
        host: &b.host,
        cfg,
        namespace,
    };
    let previous = read_sizes(&b.kube).await?.remove(namespace);
    let points = svc.points().await?;
    let folders = svc.folders().await?;
    let had_orphans = svc.drop_orphans(&points, &folders).await?;

    let fresh: Vec<Point> = points.iter().filter(|p| !p.legacy()).cloned().collect();
    let with_new = svc.measure_points(&fresh, &folders).await?;
    let without: Vec<Point> = fresh
        .iter()
        .filter(|p| p.snapshot_id != new_point)
        .cloned()
        .collect();
    let without = svc.measure_points(&without, &folders).await?;
    let added = with_new.saturating_sub(without);
    let newest = match fresh.first() {
        Some(p) => {
            svc.measure_points(std::slice::from_ref(p), &folders)
                .await?
        }
        None => 0,
    };
    let budget = budget_of(&b.kube, namespace)
        .await?
        .unwrap_or(DEFAULT_BUDGET);

    let planned = fit(&points, budget, |kept| {
        let ids = folder_ids(&kept, &folders);
        let svc = &svc;
        async move { svc.measure(&ids).await }
    })
    .await?;
    let doomed: Vec<Point> = points
        .iter()
        .filter(|p| planned.drop.contains(&p.snapshot_id))
        .cloned()
        .collect();
    let mut touched = svc.delete(&doomed, &folders).await?;
    if had_orphans {
        touched.insert(svc.store());
    }
    for repo in &touched {
        prune(&b.host, cfg, repo).await;
    }
    let mut known: BTreeMap<String, u64> = previous.map(|p| p.added).unwrap_or_default();
    known.retain(|id, _| points.iter().any(|p| &p.snapshot_id == id) && !planned.drop.contains(id));
    known.insert(new_point.to_string(), added);
    let size = ServiceSize {
        size_bytes: planned.size_after,
        newest_bytes: newest,
        budget_bytes: budget,
        over_budget: planned.over,
        added: known,
        updated_at: Utc::now().to_rfc3339(),
    };
    record_size(&b.kube, namespace, Some(size.clone())).await;
    Ok(AfterRun {
        size,
        dropped: doomed.len(),
    })
}

pub(crate) fn spawn_cleanup<H: Host + Clone + 'static>(
    b: Backend<H>,
    cfg: BackupConfig,
    namespace: String,
    touched: BTreeSet<String>,
) {
    tokio::spawn(async move {
        for repo in &touched {
            prune(&b.host, &cfg, repo).await;
        }
        refresh(&b, &cfg, &namespace).await;
    });
}

pub(crate) async fn delete_point<H: Host + Clone + 'static>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    namespace: &str,
    snapshot_id: &str,
) -> anyhow::Result<()> {
    let svc = Service {
        host: &b.host,
        cfg,
        namespace,
    };
    let points = svc.points().await?;
    let Some(point) = points.iter().find(|p| p.snapshot_id == snapshot_id) else {
        anyhow::bail!("{snapshot_id} is not a backup of {namespace}");
    };
    let folders = svc.folders().await?;
    let touched = svc.delete(std::slice::from_ref(point), &folders).await?;
    tracing::info!("backup: deleted {snapshot_id} of {namespace}");
    spawn_cleanup(b.clone(), cfg.clone(), namespace.to_string(), touched);
    Ok(())
}

pub(crate) async fn delete_all<H: Host + Clone + 'static>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    namespace: &str,
) -> anyhow::Result<usize> {
    let svc = Service {
        host: &b.host,
        cfg,
        namespace,
    };
    let points = svc.points().await?;
    let folders = svc.folders().await?;
    let mut touched = svc.delete(&points, &folders).await?;
    if svc.drop_orphans(&points, &folders).await? {
        touched.insert(svc.store());
    }
    tracing::info!(
        "backup: deleted all {} backups of {namespace}",
        points.len()
    );
    spawn_cleanup(b.clone(), cfg.clone(), namespace.to_string(), touched);
    Ok(points.len())
}

pub(crate) async fn refresh<H: Host>(b: &Backend<H>, cfg: &BackupConfig, namespace: &str) {
    let svc = Service {
        host: &b.host,
        cfg,
        namespace,
    };
    let previous = match read_sizes(&b.kube).await {
        Ok(mut all) => all.remove(namespace),
        Err(e) => {
            tracing::warn!("{namespace}: the recorded backup sizes are unreadable: {e:#}");
            None
        }
    };
    let budget = match budget_of(&b.kube, namespace).await {
        Ok(budget) => budget,
        Err(e) => {
            tracing::warn!("{namespace}: its budget could not be read: {e:#}");
            return;
        }
    };
    match svc.sized(budget, previous.as_ref()).await {
        Ok(size) if size.size_bytes == 0 && size.added.is_empty() => {
            record_size(&b.kube, namespace, None).await
        }
        Ok(size) => record_size(&b.kube, namespace, Some(size)).await,
        Err(e) => {
            tracing::warn!("{namespace}: the size of its backups could not be measured: {e:#}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn point(id: &str, run: Option<&str>) -> Point {
        Point {
            snapshot_id: id.into(),
            time: "2026-11-16T03:00:00Z".into(),
            run: run.map(str::to_string),
        }
    }

    fn sized<'a>(
        sizes: &'a [(&'a str, u64)],
    ) -> impl FnMut(Vec<Point>) -> std::future::Ready<anyhow::Result<u64>> + 'a {
        move |kept: Vec<Point>| {
            let total = kept
                .iter()
                .map(|p| {
                    sizes
                        .iter()
                        .find(|(id, _)| *id == p.snapshot_id)
                        .map(|(_, s)| *s)
                        .unwrap_or(0)
                })
                .sum();
            std::future::ready(Ok(total))
        }
    }

    #[test]
    fn points_are_read_newest_first_without_the_nightly_cluster_snapshot() {
        let snapshots = json!([
            {"id": "old", "time": "2026-11-14T03:00:00Z", "tags": ["cluster-backup", "namespace:yolab-a"]},
            {"id": "dr", "time": "2026-11-16T04:00:00Z", "tags": ["cluster-backup", "scope:cluster"]},
            {"id": "new", "time": "2026-11-16T03:00:00Z", "tags": ["cluster-backup", "namespace:yolab-a", "run:bk-2"]},
            {"id": "broken"},
        ]);
        let points = points_of(&snapshots);
        assert_eq!(
            points,
            vec![
                Point {
                    snapshot_id: "new".into(),
                    time: "2026-11-16T03:00:00Z".into(),
                    run: Some("bk-2".into()),
                },
                Point {
                    snapshot_id: "old".into(),
                    time: "2026-11-14T03:00:00Z".into(),
                    run: None,
                },
            ]
        );
        assert!(
            points[1].legacy(),
            "a point from before the folder stores has no run"
        );
    }

    #[test]
    fn folder_snapshots_are_grouped_by_the_run_that_saved_them() {
        let snapshots = json!([
            {"id": "f1", "tags": ["yolab-folders", "run:bk-1"]},
            {"id": "f2", "tags": ["yolab-folders", "run:bk-2"]},
            {"id": "f3", "tags": ["yolab-folders"]},
        ]);
        let by_run = folder_snapshots_of(&snapshots);
        assert_eq!(by_run["bk-1"], vec!["f1".to_string()]);
        assert_eq!(by_run.len(), 2);
    }

    #[test]
    fn folders_no_restore_point_names_are_orphans() {
        let points = vec![point("p1", Some("bk-1"))];
        let folders = FolderSnapshots::from([
            ("bk-1".into(), vec!["f1".into()]),
            ("bk-0".into(), vec!["f0".into()]),
        ]);
        assert_eq!(orphans(&points, &folders), vec!["f0".to_string()]);
    }

    #[test]
    fn a_service_cannot_be_pruned_twice_at_once() {
        let first = Thinning::claim("yolab-twice-at-once").unwrap();
        let err = Thinning::claim("yolab-twice-at-once").err().unwrap();
        assert!(err.to_string().contains("already being pruned"), "{err}");
        drop(first);
    }

    #[test]
    fn a_service_can_be_pruned_again_once_the_last_prune_ends() {
        drop(Thinning::claim("yolab-again-after").unwrap());
        assert!(Thinning::claim("yolab-again-after").is_ok());
    }

    #[test]
    fn pruning_one_service_does_not_hold_up_another() {
        let _one = Thinning::claim("yolab-prune-one").unwrap();
        assert!(Thinning::claim("yolab-prune-two").is_ok());
    }

    #[test]
    fn a_service_with_no_space_of_its_own_gets_ten_gigabytes() {
        assert_eq!(DEFAULT_BUDGET, 10_000_000_000);
    }

    fn aged(id: &str, hours_ago: i64, legacy: bool) -> Point {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-16T03:00:00Z").unwrap();
        Point {
            snapshot_id: id.into(),
            time: (now - chrono::Duration::hours(hours_ago)).to_rfc3339(),
            run: (!legacy).then(|| format!("run-{id}")),
        }
    }

    fn nightly(count: i64) -> Vec<Point> {
        (0..count)
            .map(|n| aged(&format!("n{n}"), 24 * n, false))
            .collect()
    }

    async fn dropped(points: &[Point], budget: u64) -> Vec<String> {
        let each: Vec<(&str, u64)> = points
            .iter()
            .map(|p| (p.snapshot_id.as_str(), 10))
            .collect();
        fit(points, budget, sized(&each)).await.unwrap().drop
    }

    #[tokio::test]
    async fn under_budget_every_backup_stays() {
        let points = nightly(5);
        let each: Vec<(&str, u64)> = points
            .iter()
            .map(|p| (p.snapshot_id.as_str(), 10))
            .collect();
        let fitted = fit(&points, 50, sized(&each)).await.unwrap();
        assert_eq!(
            fitted,
            Fit {
                drop: vec![],
                size_after: 50,
                over: false
            }
        );
    }

    #[tokio::test]
    async fn over_budget_the_backup_that_breaks_the_doubling_most_goes() {
        assert_eq!(
            dropped(&nightly(4), 34).await,
            vec!["n2"],
            "leaves gaps of 1 and 2 nights"
        );
    }

    #[tokio::test]
    async fn the_choice_is_the_same_in_hours_as_in_nights() {
        let hourly: Vec<Point> = (0..4).map(|h| aged(&format!("h{h}"), h, false)).collect();
        assert_eq!(dropped(&hourly, 34).await, vec!["h2"]);
    }

    #[tokio::test]
    async fn backups_already_doubling_lose_their_oldest() {
        let spaced = [
            aged("a", 0, false),
            aged("b", 24, false),
            aged("c", 72, false),
            aged("d", 168, false),
        ];
        assert_eq!(dropped(&spaced, 34).await, vec!["d"]);
    }

    #[tokio::test]
    async fn cleaning_up_goes_down_to_ninety_percent_so_the_next_backup_has_room() {
        let gone = dropped(&nightly(10), 99).await;
        assert_eq!(gone.len(), 2, "one would leave 90 of 99, above 89");
    }

    #[tokio::test]
    async fn the_most_backups_that_fit_win_over_a_better_doubling() {
        let points = nightly(4);
        let sizes = [("n0", 10), ("n1", 30), ("n2", 10), ("n3", 10)];
        let fitted = fit(&points, 50, sized(&sizes)).await.unwrap();
        assert_eq!(
            fitted.drop,
            vec!["n1"],
            "0,1,3 nights doubles perfectly but is 50; 0,2,3 is 30"
        );
        assert_eq!(fitted.size_after, 30);
    }

    #[tokio::test]
    async fn nightly_backups_thin_out_into_doubling_gaps() {
        let nights = nightly(16);
        let gone = dropped(&nights, 56).await;
        let left: Vec<i64> = nights
            .iter()
            .filter(|p| !gone.contains(&p.snapshot_id))
            .map(|p| seconds(p).unwrap())
            .collect();
        assert_eq!(left.len(), 5);
        let all: Vec<i64> = nights.iter().map(|p| seconds(p).unwrap()).collect();
        assert!(
            crate::backup::spacing::doubling_cost(&left)
                < crate::backup::spacing::doubling_cost(&all[..5]),
            "better than keeping the 5 newest"
        );
        assert!(left.contains(&all[15]) || left.iter().min() < Some(&all[4]));
    }

    #[tokio::test]
    async fn an_older_style_backup_counts_in_the_spacing_but_never_goes() {
        let points = [
            aged("new", 0, false),
            aged("mid", 24, false),
            aged("legacy", 48, true),
            aged("old", 72, false),
        ];
        let gone = dropped(&points, 25).await;
        assert_eq!(gone, vec!["old".to_string()]);
    }

    #[tokio::test]
    async fn when_the_newest_copy_alone_does_not_fit_every_other_copy_goes() {
        let points = [
            aged("big", 0, false),
            aged("a", 24, false),
            aged("b", 48, false),
        ];
        let fitted = fit(&points, 50, sized(&[("big", 80), ("a", 10), ("b", 10)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(fitted.size_after, 80);
        assert!(fitted.over);
    }

    #[tokio::test]
    async fn a_newest_copy_too_big_for_its_space_still_stays() {
        let points = [aged("big", 0, false), aged("a", 24, false)];
        let fitted = fit(&points, 10, sized(&[("big", 167), ("a", 1)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["a".to_string()]);
        assert!(fitted.over);
    }

    #[tokio::test]
    async fn older_style_copies_stay_even_when_the_newest_alone_does_not_fit() {
        let points = [
            aged("big", 0, false),
            aged("a", 24, false),
            aged("old", 48, true),
        ];
        let fitted = fit(&points, 50, sized(&[("big", 80), ("a", 10), ("old", 10)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["a".to_string()]);
        assert!(fitted.over);
    }

    #[tokio::test]
    async fn the_budget_is_the_line_when_what_must_stay_is_above_ninety_percent() {
        let points = [aged("new", 0, false), aged("extra", 24, false)];
        let fitted = fit(&points, 50, sized(&[("new", 48), ("extra", 10)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["extra".to_string()]);
        assert_eq!(fitted.size_after, 48);
        assert!(!fitted.over);
    }

    #[tokio::test]
    async fn a_backup_with_an_unreadable_time_stops_the_cleanup_instead_of_guessing() {
        let mut bad = aged("bad", 24, false);
        bad.time = "yesterday".into();
        let points = [aged("new", 0, false), bad];
        assert!(fit(&points, 0, sized(&[("new", 10), ("bad", 10)]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_same_set_is_measured_once() {
        let points = nightly(12);
        let mut seen: Vec<Vec<String>> = Vec::new();
        let seen_ref = &mut seen;
        fit(&points, 35, move |kept: Vec<Point>| {
            seen_ref.push(kept.iter().map(|p| p.snapshot_id.clone()).collect());
            std::future::ready(anyhow::Ok(kept.len() as u64 * 10))
        })
        .await
        .unwrap();
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len());
    }

    #[tokio::test]
    async fn the_cleanup_never_keeps_fewer_than_keeping_only_the_newest_would() {
        let points = nightly(7);
        let sizes: Vec<(&str, u64)> = points
            .iter()
            .zip([5u64, 30, 12, 8, 20, 3, 15])
            .map(|(p, s)| (p.snapshot_id.as_str(), s))
            .collect();
        for budget in [40u64, 55, 70, 85] {
            let fitted = fit(&points, budget, sized(&sizes)).await.unwrap();
            let target = budget * FILL_PERCENT / 100;
            let newest_that_fit = (1..=7)
                .filter(|&m| sizes[..m].iter().map(|(_, s)| s).sum::<u64>() <= target)
                .max()
                .unwrap();
            assert!(7 - fitted.drop.len() >= newest_that_fit, "budget {budget}");
            assert!(fitted.size_after <= target);
        }
    }

    #[test]
    fn a_service_measures_only_the_folders_of_its_restore_points() {
        let folders = FolderSnapshots::from([
            ("r1".into(), vec!["f1".into()]),
            ("r2".into(), vec!["f2".into()]),
        ]);
        let points = vec![point("p2", Some("r2")), point("legacy", None)];
        assert_eq!(folder_ids(&points, &folders), vec!["f2".to_string()]);
    }

    #[test]
    fn a_volume_copy_its_own_retention_already_removed_is_not_forgotten_again() {
        let there: HashSet<String> = ["a".to_string()].into();
        assert_eq!(
            still_there(vec!["a".into(), "gone".into()], &there),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn restic_stats_must_say_the_size() {
        assert_eq!(
            raw_size(&json!({"total_size": 42, "snapshots_count": 1})).unwrap(),
            42
        );
        assert!(raw_size(&json!({})).is_err());
    }
}
