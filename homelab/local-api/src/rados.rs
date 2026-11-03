use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::ceph::commands::MonCommand;
use crate::exec::{self, CmdError, CommandOutput};

pub const ENV: &str = "YOLAB_LIBRADOS";
const CONF_DIR: &str = "/etc/ceph";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_AFTER: Duration = Duration::from_secs(60);
const RTLD_NOW: c_int = 2;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

type Handle = *mut c_void;
type CreateFn = unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char, u64) -> c_int;
type ReadConfFn = unsafe extern "C" fn(Handle, *const c_char) -> c_int;
type SetConfFn = unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int;
type ConnectFn = unsafe extern "C" fn(Handle) -> c_int;
type MonCommandFn = unsafe extern "C" fn(
    Handle,
    *mut *const c_char,
    usize,
    *const c_char,
    usize,
    *mut *mut c_char,
    *mut usize,
    *mut *mut c_char,
    *mut usize,
) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_char);
type ShutdownFn = unsafe extern "C" fn(Handle);

struct Lib {
    create: CreateFn,
    read_conf: ReadConfFn,
    set_conf: SetConfFn,
    connect: ConnectFn,
    mon_command: MonCommandFn,
    free: FreeFn,
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
        create: symbol!(h, path, CreateFn, "rados_create2"),
        read_conf: symbol!(h, path, ReadConfFn, "rados_conf_read_file"),
        set_conf: symbol!(h, path, SetConfFn, "rados_conf_set"),
        connect: symbol!(h, path, ConnectFn, "rados_connect"),
        mon_command: symbol!(h, path, MonCommandFn, "rados_mon_command"),
        free: symbol!(h, path, FreeFn, "rados_buffer_free"),
        shutdown: symbol!(h, path, ShutdownFn, "rados_shutdown"),
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

struct Cluster {
    lib: &'static Lib,
    handle: Handle,
}

unsafe impl Send for Cluster {}
unsafe impl Sync for Cluster {}

struct Detached(Handle);

unsafe impl Send for Detached {}

impl Drop for Cluster {
    fn drop(&mut self) {
        let (shutdown, handle) = (self.lib.shutdown, Detached(self.handle));
        std::thread::spawn(move || {
            let handle = handle;
            unsafe { shutdown(handle.0) }
        });
    }
}

fn checked(step: &str, ret: c_int) -> Result<(), String> {
    if ret < 0 {
        return Err(format!("{step}: {}", errno_name(-ret)));
    }
    Ok(())
}

fn connect(lib: &'static Lib, timeout: Duration) -> Result<Cluster, String> {
    let mut handle: Handle = ptr::null_mut();
    let ret = unsafe { (lib.create)(&mut handle, c"ceph".as_ptr(), c"client.admin".as_ptr(), 0) };
    checked("rados_create2", ret)?;
    let cluster = Cluster { lib, handle };
    checked("read ceph.conf", unsafe {
        (lib.read_conf)(handle, ptr::null())
    })?;
    let secs = CString::new(timeout.as_secs().to_string()).map_err(|e| e.to_string())?;
    for option in [c"client_mount_timeout", c"rados_mon_op_timeout"] {
        checked("set a timeout", unsafe {
            (lib.set_conf)(handle, option.as_ptr(), secs.as_ptr())
        })?;
    }
    checked("connect", unsafe { (lib.connect)(handle) })?;
    Ok(cluster)
}

struct Answer {
    ret: c_int,
    out: Vec<u8>,
    outs: String,
}

impl Cluster {
    fn take(&self, buf: *mut c_char, len: usize) -> Vec<u8> {
        if buf.is_null() {
            return Vec::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) }.to_vec();
        unsafe { (self.lib.free)(buf) };
        bytes
    }

