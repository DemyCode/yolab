//! Every subprocess this crate runs, and the one error type they all fail with.
//!
//! WHY A TYPE AND NOT A STRING
//!
//! Almost every storage outage this project has had was the same bug in a new
//! place: a command that could not answer was read as a command that answered
//! "nothing". `rbd ls` timed out and `.unwrap_or(false)` said "no image"
//! (containerd_store, 23 hours). `ceph-volume` timed out and an empty map said
//! "no OSDs here" (disks_reconciler, one weak check from re-creating over a live
//! OSD). An unreachable cluster read as `""` passed a health gate (topology).
//!
//! Each was fixed by hand where it happened, and nothing stopped the next one,
//! because `anyhow::Error` carries only prose: the only way to ask "was that
//! NotFound, or was the server down?" was `format!("{e}").contains("NotFound")`.
//!
//! `CmdError` makes the question structural. A caller that wants to treat
//! absence as empty must match `Failure::NotFound` by name; every other variant
//! — a timeout, a refused connection, output it could not parse — stays an
//! error that `?` propagates. There is deliberately no `Default` and no
//! "empty" value to fall back on.

use std::fmt;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// What a command that ran and exited non-zero was telling us.
///
/// Classified from the tool's own words (see `classify`), because that is all
/// kubectl and ceph give us: kubectl exits 1 for everything, and ceph's exit code
/// is an errno only some of the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The object asked about does not exist. The only failure that may be read
    /// as "absent".
    NotFound,
    /// A create found the object already there.
    AlreadyExists,
    /// Optimistic concurrency lost: someone wrote the object since we read it.
    Conflict,
    /// The daemon refused because the object is in use (`EBUSY`) — e.g.
    /// `ceph osd safe-to-destroy` for an OSD that still holds data.
    Busy,
    /// The server could not be reached at all. Never "absent".
    Unreachable,
    /// Anything else.
    Other,
}

#[derive(Debug)]
pub enum CmdError {
    /// The binary could not be started (missing from PATH, fork failed).
    Spawn { cmd: String, source: std::io::Error },
    /// No answer within the bound. Says nothing about whether the thing exists.
    Timeout { cmd: String, after: Duration },
    /// The command ran and exited non-zero.
    Failed {
        cmd: String,
        kind: Failure,
        stderr: String,
    },
    /// It answered, but not in a shape we understand. Never "empty".
    Parse { cmd: String, detail: String },
    /// Not run, because a call it must not overlap with is already running.
    Busy { cmd: String },
    /// A destructive command was issued outside `ceph::destructive`.
    Forbidden { cmd: String },
}

impl CmdError {
    /// The failure kind for a command that ran; `None` for every way of not
    /// getting an answer at all.
    pub fn failure(&self) -> Option<Failure> {
        match self {
            CmdError::Failed { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.failure() == Some(Failure::NotFound)
    }

    pub fn is_conflict(&self) -> bool {
        self.failure() == Some(Failure::Conflict)
    }

    pub fn is_already_exists(&self) -> bool {
        self.failure() == Some(Failure::AlreadyExists)
    }

    /// True when no answer was obtained: the question is still open. The
    /// opposite of every "it told us no" case.
    pub fn is_unanswered(&self) -> bool {
        matches!(
            self,
            CmdError::Spawn { .. } | CmdError::Timeout { .. } | CmdError::Busy { .. }
        ) || self.failure() == Some(Failure::Unreachable)
    }

    pub fn parse(cmd: impl Into<String>, detail: impl fmt::Display) -> Self {
        CmdError::Parse {
            cmd: cmd.into(),
            detail: detail.to_string(),
        }
    }

    /// A scripted failure for tests and fakes, classified exactly the way a real
    /// one would be so a fake cannot disagree with production about what an
    /// error means.
    #[cfg(test)]
    pub fn failed(cmd: impl Into<String>, stderr: impl Into<String>) -> Self {
        let cmd = cmd.into();
        let stderr = stderr.into();
        let bin = cmd.split_whitespace().next().unwrap_or("");
        if stderr.contains("timed out") {
            return CmdError::Timeout {
                cmd,
                after: Duration::ZERO,
            };
        }
        CmdError::Failed {
            kind: classify(bin, &stderr),
            cmd,
            stderr,
        }
    }
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CmdError::Spawn { cmd, source } => write!(f, "{cmd}: could not start: {source}"),
            CmdError::Timeout { cmd, after } => {
                write!(f, "{cmd}: timed out after {}s", after.as_secs())
            }
            CmdError::Failed { cmd, stderr, .. } => write!(f, "{cmd}: {stderr}"),
            CmdError::Parse { cmd, detail } => write!(f, "{cmd}: unreadable output: {detail}"),
            CmdError::Busy { cmd } => write!(f, "{cmd}: skipped, already running on this node"),
            CmdError::Forbidden { cmd } => write!(
                f,
                "{cmd}: refused — destructive commands must go through ceph::destructive"
            ),
        }
    }
}

