use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::{Mutex, OnceLock, TryLockError};

use super::{before_any_folder, in_tree, parse_rbytes, sizes_of, ListError, ENV, TIMEOUT};
use crate::cephfs::FS_NAME;
use crate::rados::errno_name;

const RTLD_NOW: c_int = 2;
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
type GetXattrFn =
    unsafe extern "C" fn(Mount, *const c_char, *const c_char, *mut c_void, usize) -> c_int;

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
    getxattr: GetXattrFn,
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
        getxattr: symbol!(h, path, GetXattrFn, "ceph_getxattr"),
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

    fn size(&self, path: &str) -> Result<u64, ListError> {
        let c = CString::new(in_tree(path))
            .map_err(|_| ListError::Missing(format!("{path:?} is not a folder name")))?;
        let mut buf = [0u8; 32];
        let ret = unsafe {
            (self.lib.getxattr)(
                self.handle,
                c.as_ptr(),
                RBYTES.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if ret == -ENOENT || ret == -ENOTDIR {
            return Err(ListError::Missing(format!(
                "{path:?} is not a folder in the tree"
            )));
        }
        if ret < 0 {
            return Err(failed("read the folder size", ret));
        }
        parse_rbytes(&buf[..ret as usize])
    }
}

const RBYTES: &CStr = c"ceph.dir.rbytes";

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn with_session<T>(ask: impl FnOnce(&Session) -> Result<T, ListError>) -> Result<T, ListError> {
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
    let result = ask(session);
    if matches!(result, Err(ListError::Unavailable(_))) {
        *held = None;
    }
    result
}

pub(super) fn list(path: &str) -> Result<Vec<(String, u8)>, ListError> {
    with_session(|session| before_any_folder(path, session.entries(path)))
}

pub(super) fn sizes(folders: &[String]) -> Result<Vec<Option<u64>>, ListError> {
    with_session(|session| sizes_of(folders, |f| session.size(f)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_attribute_is_the_recursive_byte_count() {
        assert_eq!(RBYTES.to_str(), Ok("ceph.dir.rbytes"));
    }

    #[test]
    fn the_dirent_matches_the_linux_layout_libcephfs_fills() {
        assert_eq!(std::mem::offset_of!(Dirent, d_type), 18);
        assert_eq!(std::mem::offset_of!(Dirent, d_name), 19);
        assert_eq!(std::mem::size_of::<Dirent>(), 280);
    }

    #[test]
    fn a_missing_library_is_an_error_naming_it() {
        let e = load("/nonexistent/libcephfs.so.2").err().unwrap();
        assert!(e.contains("/nonexistent/libcephfs.so.2"), "{e}");
    }
}
