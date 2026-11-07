use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cephfs::FS_NAME;
use crate::rados::errno_name;
use crate::tree::ROOT;

pub const ENV: &str = "YOLAB_LIBCEPHFS";
const TIMEOUT: Duration = Duration::from_secs(10);
const LIMIT: usize = 1000;
const RTLD_NOW: c_int = 2;
const DT_DIR: u8 = 4;
const ENOENT: c_int = 2;
const ENOTDIR: c_int = 20;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

type Mount = *mut c_void;
type Dir = *mut c_void;

#[repr(C)]
#[allow(dead_code)]
struct Dirent {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    d_name: [c_char; 256],
}

type CreateFn = unsafe extern "C" fn(*mut Mount, *const c_char) -> c_int;
type ReadConfFn = unsafe extern "C" fn(Mount, *const c_char) -> c_int;
type SetConfFn = unsafe extern "C" fn(Mount, *const c_char, *const c_char) -> c_int;
type SelectFsFn = unsafe extern "C" fn(Mount, *const c_char) -> c_int;
type MountFn = unsafe extern "C" fn(Mount, *const c_char) -> c_int;
type OpenDirFn = unsafe extern "C" fn(Mount, *const c_char, *mut Dir) -> c_int;
type ReadDirFn = unsafe extern "C" fn(Mount, Dir, *mut Dirent) -> c_int;
type CloseDirFn = unsafe extern "C" fn(Mount, Dir) -> c_int;
type ShutdownFn = unsafe extern "C" fn(Mount);

struct Lib {
    create: CreateFn,
    read_conf: ReadConfFn,
    set_conf: SetConfFn,
    select_fs: SelectFsFn,
    mount: MountFn,
    opendir: OpenDirFn,
    readdir: ReadDirFn,
    closedir: CloseDirFn,
    shutdown: ShutdownFn,
}

fn dl_error() -> String {
    let e = unsafe { dlerror() };
    if e.is_null() {
        return "unknown dlopen error".into();
    }
    unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
}

macro_rules! symbol {
    ($handle:expr, $path:expr, $ty:ty, $name:literal) => {{
        let p = unsafe { dlsym($handle, concat!($name, "\0").as_ptr().cast()) };
        if p.is_null() {
            return Err(format!("{} has no {}: {}", $path, $name, dl_error()));
        }
        unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
    }};
}

fn load(path: &str) -> Result<Lib, String> {
    let c = CString::new(path).map_err(|e| format!("{path}: {e}"))?;
    let h = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
    if h.is_null() {
        return Err(format!("cannot load {path}: {}", dl_error()));
    }
    Ok(Lib {
        create: symbol!(h, path, CreateFn, "ceph_create"),
        read_conf: symbol!(h, path, ReadConfFn, "ceph_conf_read_file"),
        set_conf: symbol!(h, path, SetConfFn, "ceph_conf_set"),
        select_fs: symbol!(h, path, SelectFsFn, "ceph_select_filesystem"),
        mount: symbol!(h, path, MountFn, "ceph_mount"),
        opendir: symbol!(h, path, OpenDirFn, "ceph_opendir"),
        readdir: symbol!(h, path, ReadDirFn, "ceph_readdir_r"),
        closedir: symbol!(h, path, CloseDirFn, "ceph_closedir"),
        shutdown: symbol!(h, path, ShutdownFn, "ceph_shutdown"),
    })
}

fn library() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        let path = std::env::var(ENV).map_err(|_| format!("{ENV} is not set"))?;
        load(&path)
    })
    .as_ref()
    .map_err(Clone::clone)
}

struct Session {
    lib: &'static Lib,
    handle: Mount,
}

unsafe impl Send for Session {}

struct Detached(Mount);

unsafe impl Send for Detached {}

impl Drop for Session {
    fn drop(&mut self) {
        let (shutdown, handle) = (self.lib.shutdown, Detached(self.handle));
        std::thread::spawn(move || {
            let handle = handle;
            unsafe { shutdown(handle.0) }
        });
    }
}

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

fn failed(step: &str, ret: c_int) -> ListError {
    ListError::Unavailable(format!("{step}: {}", errno_name(-ret)))
}

