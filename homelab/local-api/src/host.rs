//! The seam between reconcile logic and the machine it runs on.
//!
//! Every side effect — kubectl, ceph, ceph-volume, systemctl, lsblk — goes
//! through this trait. The real implementation shells out to those binaries;
//! tests substitute `fake::FakeHost`, which records calls and answers from a
//! script, so the logic can be exercised without a cluster.
//!
//! Every method fails with `exec::CmdError`. There is no `unwrap_or_default`
//! anywhere on this seam: a timeout is a timeout, a NotFound is a NotFound, and
//! a caller that wants one of them to mean "empty" has to say so by name.

use std::future::Future;
use std::time::Duration;

use serde_json::Value;

use crate::ceph::destructive::Door;
use crate::ceph::model::{self, OsdDump, PgBrief};
pub use crate::exec::CommandOutput;
use crate::exec::{self, CmdError};

/// The DEFAULT bound for a `RealHost` subprocess call. Not a universal one: work whose
/// duration scales with data rather than with the cluster's responsiveness must ask for
/// its own via `run_cmd_bounded`.
///
/// This comment used to claim 600s was "long enough for a real multi-GB `cp` of
/// containerd's data-root over a modest link". That was never measured and it is false:
/// node2's 9.2G store copies at ~9MB/s and needs ~1000s, so the migration was SIGKILLed
/// here on every attempt it ever made — four times in 45 minutes on 2026-09-07, each
/// run re-triggered by the timer, each one holding k3s down. The sentence is what kept
/// anyone from looking: it asserted the exact property that was broken. Measure before
/// writing a bound into prose.
///
/// 600s remains right for everything else here, because the failure it guards against is
/// not "a slow command", it is a command that never returns at all: a `mkfs`/`mount`
/// against an RBD device blocked on Ceph parks the calling thread in uninterruptible
/// sleep (state D), which no signal can end. Bounding the await at least lets this task
/// fail fast and report it; the wedged child is orphaned either way.
pub const RUN_CMD_TIMEOUT: Duration = Duration::from_secs(600);

pub type HostResult<T> = Result<T, CmdError>;

