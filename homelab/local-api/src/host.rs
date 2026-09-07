//! The seam between the disk reconciler and the machine it runs on.
//!
//! Every side effect the reconciler performs — kubectl, ceph, ceph-volume,
//! systemctl, lsblk — goes through this trait. The real implementation shells
//! out to those binaries; tests substitute a fake that records calls and returns
//! canned answers, so the reconcile logic can be exercised without a cluster.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

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
/// not "a slow command", it is a command
/// that never returns at all: a `mkfs`/`cp`/`mount` against an RBD device blocked on
/// Ceph parks the calling thread in uninterruptible sleep (state D), a state no signal
/// — including the `kill_on_drop` below — can end. Before this, that meant the entire
/// `local-api storage <cmd>` process hung forever; systemd's own `TimeoutStartSec` was
/// the only thing that ever
/// intervened, and it could only SIGKILL the *wrapping* process, never the wedged child
/// itself. Bounding the await here at least lets that wrapping process fail fast and
/// exit cleanly instead of needing to be killed — the wedged child is orphaned either
/// way, since nothing short of the kernel resolving the I/O (or a reboot) can end it.
const RUN_CMD_TIMEOUT: Duration = Duration::from_secs(600);

/// The observable result of running a process, without carrying the process
/// handle. `std::process::Output` cannot be built by hand in tests, so a fake
/// host needs its own shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

fn from_output(out: std::process::Output) -> CommandOutput {
    CommandOutput {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

#[allow(clippy::manual_async_fn)]
pub trait Host: Send + Sync + Clone {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a;
    fn ceph_json<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<Value>> + Send + 'a;
    fn ceph_volume<'a>(&self, args: &'a [&str])
        -> impl Future<Output = Result<String>> + Send + 'a;
    fn kubectl<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a;
    fn kubectl_json<'a>(&self, args: &'a [&str])
        -> impl Future<Output = Result<Value>> + Send + 'a;
    /// Pipe a manifest to `kubectl apply -f -`. A distinct primitive (not
    /// folded into `run_cmd`) so callers that write Kubernetes objects — like
    /// storage::csi_secrets — stay behind the same seam as their reads,
    /// rather than reaching around it to `crate::kubectl::apply` the way
    /// pre-existing code (lease.rs, routers/ceph_join.rs) does.
    fn kubectl_apply<'a>(&self, manifest: &'a str) -> impl Future<Output = Result<()>> + Send + 'a;
    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a;
    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a;

    /// `run_cmd` for the one kind of work whose duration scales with the data,
    /// not with the cluster's responsiveness.
    ///
    /// `RUN_CMD_TIMEOUT` is 600s, which is the right bound for every `rbd`,
    /// `mkfs`, `mount` or `systemctl` call here: past ten minutes those are hung,
    /// not slow. Copying an image store is a different animal — it moves
    /// gigabytes across the network, and how long that legitimately takes is a
    /// property of how many containers the node runs.
    ///
    /// Applying the 600s bound to it made the first migration impossible on any
    /// node with a real image store. On node2 (2026-09-07, 9.2G at ~9MB/s) the
    /// `cp` needed ~1000s, was SIGKILLed at 600s on every attempt, and
    /// `migrate_existing_store` reported "copy failed (is the RBD large enough?)"
    /// — which was doubly unhelpful, since the RBD was 163G and size had nothing
    /// to do with it. The unit's own `TimeoutStartSec` is 3600s precisely so this
    /// copy would not be mistaken for a hang; this inner bound defeated it.
    ///
    /// Defaults to ignoring the bound and deferring to `run_cmd`, which is what
    /// every scripted test host wants — they have no clock, and the seam exists
    /// so that *production* can pick a real bound per call. `RealHost` overrides
    /// it; nothing else needs to.
    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _timeout: Duration,
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        self.run_cmd(bin, args)
    }

    fn reachable(&self) -> impl Future<Output = bool> + Send + '_ {
        async move { self.ceph(&["-s"]).await.is_ok() }
    }

    fn cluster_fsid(&self) -> impl Future<Output = Option<String>> + Send + '_ {
        async move {
            if let Ok(v) = self.ceph_json(&["fsid"]).await {
                if let Some(f) = v["fsid"].as_str().filter(|s| !s.is_empty()) {
                    return Some(f.to_string());
                }
            }
            self.ceph(&["fsid"])
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
    }

    fn osd_ids(&self) -> impl Future<Output = Result<Vec<i64>>> + Send + '_ {
        async move {
            let v = self.ceph_json(&["osd", "ls"]).await?;
            Ok(v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default())
        }
    }

    fn osd_safe_to_destroy(&self, osd_id: i64) -> impl Future<Output = bool> + Send + '_ {
        async move {
            self.ceph_json(&["osd", "safe-to-destroy", &format!("osd.{osd_id}")])
                .await
                .ok()
                .and_then(|v| {
                    v["safe_to_destroy"]
                        .as_array()
                        .map(|a| a.iter().any(|x| x.as_i64() == Some(osd_id)))
                })
                .unwrap_or(false)
        }
    }

    fn osd_purge(&self, osd_id: i64) -> impl Future<Output = Result<String>> + Send + '_ {
        async move {
            self.ceph(&[
                "osd",
                "purge",
                &format!("osd.{osd_id}"),
                "--yes-i-really-mean-it",
            ])
            .await
        }
    }
}

