use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::tree::ROOT;

mod ffi;

pub const ENV: &str = "YOLAB_LIBCEPHFS";
const TIMEOUT: Duration = Duration::from_secs(10);
const LIMIT: usize = 1000;
const DT_DIR: u8 = 4;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ListError {
    Missing(String),
    Unavailable(String),
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListError::Missing(m) | ListError::Unavailable(m) => f.write_str(m),
        }
    }
}

pub(crate) fn parse_rbytes(raw: &[u8]) -> Result<u64, ListError> {
    std::str::from_utf8(raw)
        .ok()
        .map(|t| t.trim_end_matches('\0').trim())
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| {
            ListError::Unavailable(format!(
                "the folder size {:?} is not a number",
                String::from_utf8_lossy(raw)
            ))
        })
}

pub(crate) fn in_tree(path: &str) -> String {
    if path.is_empty() {
        ROOT.to_string()
    } else {
        format!("{ROOT}/{path}")
    }
}

pub(crate) fn before_any_folder(
    path: &str,
    found: Result<Vec<(String, u8)>, ListError>,
) -> Result<Vec<(String, u8)>, ListError> {
    match found {
        Err(ListError::Missing(_)) if path.is_empty() => Ok(Vec::new()),
        other => other,
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Listing {
    pub path: String,
    pub folders: Vec<String>,
    pub truncated: bool,
}

pub(crate) fn listing(path: &str, entries: Vec<(String, u8)>) -> Listing {
    let mut folders: Vec<String> = entries
        .into_iter()
        .filter(|(name, kind)| *kind == DT_DIR && name != "." && name != "..")
        .map(|(name, _)| name)
        .collect();
    folders.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
    let truncated = folders.len() > LIMIT;
    folders.truncate(LIMIT);
    Listing {
        path: path.to_string(),
        folders,
        truncated,
    }
}

pub(crate) fn sizes_of(
    folders: &[String],
    size: impl Fn(&str) -> Result<u64, ListError>,
) -> Result<Vec<Option<u64>>, ListError> {
    let mut out = Vec::with_capacity(folders.len());
    for folder in folders {
        match size(folder) {
            Ok(bytes) => out.push(Some(bytes)),
            Err(ListError::Missing(m)) => {
                tracing::warn!("folder size: {m}");
                out.push(None);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

async fn off_thread<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> Result<T, ListError> + Send + 'static,
) -> Result<T, ListError> {
    off_thread_within(TIMEOUT * 2, what, work).await
}

async fn off_thread_within<T: Send + 'static>(
    limit: Duration,
    what: &str,
    work: impl FnOnce() -> Result<T, ListError> + Send + 'static,
) -> Result<T, ListError> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => Err(ListError::Unavailable(format!("{what} failed: {e}"))),
        Err(_) => Err(ListError::Unavailable(
            "the file tree did not answer in time".into(),
        )),
    }
}

pub(crate) async fn folder_sizes(folders: Vec<String>) -> Result<Vec<Option<u64>>, ListError> {
    off_thread("measuring the tree folders", move || ffi::sizes(&folders)).await
}

pub(crate) async fn folders(path: String) -> Result<Listing, ListError> {
    folders_from(path, ffi::list).await
}

type Lister = fn(&str) -> Result<Vec<(String, u8)>, ListError>;

async fn folders_from(path: String, list: Lister) -> Result<Listing, ListError> {
    let wanted = path.clone();
    off_thread("the file tree listing", move || list(&wanted))
        .await
        .map(|entries| listing(&path, entries))
}

#[derive(Debug, Deserialize)]
pub(crate) struct FoldersQuery {
    #[serde(default)]
    path: String,
}

pub(crate) fn answer(path: &str, result: Result<Listing, ListError>) -> Response {
    match result {
        Ok(listing) => Json(listing).into_response(),
        Err(ListError::Missing(m)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": m })),
        )
            .into_response(),
        Err(ListError::Unavailable(m)) => {
            tracing::warn!("tree folders {path:?}: {m}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": m })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn tree_folders(Query(q): Query<FoldersQuery>) -> Response {
    let Some(path) = crate::tree::clean_path(&q.path) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{:?} is not a folder path", q.path) })),
        )
            .into_response();
    };
    let result = folders(path.clone()).await;
    answer(&path, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: u8) -> (String, u8) {
        (name.to_string(), kind)
    }

    #[test]
    fn only_folders_are_listed_without_the_dot_entries() {
        let l = listing(
            "immich-83uz",
            vec![
                entry(".", DT_DIR),
                entry("..", DT_DIR),
                entry("uploads", DT_DIR),
                entry("notes.txt", 8),
                entry("link", 10),
            ],
        );
        assert_eq!(l.folders, vec!["uploads"]);
        assert_eq!(l.path, "immich-83uz");
        assert!(!l.truncated);
    }

    #[test]
    fn folders_come_sorted_whatever_their_case() {
        let l = listing(
            "",
            vec![
                entry("sonarr", DT_DIR),
                entry("Movies", DT_DIR),
                entry("immich", DT_DIR),
            ],
        );
        assert_eq!(l.folders, vec!["immich", "Movies", "sonarr"]);
    }

    #[test]
    fn a_huge_folder_is_cut_and_says_so() {
        let many = (0..LIMIT + 5)
            .map(|i| entry(&format!("d{i:05}"), DT_DIR))
            .collect();
        let l = listing("", many);
        assert_eq!(l.folders.len(), LIMIT);
        assert!(l.truncated);
    }

    #[test]
    fn a_folder_size_is_read_from_its_recursive_byte_count() {
        assert_eq!(parse_rbytes(b"167018303488"), Ok(167_018_303_488));
        assert_eq!(parse_rbytes(b"0"), Ok(0));
    }

    #[test]
    fn a_folder_size_ending_in_nul_still_reads() {
        assert_eq!(parse_rbytes(b"4096\0"), Ok(4096));
    }

    #[test]
    fn a_folder_size_that_is_not_a_number_is_an_error_not_zero() {
        assert!(matches!(parse_rbytes(b""), Err(ListError::Unavailable(_))));
        assert!(matches!(
            parse_rbytes(b"lots"),
            Err(ListError::Unavailable(_))
        ));
    }

    #[test]
    fn every_folder_asked_for_gets_a_size_in_order() {
        let folders = vec!["a".to_string(), "gone".to_string(), "b".to_string()];
        let sizes = sizes_of(&folders, |f| match f {
            "a" => Ok(1),
            "b" => Ok(2),
            _ => Err(ListError::Missing("gone".into())),
        });
        assert_eq!(sizes, Ok(vec![Some(1), None, Some(2)]));
    }

    #[test]
    fn a_tree_that_stops_answering_fails_the_whole_measure() {
        let folders = vec!["a".to_string(), "b".to_string()];
        let sizes = sizes_of(&folders, |_| Err(ListError::Unavailable("down".into())));
        assert_eq!(sizes, Err(ListError::Unavailable("down".into())));
    }

    #[test]
    fn a_folder_that_is_not_there_is_not_found_and_a_silent_tree_is_unavailable() {
        assert_eq!(
            answer("x", Err(ListError::Missing("gone".into()))).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            answer("x", Err(ListError::Unavailable("slow".into()))).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            answer("x", Ok(listing("x", vec![]))).status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_path_leaving_the_tree_is_refused_before_ceph_is_asked() {
        for path in ["../etc", "a/../b", "a//b"] {
            let r = tree_folders(Query(FoldersQuery { path: path.into() })).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{path}");
        }
    }

    #[test]
    fn folders_are_looked_up_under_the_tree_root() {
        assert_eq!(in_tree(""), "/tree");
        assert_eq!(in_tree("immich-83uz/uploads"), "/tree/immich-83uz/uploads");
    }

    #[test]
    fn a_tree_no_service_has_made_yet_is_empty_not_broken() {
        assert_eq!(
            before_any_folder("", Err(ListError::Missing("no /tree".into()))),
            Ok(Vec::new())
        );
        assert_eq!(
            before_any_folder("photos", Err(ListError::Missing("gone".into()))),
            Err(ListError::Missing("gone".into()))
        );
        assert_eq!(
            before_any_folder("", Err(ListError::Unavailable("slow".into()))),
            Err(ListError::Unavailable("slow".into()))
        );
    }

    #[tokio::test]
    async fn without_libcephfs_the_tree_is_unavailable_and_says_why() {
        let missing = ListError::Unavailable(format!("{ENV} is not set"));
        assert_eq!(folders(String::new()).await, Err(missing));
        assert!(folder_sizes(vec!["photos".into()]).await.is_err());
        let r = tree_folders(Query(FoldersQuery {
            path: "photos".into(),
        }))
        .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn work_that_crashes_is_unavailable_not_a_crash_of_the_page() {
        let crashed = off_thread("listing", || -> Result<(), ListError> {
            panic!("libcephfs crashed")
        })
        .await;
        assert!(
            matches!(&crashed, Err(ListError::Unavailable(m)) if m.starts_with("listing failed: ")),
            "{crashed:?}"
        );
    }

    fn sized(folder: &str) -> Result<u64, ListError> {
        match folder {
            "a" => Ok(1),
            "down" => Err(ListError::Unavailable("down".into())),
            _ => Err(ListError::Missing("gone".into())),
        }
    }

    #[test]
    fn the_same_measure_skips_a_vanished_folder_but_stops_at_a_silent_tree() {
        let owned = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            sizes_of(&owned(&["a", "gone"]), sized),
            Ok(vec![Some(1), None])
        );
        assert_eq!(
            sizes_of(&owned(&["a", "down"]), sized),
            Err(ListError::Unavailable("down".into()))
        );
    }

    fn two_folders(path: &str) -> Result<Vec<(String, u8)>, ListError> {
        Ok(vec![entry(path, DT_DIR), entry("Movies", DT_DIR)])
    }

    #[tokio::test]
    async fn a_listing_the_tree_gives_is_shown_as_its_folders() {
        assert_eq!(
            folders_from("photos".into(), two_folders).await,
            Ok(Listing {
                path: "photos".into(),
                folders: vec!["Movies".into(), "photos".into()],
                truncated: false,
            })
        );
    }

    enum Work {
        Answer,
        Crash,
        Hang(std::sync::mpsc::Receiver<()>),
    }

    fn doing(work: Work) -> impl FnOnce() -> Result<u8, ListError> + Send + 'static {
        move || match work {
            Work::Answer => Ok(7),
            Work::Crash => panic!("libcephfs crashed"),
            Work::Hang(wait) => {
                wait.recv().expect("the test releases the work");
                Ok(0)
            }
        }
    }

    #[tokio::test]
    async fn the_same_work_off_the_thread_answers_crashes_or_runs_out_of_time() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            off_thread_within(limit, "listing", doing(Work::Answer)).await,
            Ok(7)
        );
        assert!(matches!(
            off_thread_within(limit, "listing", doing(Work::Crash)).await,
            Err(ListError::Unavailable(m)) if m.starts_with("listing failed: ")
        ));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let slow = off_thread_within(
            Duration::from_millis(20),
            "listing",
            doing(Work::Hang(wait)),
        )
        .await;
        release.send(()).expect("the work is still waiting");
        assert_eq!(
            slow,
            Err(ListError::Unavailable(
                "the file tree did not answer in time".into()
            ))
        );
    }

    #[tokio::test]
    async fn a_tree_that_does_not_answer_in_time_is_unavailable() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let slow = off_thread_within(Duration::from_millis(20), "listing", move || {
            wait.recv().expect("the test releases the work");
            Ok(())
        })
        .await;
        release.send(()).expect("the work is still waiting");
        assert_eq!(
            slow,
            Err(ListError::Unavailable(
                "the file tree did not answer in time".into()
            ))
        );
    }
}