impl std::error::Error for CmdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CmdError::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Finds a `CmdError` wherever it sits: directly, or anywhere in an
/// `anyhow::Error` chain (a `.context()` wrapped around it must not hide it).
pub trait AsCmdError {
    fn as_cmd_error(&self) -> Option<&CmdError>;
}

impl AsCmdError for CmdError {
    fn as_cmd_error(&self) -> Option<&CmdError> {
        Some(self)
    }
}

impl AsCmdError for anyhow::Error {
    fn as_cmd_error(&self) -> Option<&CmdError> {
        self.chain().find_map(|e| e.downcast_ref::<CmdError>())
    }
}

/// "This object does not exist" — and nothing else. An error that is not a
/// `CmdError` at all is not a NotFound, whatever its text says.
pub fn is_not_found(e: &impl AsCmdError) -> bool {
    e.as_cmd_error().is_some_and(CmdError::is_not_found)
}

/// Reads the tool's own description of what went wrong.
///
/// Narrow on purpose: each pattern is one the tool prints for exactly that case,
/// and anything unrecognised is `Other`, which callers must treat as an error.
pub fn classify(bin: &str, stderr: &str) -> Failure {
    match bin {
        "kubectl" => {
            if stderr.contains("(NotFound)") || stderr.contains("NotFound") {
                Failure::NotFound
            } else if stderr.contains("(AlreadyExists)") {
                Failure::AlreadyExists
            } else if stderr.contains("(Conflict)")
                || stderr.contains("the object has been modified")
            {
                Failure::Conflict
            } else if stderr.contains("connection to the server")
                || stderr.contains("unable to connect to the server")
                || stderr.contains("context deadline exceeded")
                || stderr.contains("connection refused")
            {
                Failure::Unreachable
            } else {
                Failure::Other
            }
        }
        "ceph" | "rbd" | "rados" => {
            if stderr.contains("ENOENT") {
                Failure::NotFound
            } else if stderr.contains("EEXIST") {
                Failure::AlreadyExists
            } else if stderr.contains("EBUSY") {
                Failure::Busy
            } else if stderr.contains("error connecting to the cluster")
                || stderr.contains("RADOS timed out")
                || stderr.contains("monclient") && stderr.contains("authenticate timed out")
            {
                Failure::Unreachable
            } else {
                Failure::Other
            }
        }
        _ => Failure::Other,
    }
}

/// The observable result of running a process that is allowed to exit non-zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub fn render(bin: &str, args: &[&str]) -> String {
    if args.is_empty() {
        bin.to_string()
    } else {
        format!("{bin} {}", args.join(" "))
    }
}