    fn mon_command(&self, cmd: &CStr, input: &[u8]) -> Answer {
        let mut cmds = [cmd.as_ptr()];
        let (mut out, mut out_len) = (ptr::null_mut(), 0usize);
        let (mut outs, mut outs_len) = (ptr::null_mut(), 0usize);
        let ret = unsafe {
            (self.lib.mon_command)(
                self.handle,
                cmds.as_mut_ptr(),
                1,
                input.as_ptr().cast(),
                input.len(),
                &mut out,
                &mut out_len,
                &mut outs,
                &mut outs_len,
            )
        };
        Answer {
            ret,
            out: self.take(out, out_len),
            outs: String::from_utf8_lossy(&self.take(outs, outs_len)).into_owned(),
        }
    }
}

pub(crate) fn errno_name(errno: c_int) -> &'static str {
    match errno {
        1 => "EPERM",
        2 => "ENOENT",
        3 => "ESRCH",
        5 => "EIO",
        11 => "EAGAIN",
        13 => "EACCES",
        16 => "EBUSY",
        17 => "EEXIST",
        19 => "ENODEV",
        22 => "EINVAL",
        34 => "ERANGE",
        95 => "EOPNOTSUPP",
        108 => "ESHUTDOWN",
        110 => "ETIMEDOUT",
        111 => "ECONNREFUSED",
        _ => "Unknown",
    }
}

fn into_output(answer: Answer, json_format: bool) -> CommandOutput {
    let mut stdout = String::from_utf8_lossy(&answer.out).into_owned();
    if json_format {
        stdout.insert(0, '\n');
    }
    let stderr = if answer.ret < 0 {
        format!("Error {}: {}", errno_name(-answer.ret), answer.outs)
    } else {
        answer.outs
    };
    CommandOutput {
        success: answer.ret >= 0,
        stdout,
        stderr,
    }
}

pub(crate) fn conf_digest(dir: &Path) -> std::io::Result<String> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.sort();
    let mut h = Sha256::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        h.update(path.file_name().unwrap_or_default().as_encoded_bytes());
        h.update([0]);
        h.update(std::fs::read(&path)?);
        h.update([0]);
    }
    Ok(hex::encode(h.finalize()))
}

pub(crate) fn retry_due(failed_at: Option<Instant>, now: Instant) -> bool {
    failed_at.is_none_or(|t| now.duration_since(t) >= RETRY_AFTER)
}

enum State {
    Idle,
    Connecting,
    Ready(Arc<Cluster>),
    Failed(Instant),
}

struct Slot {
    conf: String,
    state: State,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    conf: String::new(),
    state: State::Idle,
});

fn slot() -> std::sync::MutexGuard<'static, Slot> {
    SLOT.lock().expect("the librados slot is never poisoned")
}