fn connect(lib: &'static Lib) -> Result<Session, ListError> {
    let mut handle: Mount = ptr::null_mut();
    let ret = unsafe { (lib.create)(&mut handle, c"admin".as_ptr()) };
    if ret < 0 {
        return Err(failed("ceph_create", ret));
    }
    let session = Session { lib, handle };
    let ret = unsafe { (lib.read_conf)(handle, ptr::null()) };
    if ret < 0 {
        return Err(failed("read ceph.conf", ret));
    }
    let secs = CString::new(TIMEOUT.as_secs().to_string()).expect("digits hold no NUL");
    let ret = unsafe { (lib.set_conf)(handle, c"client_mount_timeout".as_ptr(), secs.as_ptr()) };
    if ret < 0 {
        return Err(failed("set the mount timeout", ret));
    }
    let fs = CString::new(FS_NAME).expect("the file system name holds no NUL");
    let ret = unsafe { (lib.select_fs)(handle, fs.as_ptr()) };
    if ret < 0 {
        return Err(failed("select the file system", ret));
    }
    let ret = unsafe { (lib.mount)(handle, c"/".as_ptr()) };
    if ret < 0 {
        return Err(failed("mount the file tree", ret));
    }
    Ok(session)
}

fn name_of(de: &Dirent) -> String {
    unsafe { CStr::from_ptr(de.d_name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

impl Session {
    fn entries(&self, path: &str) -> Result<Vec<(String, u8)>, ListError> {
        let c = CString::new(in_tree(path))
            .map_err(|_| ListError::Missing(format!("{path:?} is not a folder name")))?;
        let mut dir: Dir = ptr::null_mut();
        let ret = unsafe { (self.lib.opendir)(self.handle, c.as_ptr(), &mut dir) };
        if ret == -ENOENT || ret == -ENOTDIR {
            return Err(ListError::Missing(format!(
                "{path:?} is not a folder in the tree"
            )));
        }
        if ret < 0 {
            return Err(failed("open the folder", ret));
        }
        let mut found = Vec::new();
        let mut de = Dirent {
            d_ino: 0,
            d_off: 0,
            d_reclen: 0,
            d_type: 0,
            d_name: [0; 256],
        };
        let outcome = loop {
            let ret = unsafe { (self.lib.readdir)(self.handle, dir, &mut de) };
            if ret == 0 {
                break Ok(());
            }
            if ret < 0 {
                break Err(failed("read the folder", ret));
            }
            found.push((name_of(&de), de.d_type));
        };
        unsafe { (self.lib.closedir)(self.handle, dir) };
        outcome.map(|()| found)
    }
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

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn list_blocking(path: &str) -> Result<Vec<(String, u8)>, ListError> {
    let mut held = match SESSION.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            return Err(ListError::Unavailable(
                "the file tree is still answering an earlier question".into(),
            ))
        }
        Err(TryLockError::Poisoned(p)) => {
            let mut guard = p.into_inner();
            *guard = None;
            guard
        }
    };
    if held.is_none() {
        let lib = library().map_err(ListError::Unavailable)?;
        *held = Some(connect(lib)?);
    }
    let session = held.as_ref().expect("a session was just set");
    let result = before_any_folder(path, session.entries(path));
    if matches!(result, Err(ListError::Unavailable(_))) {
        *held = None;
    }
    result
}

pub(crate) async fn folders(path: String) -> Result<Listing, ListError> {
    let wanted = path.clone();
    let work = tokio::task::spawn_blocking(move || list_blocking(&wanted));
    match tokio::time::timeout(TIMEOUT * 2, work).await {
        Ok(Ok(found)) => found.map(|entries| listing(&path, entries)),
        Ok(Err(e)) => Err(ListError::Unavailable(format!(
            "the file tree listing failed: {e}"
        ))),
        Err(_) => Err(ListError::Unavailable(format!(
            "the file tree did not answer within {}s",
            (TIMEOUT * 2).as_secs()
        ))),
    }
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
    fn the_dirent_matches_the_linux_layout_libcephfs_fills() {
        assert_eq!(std::mem::offset_of!(Dirent, d_type), 18);
        assert_eq!(std::mem::offset_of!(Dirent, d_name), 19);
        assert_eq!(std::mem::size_of::<Dirent>(), 280);
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

    #[test]
    fn a_missing_library_is_an_error_naming_it() {
        let e = load("/nonexistent/libcephfs.so.2").err().unwrap();
        assert!(e.contains("/nonexistent/libcephfs.so.2"), "{e}");
    }
}
