use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};

use crate::exec::{self, CmdError, CommandOutput};

pub const ENV: &str = "YOLAB_CEPH_WORKER";
const CONF_DIR: &str = "/etc/ceph";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request<'a> {
    pub args: Vec<&'a str>,
    pub format: Option<&'a str>,
    pub timeout: Option<Duration>,
}

pub(crate) fn request<'a>(args: &[&'a str]) -> Option<Request<'a>> {
    let mut out = Request {
        args: Vec::new(),
        format: None,
        timeout: None,
    };
    let mut it = args.iter().copied();
    while let Some(a) = it.next() {
        match a {
            "-f" | "--format" => out.format = Some(it.next()?),
            "-s" | "--status" => out.args.insert(0, "status"),
            "--connect-timeout" => {
                out.timeout = Some(Duration::from_secs(it.next()?.parse().ok()?));
            }
            _ => {
                if let Some(f) = a.strip_prefix("--format=") {
                    out.format = Some(f);
                } else if is_option(a) {
                    return None;
                } else {
                    out.args.push(a);
                }
            }
        }
    }
    match out.args.first() {
        None | Some(&"tell") | Some(&"daemon") => None,
        Some(_) => Some(out),
    }
}

fn is_option(a: &str) -> bool {
    a.starts_with('-') && !a[1..].starts_with(|c: char| c.is_ascii_digit())
}

#[derive(Serialize)]
struct Wire<'a> {
    id: u64,
    args: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a str>,
    timeout: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct Reply {
    id: u64,
    ret: i64,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub(crate) enum Line {
    Ready { ready: bool },
    Reply(Reply),
}

pub(crate) fn into_output(reply: Reply) -> CommandOutput {
    CommandOutput {
        success: reply.ret >= 0,
        stdout: reply.stdout,
        stderr: reply.stderr,
    }
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Answered(CommandOutput),
    NotRun(String),
}

enum Fate {
    Reply(Reply),
    NotRun,
    Lost,
}

type Waiters = HashMap<u64, oneshot::Sender<Fate>>;
type Pending = Arc<StdMutex<Waiters>>;

fn waiters(p: &Pending) -> std::sync::MutexGuard<'_, Waiters> {
    p.lock().expect("pending map poisoned")
}

struct Worker {
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    next: AtomicU64,
    exited: Arc<AtomicBool>,
    retired: AtomicBool,
    conf: String,
}

impl Worker {
    fn usable(&self, conf: &str) -> bool {
        !self.exited.load(Ordering::SeqCst)
            && !self.retired.load(Ordering::SeqCst)
            && self.conf == conf
    }

    async fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.stdin.lock().await.take();
    }
}

struct Slot {
    worker: Option<Arc<Worker>>,
    failed_at: Option<Instant>,
}

static SLOT: Mutex<Slot> = Mutex::const_new(Slot {
    worker: None,
    failed_at: None,
});