fn cluster() -> Result<Arc<Cluster>, String> {
    let conf =
        conf_digest(Path::new(CONF_DIR)).map_err(|e| format!("cannot read {CONF_DIR}: {e}"))?;
    let mut s = slot();
    if s.conf != conf {
        s.conf = conf.clone();
        s.state = State::Idle;
    }
    match &s.state {
        State::Ready(c) => return Ok(c.clone()),
        State::Connecting => return Err("librados is still connecting".into()),
        State::Failed(at) if !retry_due(Some(*at), Instant::now()) => {
            return Err("librados failed to connect recently".into())
        }
        State::Idle | State::Failed(_) => {}
    }
    let lib = match library() {
        Ok(lib) => lib,
        Err(e) => {
            tracing::warn!("librados: {e}; ceph commands use the ceph CLI");
            s.state = State::Failed(Instant::now());
            return Err(e);
        }
    };
    s.state = State::Connecting;
    tokio::spawn(async move {
        let connected = tokio::task::spawn_blocking(move || connect(lib, CONNECT_TIMEOUT))
            .await
            .map_err(|e| format!("the connect task failed: {e}"))
            .and_then(|r| r);
        let mut s = slot();
        if s.conf != conf {
            return;
        }
        s.state = match connected {
            Ok(c) => {
                tracing::info!("librados: connected");
                State::Ready(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!(
                    "librados: {e}; ceph commands use the ceph CLI for the next {}s",
                    RETRY_AFTER.as_secs()
                );
                State::Failed(Instant::now())
            }
        };
    });
    Err("librados is connecting".into())
}

fn forget(stale: &Arc<Cluster>) {
    let mut s = slot();
    if matches!(&s.state, State::Ready(c) if Arc::ptr_eq(c, stale)) {
        s.state = State::Idle;
    }
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Answered(CommandOutput),
    NotRun(String),
}

pub(crate) async fn run(cmd: &MonCommand, default_timeout: Duration) -> Result<Outcome, CmdError> {
    let rendered = format!("ceph {}", cmd.json);
    let c = match cluster() {
        Ok(c) => c,
        Err(why) => return Ok(Outcome::NotRun(why)),
    };
    let line = CString::new(cmd.json.to_string()).map_err(|e| CmdError::parse(&rendered, e))?;
    let input = match &cmd.input {
        None => Vec::new(),
        Some(path) => tokio::fs::read(path).await.map_err(|e| CmdError::Failed {
            cmd: rendered.clone(),
            kind: exec::Failure::Other,
            stderr: format!("Can't open input file {path}: {e}"),
        })?,
    };
    let timeout = cmd.timeout.unwrap_or(default_timeout);
    let worker = c.clone();
    let work = tokio::task::spawn_blocking(move || worker.mon_command(&line, &input));
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(answer)) => Ok(Outcome::Answered(into_output(answer, cmd.json_format()))),
        Ok(Err(e)) => Err(CmdError::Failed {
            cmd: rendered,
            kind: exec::Failure::Other,
            stderr: format!("the librados call failed: {e}"),
        }),
        Err(_) => {
            forget(&c);
            Err(CmdError::Timeout {
                cmd: rendered,
                after: timeout,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_answer_starts_with_the_blank_line_the_cli_prints() {
        let out = into_output(
            Answer {
                ret: 0,
                out: br#"{"fsid":"a"}"#.to_vec(),
                outs: String::new(),
            },
            true,
        );
        assert!(out.success);
        assert_eq!(out.stdout, "\n{\"fsid\":\"a\"}");
        let plain = into_output(
            Answer {
                ret: 0,
                out: b"HEALTH_OK\n".to_vec(),
                outs: String::new(),
            },
            false,
        );
        assert_eq!(plain.stdout, "HEALTH_OK\n");
    }

    #[test]
    fn a_failure_is_reported_like_the_cli_so_it_classifies_the_same() {
        let out = into_output(
            Answer {
                ret: -2,
                out: Vec::new(),
                outs: "error obtaining 'yolab/x': (2) No such file or directory".into(),
            },
            false,
        );
        assert!(!out.success);
        assert!(out.stderr.starts_with("Error ENOENT: "));
        assert_eq!(exec::classify("ceph", &out.stderr), exec::Failure::NotFound);
        let busy = into_output(
            Answer {
                ret: -16,
                out: Vec::new(),
                outs: String::new(),
            },
            false,
        );
        assert_eq!(exec::classify("ceph", &busy.stderr), exec::Failure::Busy);
    }

    #[test]
    fn a_changed_ceph_conf_or_keyring_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ceph.conf"), "fsid = a").unwrap();
        let before = conf_digest(dir.path()).unwrap();
        assert_eq!(before, conf_digest(dir.path()).unwrap());
        std::fs::write(dir.path().join("ceph.client.admin.keyring"), "key").unwrap();
        let with_key = conf_digest(dir.path()).unwrap();
        assert_ne!(before, with_key);
        std::fs::write(dir.path().join("ceph.conf"), "fsid = b").unwrap();
        assert_ne!(with_key, conf_digest(dir.path()).unwrap());
    }

    #[test]
    fn a_failed_connect_is_not_retried_for_a_minute() {
        let now = Instant::now();
        assert!(retry_due(None, now));
        assert!(!retry_due(Some(now), now + Duration::from_secs(59)));
        assert!(retry_due(Some(now), now + RETRY_AFTER));
    }

    #[test]
    fn a_missing_library_is_an_error_naming_it() {
        let e = load("/nonexistent/librados.so.2").err().unwrap();
        assert!(e.contains("/nonexistent/librados.so.2"), "{e}");
    }
}
