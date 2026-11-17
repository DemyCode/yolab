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
use crate::routers::folder_backup::{self, FOLDERS_TAG, KEEP_TAG};

pub(crate) const SETTINGS_REPO: &str = "cluster-backup";
pub(crate) const MIN_BUDGET: u64 = 1 << 30;
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
    pub keep: bool,
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
                    keep: tags.iter().any(|t| t == KEEP_TAG),
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

pub(crate) fn default_budget(newest_bytes: u64) -> u64 {
    newest_bytes.saturating_mul(2).max(MIN_BUDGET)
}

pub(crate) fn droppable(points: &[Point]) -> Vec<&Point> {
    points
        .iter()
        .skip(1)
        .filter(|p| !p.keep && !p.legacy())
        .rev()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fit {
    pub drop: Vec<String>,
    pub size_after: u64,
    pub over: bool,
}

pub(crate) async fn fit<F, Fut>(
    points: &[Point],
    budget: u64,
    mut measure: F,
) -> anyhow::Result<Fit>
where
    F: FnMut(Vec<Point>) -> Fut,
    Fut: Future<Output = anyhow::Result<u64>>,
{
    let mut kept: Vec<Point> = points.iter().filter(|p| !p.legacy()).cloned().collect();
    let mut size = measure(kept.clone()).await?;
    let mut drop = Vec::new();
    for candidate in droppable(points) {
        if size <= budget {
            break;
        }
        kept.retain(|p| p.snapshot_id != candidate.snapshot_id);
        drop.push(candidate.snapshot_id.clone());
        size = measure(kept.clone()).await?;
    }
    Ok(Fit {
        drop,
        size_after: size,
        over: size > budget,
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
            for (repo, ids) in crate::backup::restore::legacy_volumes(
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

    pub(crate) async fn set_keep(&self, snapshot_id: &str, keep: bool) -> anyhow::Result<()> {
        let flag = if keep { "--add" } else { "--remove" };
        self.restic(
            SETTINGS_REPO,
            &["tag", flag, KEEP_TAG, snapshot_id],
            RESTIC_TIMEOUT,
        )
        .await
        .map(|_| ())
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
        let budget_bytes = budget.unwrap_or_else(|| default_budget(newest));
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

pub(crate) async fn after_run<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    namespace: &str,
    new_point: &str,
) -> anyhow::Result<AfterRun> {
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
    let added = with_new.saturating_sub(svc.measure_points(&without, &folders).await?);
    let newest = match fresh.first() {
        Some(p) => {
            svc.measure_points(std::slice::from_ref(p), &folders)
                .await?
        }
        None => 0,
    };
    let budget = budget_of(&b.kube, namespace)
        .await?
        .unwrap_or_else(|| default_budget(newest));

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

pub(crate) fn retagged<'a>(before: &Point, after: &'a [Point]) -> Option<&'a Point> {
    after.iter().find(|p| match &before.run {
        Some(run) => p.run.as_ref() == Some(run),
        None => p.run.is_none() && p.time == before.time,
    })
}

pub(crate) async fn keep<H: Host>(
    b: &Backend<H>,
    cfg: &BackupConfig,
    namespace: &str,
    snapshot_id: &str,
    keep: bool,
) -> anyhow::Result<String> {
    let svc = Service {
        host: &b.host,
        cfg,
        namespace,
    };
    let points = svc.points().await?;
    let Some(before) = points.iter().find(|p| p.snapshot_id == snapshot_id) else {
        anyhow::bail!("{snapshot_id} is not a backup of {namespace}");
    };
    if before.keep == keep {
        return Ok(snapshot_id.to_string());
    }
    svc.set_keep(snapshot_id, keep).await?;
    let after = svc.points().await?;
    let Some(now) = retagged(before, &after) else {
        anyhow::bail!("{snapshot_id} was retagged but cannot be found again");
    };
    let new_id = now.snapshot_id.clone();
    SIZES
        .update(&b.kube, |all: &mut Sizes| {
            if let Some(size) = all.get_mut(namespace) {
                if let Some(added) = size.added.remove(snapshot_id) {
                    size.added.insert(new_id.clone(), added);
                }
            }
        })
        .await?;
    Ok(new_id)
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

    fn point(id: &str, run: Option<&str>, keep: bool) -> Point {
        Point {
            snapshot_id: id.into(),
            time: "2026-11-16T03:00:00Z".into(),
            run: run.map(str::to_string),
            keep,
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
    fn a_point_is_found_again_after_retagging_gives_it_a_new_id() {
        let before = point("aaa", Some("run:1"), false);
        let after = vec![
            point("bbb", Some("run:2"), false),
            point("ccc", Some("run:1"), true),
        ];
        assert_eq!(retagged(&before, &after).unwrap().snapshot_id, "ccc");
        let older = point("old", None, false);
        let after = vec![point("new", None, true)];
        assert_eq!(
            retagged(&older, &after).unwrap().snapshot_id,
            "new",
            "an older backup has no run, so its time identifies it"
        );
    }

    #[test]
    fn points_are_read_newest_first_without_the_nightly_cluster_snapshot() {
        let snapshots = json!([
            {"id": "old", "time": "2026-11-14T03:00:00Z", "tags": ["cluster-backup", "namespace:yolab-a"]},
            {"id": "dr", "time": "2026-11-16T04:00:00Z", "tags": ["cluster-backup", "scope:cluster"]},
            {"id": "new", "time": "2026-11-16T03:00:00Z", "tags": ["cluster-backup", "namespace:yolab-a", "run:bk-2", "yolab-keep"]},
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
                    keep: true
                },
                Point {
                    snapshot_id: "old".into(),
                    time: "2026-11-14T03:00:00Z".into(),
                    run: None,
                    keep: false
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
        let points = vec![point("p1", Some("bk-1"), false)];
        let folders = FolderSnapshots::from([
            ("bk-1".into(), vec!["f1".into()]),
            ("bk-0".into(), vec!["f0".into()]),
        ]);
        assert_eq!(orphans(&points, &folders), vec!["f0".to_string()]);
    }

    #[test]
    fn the_default_budget_is_twice_the_newest_point_and_never_under_a_gigabyte() {
        assert_eq!(default_budget(0), MIN_BUDGET);
        assert_eq!(default_budget(100), MIN_BUDGET);
        assert_eq!(default_budget(10 << 30), 20 << 30);
        assert_eq!(default_budget(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_budget_never_drops_the_newest_a_kept_or_an_older_style_point() {
        let points = vec![
            point("newest", Some("r4"), false),
            point("kept", Some("r3"), true),
            point("middle", Some("r2"), false),
            point("legacy", None, false),
            point("oldest", Some("r1"), false),
        ];
        let ids: Vec<&str> = droppable(&points)
            .iter()
            .map(|p| p.snapshot_id.as_str())
            .collect();
        assert_eq!(ids, vec!["oldest", "middle"], "oldest first");
    }

    #[tokio::test]
    async fn under_budget_nothing_is_dropped() {
        let points = vec![point("b", Some("r2"), false), point("a", Some("r1"), false)];
        let fitted = fit(&points, 100, sized(&[("a", 30), ("b", 30)]))
            .await
            .unwrap();
        assert_eq!(
            fitted,
            Fit {
                drop: vec![],
                size_after: 60,
                over: false
            }
        );
    }

    #[tokio::test]
    async fn over_budget_the_oldest_points_go_until_it_fits() {
        let points = vec![
            point("c", Some("r3"), false),
            point("b", Some("r2"), false),
            point("a", Some("r1"), false),
        ];
        let fitted = fit(&points, 70, sized(&[("a", 40), ("b", 30), ("c", 30)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["a".to_string()]);
        assert_eq!(fitted.size_after, 60);
        assert!(!fitted.over);
    }

    #[tokio::test]
    async fn a_newest_point_bigger_than_the_budget_is_kept_and_reported() {
        let points = vec![
            point("big", Some("r2"), false),
            point("a", Some("r1"), false),
        ];
        let fitted = fit(&points, 50, sized(&[("a", 10), ("big", 80)]))
            .await
            .unwrap();
        assert_eq!(fitted.drop, vec!["a".to_string()]);
        assert_eq!(fitted.size_after, 80);
        assert!(fitted.over, "the newest alone is over budget");
    }

    #[tokio::test]
    async fn a_kept_point_stays_even_when_that_leaves_the_service_over_budget() {
        let points = vec![
            point("new", Some("r2"), false),
            point("pinned", Some("r1"), true),
        ];
        let fitted = fit(&points, 10, sized(&[("pinned", 50), ("new", 5)]))
            .await
            .unwrap();
        assert!(fitted.drop.is_empty());
        assert!(fitted.over);
    }

    #[test]
    fn a_service_measures_only_the_folders_of_its_restore_points() {
        let folders = FolderSnapshots::from([
            ("r1".into(), vec!["f1".into()]),
            ("r2".into(), vec!["f2".into()]),
        ]);
        let points = vec![point("p2", Some("r2"), false), point("legacy", None, false)];
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