pub(crate) fn conf_digest(dir: &Path) -> std::io::Result<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    names.sort();
    let mut h = Sha256::new();
    for path in names {
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

async fn worker() -> Result<Arc<Worker>, String> {
    let conf =
        conf_digest(Path::new(CONF_DIR)).map_err(|e| format!("cannot read {CONF_DIR}: {e}"))?;
    let mut slot = SLOT.lock().await;
    if let Some(w) = slot.worker.as_ref() {
        if w.usable(&conf) {
            return Ok(w.clone());
        }
        w.retire().await;
        slot.worker = None;
    }
    if !retry_due(slot.failed_at, Instant::now()) {
        return Err("the ceph worker failed recently".into());
    }
    let spawned = std::env::var(ENV)
        .map_err(|_| format!("{ENV} is not set"))
        .and_then(|program| spawn(&program, conf));
    match spawned {
        Ok(w) => {
            let w = Arc::new(w);
            slot.worker = Some(w.clone());
            slot.failed_at = None;
            Ok(w)
        }
        Err(e) => {
            tracing::warn!(
                "ceph worker: {e}; ceph commands use the ceph CLI for the next {}s",
                RETRY_AFTER.as_secs()
            );
            slot.failed_at = Some(Instant::now());
            Err(e)
        }
    }
}

fn spawn(program: &str, conf: String) -> Result<Worker, String> {
    let mut child = Command::new(program)
        .arg(CONNECT_TIMEOUT.as_secs().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;
    let stdin = child.stdin.take().ok_or("the worker has no stdin")?;
    let stdout = child.stdout.take().ok_or("the worker has no stdout")?;
    let pending: Pending = Arc::default();
    let exited = Arc::new(AtomicBool::new(false));
    let (p, x) = (pending.clone(), exited.clone());
    tokio::spawn(async move {
        let connected = read_replies(stdout, &p).await;
        x.store(true, Ordering::SeqCst);
        if !connected {
            SLOT.lock().await.failed_at = Some(Instant::now());
        }
        let waiting: Vec<_> = waiters(&p).drain().collect();
        for (id, tx) in waiting {
            let fate = if connected { Fate::Lost } else { Fate::NotRun };
            if tx.send(fate).is_err() {
                tracing::debug!("ceph worker: request {id} was already given up on");
            }
        }
        match child.wait().await {
            Ok(status) if status.success() => tracing::info!("ceph worker exited"),
            Ok(status) => tracing::warn!("ceph worker exited with {status}"),
            Err(e) => tracing::warn!("ceph worker: could not wait for it to exit: {e}"),
        }
    });
    Ok(Worker {
        stdin: Mutex::new(Some(stdin)),
        pending,
        next: AtomicU64::new(1),
        exited,
        retired: AtomicBool::new(false),
        conf,
    })
}

async fn read_replies(stdout: ChildStdout, pending: &Pending) -> bool {
    let mut connected = false;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return connected,
            Err(e) => {
                tracing::warn!("ceph worker: could not read its output: {e}");
                return connected;
            }
        };
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Ready { ready }) => connected = ready,
            Ok(Line::Reply(reply)) => {
                let tx = waiters(pending).remove(&reply.id);
                if let Some(tx) = tx {
                    if tx.send(Fate::Reply(reply)).is_err() {
                        tracing::debug!("ceph worker: a reply came after its caller gave up");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("ceph worker: unreadable line {line:?}: {e}");
                return connected;
            }
        }
    }
}