#[derive(Clone, Default)]
pub struct RealHost;

#[allow(clippy::manual_async_fn)]
impl Host for RealHost {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph(args).await }
    }

    fn ceph_json<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<Value>> + Send + 'a {
        async move { crate::ceph_cli::ceph_json(args).await }
    }

    fn ceph_volume<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph_volume(args).await }
    }

    fn kubectl<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
        async move { crate::kubectl::run(args).await }
    }

    fn kubectl_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<Value>> + Send + 'a {
        async move { crate::kubectl::get_json(args).await }
    }

    fn kubectl_apply<'a>(&self, manifest: &'a str) -> impl Future<Output = Result<()>> + Send + 'a {
        async move { crate::kubectl::apply(manifest).await }
    }

    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        async move {
            let work = Command::new("systemctl")
                .args(args)
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(RUN_CMD_TIMEOUT, work)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "systemctl {}: timed out after {}s",
                        args.join(" "),
                        RUN_CMD_TIMEOUT.as_secs()
                    )
                })?
                .context("spawn systemctl")?;
            Ok(from_output(out))
        }
    }

    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        self.run_cmd_bounded(bin, args, RUN_CMD_TIMEOUT)
    }

    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        async move {
            let work = Command::new(bin).args(args).kill_on_drop(true).output();
            let out = tokio::time::timeout(timeout, work)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "{bin} {}: timed out after {}s",
                        args.join(" "),
                        timeout.as_secs()
                    )
                })?
                .with_context(|| format!("spawn {bin}"))?;
            Ok(from_output(out))
        }
    }
}

/// A scripted `Host` for tests, shared by every module under `storage/` so
/// each one does not grow its own copy. (disks_reconciler.rs predates this and
/// keeps its own private version — not worth the churn of migrating a 4900-line
/// file's existing, passing test suite onto a shared one.)
#[cfg(test)]
pub(crate) mod fake {
    use std::{
        collections::VecDeque,
        future::Future,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use serde_json::Value;

    use super::{CommandOutput, Host};

    type ScriptedAnswer = std::result::Result<String, String>;
    type Script = Vec<(String, VecDeque<ScriptedAnswer>)>;

    /// Every effect a storage subcommand performs is a command, so scripting
    /// commands is what makes the sequence assertable. Unscripted commands
    /// fail loudly by default — a test must say what the machine answers
    /// rather than silently getting a plausible one.
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
        fn answer(&self, cmd: &str) -> Result<String> {
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
                    out.map_err(|e| anyhow::anyhow!("{e}"))
                }
                None => Err(anyhow::anyhow!("unscripted command: {cmd}")),
            }
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn ran(&self, needle: &str) -> bool {
            self.calls().iter().any(|c| c.contains(needle))
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for FakeHost {
        fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph {}", args.join(" "))) }
        }

        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.answer(&format!("ceph {}", args.join(" ")))?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            }
        }

        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph-volume {}", args.join(" "))) }
        }

        fn kubectl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl {}", args.join(" "))) }
        }

        fn kubectl_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.answer(&format!("kubectl {}", args.join(" ")))?;
                Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
            }
        }

        fn kubectl_apply<'a>(
            &self,
            manifest: &'a str,
        ) -> impl Future<Output = Result<()>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("kubectl-apply {manifest}")).map(|_| ()) }
        }

        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let out = me.answer(&format!("systemctl {}", args.join(" ")));
                Ok(CommandOutput {
                    success: out.is_ok(),
                    stdout: out.unwrap_or_default(),
                    stderr: String::new(),
                })
            }
        }

        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let out = me.answer(&format!("{bin} {}", args.join(" ")));
                Ok(CommandOutput {
                    success: out.is_ok(),
                    stdout: out.unwrap_or_default(),
                    stderr: String::new(),
                })
            }
        }
    }
}