#[allow(clippy::manual_async_fn)]
pub trait Host: Send + Sync + Clone {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = HostResult<String>> + Send + 'a;
    fn ceph_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a;
    fn ceph_volume<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a;
    fn kubectl<'a>(&self, args: &'a [&str])
        -> impl Future<Output = HostResult<String>> + Send + 'a;
    fn kubectl_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a;
    /// Pipe a manifest to `kubectl apply -f -`.
    fn kubectl_apply<'a>(
        &self,
        manifest: &'a str,
    ) -> impl Future<Output = HostResult<()>> + Send + 'a;
    /// Pipe a manifest to `kubectl create -f -` (`verb = "create"`) or
    /// `kubectl replace -f -` (`verb = "replace"`, a compare-and-swap when the
    /// manifest carries `metadata.resourceVersion`). Hosts that never write
    /// records need not support it.
    fn kubectl_write<'a>(
        &self,
        verb: &'a str,
        _manifest: &'a str,
    ) -> impl Future<Output = HostResult<()>> + Send + 'a {
        async move {
            Err(CmdError::Forbidden {
                cmd: format!("kubectl {verb} -f - (not supported by this host)"),
            })
        }
    }
    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a;
    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a;

    /// The destructive entry points. Only `ceph::destructive` can supply a
    /// `Door`. The defaults route through the guarded methods, so a host that
    /// does not override them refuses destruction rather than performing it.
    fn ceph_destructive<'a>(
        &self,
        _door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        self.ceph(args)
    }

    fn ceph_volume_destructive<'a>(
        &self,
        _door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        self.ceph_volume(args)
    }

    /// `run_cmd` for the one kind of work whose duration scales with the data,
    /// not with the cluster's responsiveness (mkfs, a store copy).
    ///
    /// Applying the 600s bound to a copy made the first migration impossible on
    /// any node with a real image store (node2, 2026-09-07: 9.2G at ~9MB/s needed
    /// ~1000s). Scripted test hosts have no clock, so the default ignores the
    /// bound; `RealHost` overrides it.
    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        self.run_cmd(bin, args)
    }

    fn reachable(&self) -> impl Future<Output = bool> + Send + '_ {
        async move { self.ceph(&["-s"]).await.is_ok() }
    }

    /// Our cluster's fsid. An error when Ceph cannot say — callers compare disk
    /// labels against it, and a defaulted value makes every foreign disk look
    /// ours or every disk of ours look foreign.
    fn cluster_fsid(&self) -> impl Future<Output = HostResult<String>> + Send + '_ {
        async move {
            let first = match self.ceph_json(&["fsid"]).await {
                Ok(v) => match v["fsid"].as_str().filter(|s| !s.is_empty()) {
                    Some(f) => return Ok(f.to_string()),
                    None => CmdError::parse("ceph fsid -f json", "no fsid field"),
                },
                Err(e) => e,
            };
            if first.is_unanswered() {
                return Err(first);
            }
            let plain = self.ceph(&["fsid"]).await?;
            let f = plain.trim();
            if f.is_empty() {
                return Err(CmdError::parse("ceph fsid", "empty output"));
            }
            Ok(f.to_string())
        }
    }

    /// Every OSD id the cluster knows. Output that is not a list of integers is
    /// a parse error — never an empty cluster.
    fn osd_ids(&self) -> impl Future<Output = HostResult<Vec<i64>>> + Send + '_ {
        async move {
            let v = self.ceph_json(&["osd", "ls"]).await?;
            serde_json::from_value::<Vec<i64>>(v).map_err(|e| CmdError::parse("ceph osd ls", e))
        }
    }

    fn osd_dump(&self) -> impl Future<Output = HostResult<OsdDump>> + Send + '_ {
        async move {
            let v = self.ceph_json(&["osd", "dump"]).await?;
            serde_json::from_value(v).map_err(|e| CmdError::parse("ceph osd dump", e))
        }
    }

    fn pgs_brief(&self) -> impl Future<Output = HostResult<Vec<PgBrief>>> + Send + '_ {
        async move {
            let raw = self
                .ceph(&["pg", "dump", "pgs_brief", "-f", "json"])
                .await?;
            model::parse_pgs_brief("ceph pg dump pgs_brief", &raw)
        }
    }

    /// `kubectl get … -o json` where NotFound is a legitimate answer: `Ok(None)`
    /// for a missing object, `Err` for everything else.
    fn kubectl_get_opt<'a>(
        &'a self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Option<Value>>> + Send + 'a {
        async move {
            match self.kubectl_json(args).await {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.is_not_found() => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct RealHost;

#[allow(clippy::manual_async_fn)]
impl Host for RealHost {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph(args).await }
    }

    fn ceph_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
        async move { crate::ceph_cli::ceph_json(args).await }
    }

    fn ceph_volume<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph_volume(args).await }
    }

    fn ceph_destructive<'a>(
        &self,
        door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        crate::ceph_cli::ceph_destructive(door, args)
    }

    fn ceph_volume_destructive<'a>(
        &self,
        door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        crate::ceph_cli::ceph_volume_destructive(door, args)
    }

    fn kubectl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { crate::kubectl::run(args).await }
    }

    fn kubectl_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
        async move { crate::kubectl::get_json(args).await }
    }

    fn kubectl_apply<'a>(
        &self,
        manifest: &'a str,
    ) -> impl Future<Output = HostResult<()>> + Send + 'a {
        async move { crate::kubectl::apply(manifest).await }
    }

    fn kubectl_write<'a>(
        &self,
        verb: &'a str,
        manifest: &'a str,
    ) -> impl Future<Output = HostResult<()>> + Send + 'a {
        async move {
            match verb {
                "create" => crate::kubectl::create(manifest).await,
                "replace" => crate::kubectl::replace(manifest).await,
                other => Err(CmdError::Forbidden {
                    cmd: format!("kubectl {other} -f -"),
                }),
            }
        }
    }

    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move { exec::output("systemctl", args, RUN_CMD_TIMEOUT).await }
    }

    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        self.run_cmd_bounded(bin, args, RUN_CMD_TIMEOUT)
    }

    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move { exec::output(bin, args, timeout).await }
    }
}

/// A scripted `Host` for tests, shared by every module so each one does not
/// grow its own copy.
#[cfg(test)]
pub(crate) mod fake {
    use std::{
        collections::VecDeque,
        future::Future,
        sync::{Arc, Mutex},
    };

    use serde_json::Value;

    use super::{CommandOutput, Door, Host, HostResult};
    use crate::ceph::destructive::is_destructive;
    use crate::exec::CmdError;

    type ScriptedAnswer = std::result::Result<String, String>;
    type Script = Vec<(String, VecDeque<ScriptedAnswer>)>;

    /// Every effect is a command, so scripting commands is what makes the
    /// sequence assertable. Unscripted commands fail loudly by default — a test
    /// must say what the machine answers rather than silently getting a
    /// plausible one.
    ///
    /// Each prefix maps to a queue of answers, consumed in call order; the last
    /// answer repeats once the queue is empty, so a test only has to spell out
    /// the calls whose answer actually changes.
    #[derive(Clone, Default)]
    pub(crate) struct FakeHost {
        calls: Arc<Mutex<Vec<String>>>,
        script: Arc<Mutex<Script>>,
    }

    impl FakeHost {
        pub fn new() -> Self {
            Self::default()
        }

        fn push(&self, prefix: &str, answer: ScriptedAnswer) {
            let mut script = self.script.lock().unwrap();
            match script.iter_mut().find(|(p, _)| p == prefix) {
                Some((_, q)) => q.push_back(answer),
                None => {
                    let mut q = VecDeque::new();
                    q.push_back(answer);
                    script.push((prefix.to_string(), q));
                }
            }
        }

        pub fn ok(self, prefix: &str, out: &str) -> Self {
            self.push(prefix, Ok(out.to_string()));
            self
        }