pub(crate) async fn run(req: &Request<'_>, default_timeout: Duration) -> Result<Outcome, CmdError> {
    let cmd = exec::render("ceph", &req.args);
    let w = match worker().await {
        Ok(w) => w,
        Err(why) => return Ok(Outcome::NotRun(why)),
    };
    let timeout = req.timeout.unwrap_or(default_timeout);
    let id = w.next.fetch_add(1, Ordering::SeqCst);
    let line = serde_json::to_string(&Wire {
        id,
        args: &req.args,
        format: req.format,
        timeout: timeout.as_secs().max(1),
    })
    .map_err(|e| CmdError::parse(cmd.clone(), e))?;
    let (tx, rx) = oneshot::channel();
    waiters(&w.pending).insert(id, tx);
    let sent = {
        let mut stdin = w.stdin.lock().await;
        match stdin.as_mut() {
            None => Err("the worker was retired".to_string()),
            Some(s) => s
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| e.to_string()),
        }
    };
    if let Err(why) = sent {
        waiters(&w.pending).remove(&id);
        return Ok(Outcome::NotRun(why));
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Fate::Reply(reply))) => Ok(Outcome::Answered(into_output(reply))),
        Ok(Ok(Fate::NotRun)) => Ok(Outcome::NotRun(
            "the worker exited before it connected".into(),
        )),
        Ok(Ok(Fate::Lost)) | Ok(Err(_)) => Err(CmdError::Failed {
            cmd,
            kind: exec::Failure::Unreachable,
            stderr: "the ceph worker exited before it answered".into(),
        }),
        Err(_) => {
            waiters(&w.pending).remove(&id);
            w.retire().await;
            Err(CmdError::Timeout {
                cmd,
                after: timeout,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_read_goes_to_the_worker_with_its_format_split_off() {
        assert_eq!(
            request(&["osd", "df", "tree", "-f", "json"]),
            Some(Request {
                args: vec!["osd", "df", "tree"],
                format: Some("json"),
                timeout: None,
            })
        );
        assert_eq!(
            request(&["status", "--format=json-pretty"]).unwrap().format,
            Some("json-pretty")
        );
    }

    #[test]
    fn dash_s_is_ceph_status_as_the_cli_treats_it() {
        assert_eq!(request(&["-s"]).unwrap().args, vec!["status"]);
    }

    #[test]
    fn connect_timeout_becomes_the_request_deadline() {
        let r = request(&["--connect-timeout", "10", "mon", "dump", "-f", "json"]).unwrap();
        assert_eq!(r.args, vec!["mon", "dump"]);
        assert_eq!(r.timeout, Some(Duration::from_secs(10)));
        assert_eq!(request(&["--connect-timeout", "soon", "health"]), None);
    }

    #[test]
    fn any_other_option_stays_on_the_real_cli() {
        for args in [
            vec!["fs", "new", "fs", "meta", "data", "--force"],
            vec!["auth", "rotate", "mon.", "--key_type", "aes256k"],
            vec!["-i", "/tmp/map", "osd", "setcrushmap"],
            vec!["--watch"],
            vec!["status", "-f"],
        ] {
            assert_eq!(request(&args), None, "{args:?}");
        }
    }

    #[test]
    fn tell_daemon_and_empty_commands_stay_on_the_real_cli() {
        assert_eq!(request(&["tell", "osd.0", "bench"]), None);
        assert_eq!(request(&["daemon", "osd.0", "perf", "dump"]), None);
        assert_eq!(request(&[]), None);
        assert_eq!(request(&["-f", "json"]), None);
    }

    #[test]
    fn negative_numbers_are_arguments_not_options() {
        assert_eq!(
            request(&["osd", "pool", "set", "p", "x", "-1"])
                .unwrap()
                .args,
            vec!["osd", "pool", "set", "p", "x", "-1"]
        );
    }

    #[test]
    fn a_ceph_error_reply_classifies_like_the_cli_stderr() {
        let line: Line = serde_json::from_str(
            r#"{"id":4,"ret":-2,"stdout":"","stderr":"Error ENOENT: unrecognized pool 'x'"}"#,
        )
        .unwrap();
        let Line::Reply(reply) = line else {
            panic!("a reply line was read as the ready line")
        };
        let out = into_output(reply);
        assert!(!out.success);
        assert_eq!(exec::classify("ceph", &out.stderr), exec::Failure::NotFound);
    }

    #[test]
    fn the_ready_line_is_not_mistaken_for_a_reply() {
        assert_eq!(
            serde_json::from_str::<Line>(r#"{"ready":true}"#).unwrap(),
            Line::Ready { ready: true }
        );
    }

    #[test]
    fn a_successful_reply_keeps_stdout_verbatim() {
        let out = into_output(Reply {
            id: 1,
            ret: 0,
            stdout: "\n{\"fsid\":\"a\"}".into(),
            stderr: String::new(),
        });
        assert!(out.success);
        assert_eq!(out.stdout, "\n{\"fsid\":\"a\"}");
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
    fn a_failed_worker_is_not_respawned_for_a_minute() {
        let now = Instant::now();
        assert!(retry_due(None, now));
        assert!(!retry_due(Some(now), now + Duration::from_secs(59)));
        assert!(retry_due(Some(now), now + RETRY_AFTER));
    }
}
