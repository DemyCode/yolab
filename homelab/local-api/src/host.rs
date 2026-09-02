//! The seam between the disk reconciler and the machine it runs on.
//!
//! Every side effect the reconciler performs — kubectl, ceph, ceph-volume,
//! systemctl, lsblk — goes through this trait. The real implementation shells
//! out to those binaries; tests substitute a fake that records calls and returns
//! canned answers, so the reconcile logic can be exercised without a cluster.

use std::future::Future;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

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
    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a;
    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a;

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

    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        async move {
            let out = Command::new("systemctl")
                .args(args)
                .kill_on_drop(true)
                .output()
                .await
                .context("spawn systemctl")?;
            Ok(from_output(out))
        }
    }

    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = Result<CommandOutput>> + Send + 'a {
        async move {
            let out = Command::new(bin)
                .args(args)
                .kill_on_drop(true)
                .output()
                .await
                .with_context(|| format!("spawn {bin}"))?;
            Ok(from_output(out))
        }
    }
}