/// Runs `bin args`, bounded by `timeout`, and returns what it printed whether or
/// not it succeeded. `kill_on_drop`, so a timeout kills the child instead of
/// abandoning it to pile up behind the next attempt.
///
/// A child stuck in uninterruptible sleep (a `mount` against a dead RBD) cannot
/// be killed by anything; the bound still lets THIS task return and report it.
pub async fn output(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<CommandOutput, CmdError> {
    let cmd = render(bin, args);
    let work = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| CmdError::Timeout {
            cmd: cmd.clone(),
            after: timeout,
        })?
        .map_err(|source| CmdError::Spawn {
            cmd: cmd.clone(),
            source,
        })?;
    Ok(CommandOutput {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// `output`, but a non-zero exit is an error classified by `classify`.
pub async fn checked(bin: &str, args: &[&str], timeout: Duration) -> Result<String, CmdError> {
    let out = output(bin, args, timeout).await?;
    into_checked(bin, args, out)
}

pub(crate) fn into_checked(
    bin: &str,
    args: &[&str],
    out: CommandOutput,
) -> Result<String, CmdError> {
    if out.success {
        return Ok(out.stdout);
    }
    let stderr = out.stderr.trim().to_string();
    Err(CmdError::Failed {
        cmd: render(bin, args),
        kind: classify(bin, &stderr),
        stderr,
    })
}

/// Runs `bin args` with `input` on stdin. Same bound and kill semantics as
/// `output`; a non-zero exit is a classified error.
pub async fn with_stdin(
    bin: &str,
    args: &[&str],
    input: &str,
    timeout: Duration,
) -> Result<String, CmdError> {
    let cmd = render(bin, args);
    let work = async {
        let mut child = Command::new(bin)
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).await?;
        }
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| CmdError::Timeout {
            cmd: cmd.clone(),
            after: timeout,
        })?
        .map_err(|source| CmdError::Spawn {
            cmd: cmd.clone(),
            source,
        })?;
    into_checked(
        bin,
        args,
        CommandOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
    )
}

/// Parses a command's stdout as JSON into `T`. A shape mismatch is a `Parse`
/// error — never a defaulted value.
pub fn parse_json<T: serde::de::DeserializeOwned>(cmd: &str, raw: &str) -> Result<T, CmdError> {
    serde_json::from_str(raw).map_err(|e| CmdError::parse(cmd, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kubectl_not_found_is_classified_as_not_found() {
        let e = CmdError::failed(
            "kubectl get configmap yolab-disk-config -n rook-ceph -o json",
            "Error from server (NotFound): configmaps \"yolab-disk-config\" not found",
        );
        assert!(e.is_not_found());
        assert!(is_not_found(&e));
        assert!(!e.is_unanswered());
    }

    #[test]
    fn an_unreachable_api_server_is_never_not_found() {
        for msg in [
            "The connection to the server localhost:6443 was refused - did you specify the right host or port?",
            "unable to connect to the server: EOF",
            "context deadline exceeded",
        ] {
            let e = CmdError::failed("kubectl get configmap x", msg);
            assert!(!e.is_not_found(), "{msg}");
            assert!(e.is_unanswered(), "{msg}");
        }
    }

    #[test]
    fn a_timeout_is_unanswered_and_not_absent() {
        let e = CmdError::Timeout {
            cmd: "rbd ls images".into(),
            after: Duration::from_secs(30),
        };
        assert!(!e.is_not_found());
        assert!(e.is_unanswered());
        assert_eq!(e.failure(), None);
    }

    #[test]
    fn a_parse_error_is_neither_absent_nor_unanswered() {
        let e = CmdError::parse("ceph osd dump", "expected value");
        assert!(!e.is_not_found());
        assert!(!e.is_unanswered());
    }

    #[test]
    fn kubectl_conflicts_and_duplicates_are_distinguished() {
        assert_eq!(
            classify(
                "kubectl",
                "Error from server (Conflict): Operation cannot be fulfilled on configmaps \"x\": the object has been modified"
            ),
            Failure::Conflict
        );
        assert_eq!(
            classify(
                "kubectl",
                "Error from server (AlreadyExists): leases.coordination.k8s.io \"x\" already exists"
            ),
            Failure::AlreadyExists
        );
    }

    #[test]
    fn ceph_errnos_are_classified() {
        assert_eq!(
            classify("ceph", "Error ENOENT: unrecognized pool 'x'"),
            Failure::NotFound
        );
        assert_eq!(
            classify(
                "ceph",
                "Error EBUSY: OSD(s) 3 have 25 pgs currently mapped to them."
            ),
            Failure::Busy
        );
        assert_eq!(
            classify("ceph", "Error EEXIST: entity osd.3 exists"),
            Failure::AlreadyExists
        );
        assert_eq!(classify("ceph", "Error EINVAL: bad"), Failure::Other);
    }

    #[test]
    fn an_unknown_binary_is_never_classified_as_absent() {
        assert_eq!(classify("lsblk", "NotFound ENOENT"), Failure::Other);
    }

    #[test]
    fn not_found_survives_anyhow_context() {
        let inner = CmdError::failed("kubectl get lease x", "Error from server (NotFound): x");
        let wrapped = anyhow::Error::new(inner).context("reading the lease");
        assert!(is_not_found(&wrapped));
    }

    #[test]
    fn prose_that_merely_mentions_notfound_is_not_a_not_found() {
        // The old check was a substring search over any error's text.
        let e = anyhow::anyhow!("helm: release NotFound in cache");
        assert!(!is_not_found(&e));
    }

    #[test]
    fn a_shape_mismatch_is_a_parse_error() {
        #[derive(serde::Deserialize, Debug)]
        struct Stat {
            #[allow(dead_code)]
            num_up_osds: u64,
        }
        let err = parse_json::<Stat>("ceph osd stat", "{}").unwrap_err();
        assert!(matches!(err, CmdError::Parse { .. }));
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_spawn_error() {
        let err = output("yolab-definitely-not-a-binary", &[], Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Spawn { .. }));
        assert!(err.is_unanswered());
    }

    #[tokio::test]
    async fn a_slow_command_times_out() {
        let err = output("sleep", &["5"], Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Timeout { .. }));
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_classified_failure() {
        let err = checked(
            "sh",
            &["-c", "echo nope >&2; exit 3"],
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        match err {
            CmdError::Failed { stderr, kind, .. } => {
                assert_eq!(stderr, "nope");
                assert_eq!(kind, Failure::Other);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