        pub fn fail(self, prefix: &str, err: &str) -> Self {
            self.push(prefix, Err(err.to_string()));
            self
        }

        /// Longest matching prefix wins, so a general steady-state answer and a
        /// more specific override can coexist without colliding.
        fn answer(&self, cmd: &str) -> HostResult<String> {
            self.calls.lock().unwrap().push(cmd.to_string());
            let mut script = self.script.lock().unwrap();
            let best = script
                .iter_mut()
                .filter(|(p, _)| cmd.starts_with(p.as_str()))
                .max_by_key(|(p, _)| p.len());
            match best {
                Some((_, q)) => {
                    let out = if q.len() > 1 {
                        q.pop_front().unwrap()
                    } else {
                        q.front().unwrap().clone()
                    };
                    out.map_err(|e| CmdError::failed(cmd, e))
                }
                None => Err(CmdError::failed(cmd, format!("unscripted command: {cmd}"))),
            }
        }

        fn guarded(&self, bin: &str, args: &[&str]) -> HostResult<String> {
            let cmd = format!("{bin} {}", args.join(" "));
            if is_destructive(bin, args) {
                self.calls.lock().unwrap().push(format!("REFUSED {cmd}"));
                return Err(CmdError::Forbidden { cmd });
            }
            self.answer(&cmd)
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn ran(&self, needle: &str) -> bool {
            self.calls()
                .iter()
                .any(|c| !c.starts_with("REFUSED ") && c.contains(needle))
        }

        /// Whether a destructive command was attempted outside the door.
        pub fn refused(&self, needle: &str) -> bool {
            self.calls()
                .iter()
                .any(|c| c.starts_with("REFUSED ") && c.contains(needle))
        }

        /// Index of the first call containing `needle`, for ordering asserts.
        pub fn position(&self, needle: &str) -> Option<usize> {
            self.calls().iter().position(|c| c.contains(needle))
        }
    }

    fn output_of(out: HostResult<String>) -> CommandOutput {
        match out {
            Ok(stdout) => CommandOutput {
                success: true,
                stdout,
                stderr: String::new(),
            },
            Err(e) => CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for FakeHost {
        fn ceph<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.guarded("ceph", args) }
        }

        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.guarded("ceph", args)?;
                crate::exec::parse_json(&format!("ceph {}", args.join(" ")), &raw)
            }
        }

        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.guarded("ceph-volume", args) }
        }

        fn ceph_destructive<'a>(
            &self,
            _door: &Door,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph {}", args.join(" "))) }
        }

        fn ceph_volume_destructive<'a>(
            &self,
            _door: &Door,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph-volume {}", args.join(" "))) }
        }

        fn kubectl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl {}", args.join(" "))) }
        }

        fn kubectl_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let cmd = format!("kubectl {}", args.join(" "));
                let raw = me.answer(&cmd)?;
                crate::exec::parse_json(&cmd, &raw)
            }
        }

        fn kubectl_apply<'a>(
            &self,
            manifest: &'a str,
        ) -> impl Future<Output = HostResult<()>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl-apply {manifest}")).map(|_| ()) }
        }

        fn kubectl_write<'a>(
            &self,
            verb: &'a str,
            manifest: &'a str,
        ) -> impl Future<Output = HostResult<()>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl-{verb} {manifest}")).map(|_| ()) }
        }

        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                Ok(output_of(
                    me.answer(&format!("systemctl {}", args.join(" "))),
                ))
            }
        }

        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move { Ok(output_of(me.answer(&format!("{bin} {}", args.join(" "))))) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHost;
    use super::*;

    #[tokio::test]
    async fn osd_ids_that_are_not_a_list_are_an_error_not_an_empty_cluster() {
        let host = FakeHost::new().ok("ceph osd ls", r#"{"unexpected": true}"#);
        assert!(host.osd_ids().await.is_err());
        let host = FakeHost::new().ok("ceph osd ls", "[0, 3]");
        assert_eq!(host.osd_ids().await.unwrap(), vec![0, 3]);
    }

    #[tokio::test]
    async fn an_unreadable_fsid_is_an_error() {
        let host = FakeHost::new().fail("ceph fsid", "timed out");
        assert!(host.cluster_fsid().await.is_err());
        let host = FakeHost::new()
            .ok("ceph fsid -f json", r#"{"fsid":""}"#)
            .ok("ceph fsid", "");
        assert!(host.cluster_fsid().await.is_err());
    }

    #[tokio::test]
    async fn kubectl_get_opt_separates_absent_from_unreachable() {
        let absent = FakeHost::new().fail(
            "kubectl get configmap x",
            "Error from server (NotFound): configmaps \"x\" not found",
        );
        assert_eq!(
            absent
                .kubectl_get_opt(&["get", "configmap", "x"])
                .await
                .unwrap(),
            None
        );
        let down = FakeHost::new().fail(
            "kubectl get configmap x",
            "The connection to the server localhost:6443 was refused",
        );
        assert!(down
            .kubectl_get_opt(&["get", "configmap", "x"])
            .await
            .is_err());
    }
}
