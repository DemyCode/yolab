use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::ceph::commands;
use crate::ceph::destructive::{self, Door};
use crate::ceph::lvm_list;
use crate::exec::{self, CmdError};
use crate::rados;

static CEPH_VOLUME_LOCK: Mutex<()> = Mutex::const_new(());

fn is_read_only(args: &[&str]) -> bool {
    matches!(args, ["lvm", "list", ..] | ["inventory", ..])
}

const TIMEOUT: Duration = Duration::from_secs(30);

const CEPH_VOLUME_TIMEOUT: Duration = Duration::from_secs(600);

const LVS_TIMEOUT: Duration = Duration::from_secs(60);

async fn run_bin(bin: &str, args: &[&str]) -> Result<String, CmdError> {
    let out = exec::output(bin, args, TIMEOUT).await?;
    checked(bin, args, out)
}

fn checked(bin: &str, args: &[&str], out: exec::CommandOutput) -> Result<String, CmdError> {
    if out.success {
        return Ok(out.stdout);
    }
    let stderr = out.stderr.trim();
    let last = stderr.lines().last().unwrap_or("unknown error").to_string();
    Err(CmdError::Failed {
        cmd: exec::render(bin, args),
        kind: exec::classify(bin, stderr),
        stderr: last,
    })
}

fn refuse_destructive(bin: &str, args: &[&str]) -> Result<(), CmdError> {
    if destructive::is_destructive(bin, args) {
        tracing::error!(
            "refusing `{}` — destructive commands must go through ceph::destructive",
            exec::render(bin, args)
        );
        return Err(CmdError::Forbidden {
            cmd: exec::render(bin, args),
        });
    }
    Ok(())
}

pub async fn ceph(args: &[&str]) -> Result<String, CmdError> {
    refuse_destructive("ceph", args)?;
    run_ceph(args).await
}

async fn run_ceph(args: &[&str]) -> Result<String, CmdError> {
    let Some(cmd) = commands::mon_command(args) else {
        return run_bin("ceph", args).await;
    };
    match rados::run(&cmd, TIMEOUT).await? {
        rados::Outcome::Answered(out) => checked("ceph", args, out),
        rados::Outcome::NotRun(why) => {
            tracing::debug!(
                "`{}` runs through the ceph CLI because librados is unavailable: {why}",
                exec::render("ceph", args)
            );
            run_bin("ceph", args).await
        }
    }
}

pub fn ceph_destructive<'a>(
    _door: &Door,
    args: &'a [&str],
) -> impl std::future::Future<Output = Result<String, CmdError>> + Send + 'a {
    async move { run_ceph(args).await }
}

fn with_json_format<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut a = args.to_vec();
    a.extend_from_slice(&["-f", "json"]);
    a
}

pub async fn ceph_json(args: &[&str]) -> Result<Value, CmdError> {
    let a = with_json_format(args);
    let raw = ceph(&a).await?;
    exec::parse_json(&exec::render("ceph", &a), &raw)
}

pub async fn ceph_volume(args: &[&str]) -> Result<String, CmdError> {
    refuse_destructive("ceph-volume", args)?;
    ceph_volume_inner(args).await
}

pub fn ceph_volume_destructive<'a>(
    _door: &Door,
    args: &'a [&str],
) -> impl std::future::Future<Output = Result<String, CmdError>> + Send + 'a {
    async move { ceph_volume_inner(args).await }
}

async fn ceph_volume_inner(args: &[&str]) -> Result<String, CmdError> {
    let cmd = exec::render("ceph-volume", args);
    let _serialised = if is_read_only(args) {
        let Ok(guard) = CEPH_VOLUME_LOCK.try_lock() else {
            return Err(CmdError::Busy { cmd });
        };
        guard
    } else {
        CEPH_VOLUME_LOCK.lock().await
    };
    if is_full_listing(args) {
        let out = exec::output("lvs", lvm_list::LVS_ARGS, LVS_TIMEOUT).await?;
        let raw = exec::into_checked("lvs", lvm_list::LVS_ARGS, out)?;
        return lvm_list::from_lvs(&raw);
    }
    let out = exec::output("ceph-volume", args, CEPH_VOLUME_TIMEOUT).await?;
    exec::into_checked("ceph-volume", args, out)
}

fn is_full_listing(args: &[&str]) -> bool {
    args == ["lvm", "list", "--format", "json"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_full_osd_listing_is_read_from_lvs() {
        assert!(is_full_listing(&["lvm", "list", "--format", "json"]));
        for other in [
            vec!["lvm", "list", "3", "--format", "json"],
            vec!["lvm", "list"],
            vec!["inventory", "--format", "json"],
            vec!["lvm", "zap", "--destroy", "/dev/vdb"],
        ] {
            assert!(!is_full_listing(&other), "{other:?}");
        }
    }

    #[test]
    fn only_inspection_may_be_skipped() {
        assert!(is_read_only(&["lvm", "list", "--format", "json"]));
        assert!(is_read_only(&["inventory", "--format", "json"]));

        for mutating in [
            vec!["lvm", "create", "--bluestore", "--data", "/dev/vdb"],
            vec!["lvm", "zap", "--destroy", "/dev/vdb"],
            vec!["lvm", "prepare", "--data", "/dev/vdb"],
            vec!["lvm", "activate", "--all"],
        ] {
            assert!(
                !is_read_only(&mutating),
                "{mutating:?} changes the disk and must queue, never be dropped"
            );
        }
    }

    #[test]
    fn anything_unrecognised_is_treated_as_mutating() {
        assert!(!is_read_only(&["something-new"]));
        assert!(!is_read_only(&[]));
    }

    #[tokio::test]
    async fn a_purge_through_the_plain_entry_point_is_refused_before_it_runs() {
        let err = ceph(&["osd", "purge", "osd.3", "--yes-i-really-mean-it"])
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Forbidden { .. }));
    }

    #[tokio::test]
    async fn a_zap_through_the_plain_entry_point_is_refused_before_it_runs() {
        let err = ceph_volume(&["lvm", "zap", "--destroy", "/dev/vdb"])
            .await
            .unwrap_err();
        assert!(matches!(err, CmdError::Forbidden { .. }));
    }
}
