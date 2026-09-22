use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::ceph::destructive::{self, Door};
use crate::exec::{self, CmdError};

static CEPH_VOLUME_LOCK: Mutex<()> = Mutex::const_new(());

fn is_read_only(args: &[&str]) -> bool {
    matches!(args, ["lvm", "list", ..] | ["inventory", ..])
}

const TIMEOUT: Duration = Duration::from_secs(30);

const CEPH_VOLUME_TIMEOUT: Duration = Duration::from_secs(600);

async fn run_bin(bin: &str, args: &[&str]) -> Result<String, CmdError> {
    let out = exec::output(bin, args, TIMEOUT).await?;
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
    run_bin("ceph", args).await
}

pub fn ceph_destructive<'a>(
    _door: &Door,
    args: &'a [&str],
) -> impl std::future::Future<Output = Result<String, CmdError>> + Send + 'a {
    async move { run_bin("ceph", args).await }
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

pub async fn ceph_typed<T: DeserializeOwned>(args: &[&str]) -> Result<T, CmdError> {
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
    let out = exec::output("ceph-volume", args, CEPH_VOLUME_TIMEOUT).await?;
    exec::into_checked("ceph-volume", args, out)
}

pub async fn cluster_fsid() -> Result<String, CmdError> {
    let json_err = match ceph_json(&["fsid"]).await {
        Ok(v) => match v["fsid"].as_str().filter(|s| !s.is_empty()) {
            Some(f) => return Ok(f.to_string()),
            None => CmdError::parse("ceph fsid -f json", "no fsid field"),
        },
        Err(e) => e,
    };
    if json_err.is_unanswered() {
        return Err(json_err);
    }
    let plain = ceph(&["fsid"]).await?;
    let f = plain.trim();
    if f.is_empty() {
        return Err(CmdError::parse("ceph fsid", "empty output"));
    }
    Ok(f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
