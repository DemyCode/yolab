//! Cross-process exclusion for work that a boot unit and the daemon both run.
//!
//! Some storage work has to happen before k3s at boot (so it stays a systemd
//! oneshot ordered `Before=k3s.service`) and also has to be re-checked for the
//! life of the machine (so the daemon runs the same code as a controller). The
//! two must never run it at once: `containerd-store` stops k3s, unmounts and
//! mkfs's a device, and two of those interleaved destroy the store.
//!
//! An in-process `Mutex` cannot see the other process. An `flock` on a file in
//! `/run` can, is released by the kernel when the holder dies — so a crashed
//! holder never leaves the lock stuck — and `/run` is tmpfs, so nothing survives
//! a reboot. It is what a real OS uses for exactly this.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LOCK_DIR: &str = "/run/yolab/locks";

/// Held while the guard lives; dropping it (or the process dying) releases it.
#[derive(Debug)]
pub struct LockGuard {
    _file: File,
    name: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        tracing::debug!("released lock {}", self.name);
    }
}

fn path_in(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lock"))
}

fn open(dir: &Path, name: &str) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path_in(dir, name))
}

/// Take the lock `name` in `dir` (normally `LOCK_DIR`) if nobody holds it.
/// `Ok(None)` when another process (or another holder in this one) has it.
pub fn try_acquire_in(dir: &Path, name: &str) -> std::io::Result<Option<LockGuard>> {
    let file = open(dir, name)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LockGuard {
            _file: file,
            name: name.to_string(),
        })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Wait for the lock, up to `timeout`. `Ok(None)` if it never came free.
///
/// Polls rather than blocking in `flock(LOCK_EX)`, so the wait stays bounded and
/// cancellable from async code — a boot unit must never hang forever behind a
/// holder that is itself stuck.
pub async fn acquire(name: &str, timeout: Duration) -> std::io::Result<Option<LockGuard>> {
    acquire_in(Path::new(LOCK_DIR), name, timeout).await
}

pub async fn acquire_in(
    dir: &Path,
    name: &str,
    timeout: Duration,
) -> std::io::Result<Option<LockGuard>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(g) = try_acquire_in(dir, name)? {
            return Ok(Some(g));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_cannot_be_taken_again_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_acquire_in(dir.path(), "store").unwrap();
        assert!(first.is_some());
        // flock locks belong to the open file description, so a second open of
        // the same path — exactly what another process does — is refused.
        assert!(try_acquire_in(dir.path(), "store").unwrap().is_none());
        drop(first);
        assert!(try_acquire_in(dir.path(), "store").unwrap().is_some());
    }

    #[test]
    fn different_names_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let _a = try_acquire_in(dir.path(), "a").unwrap().unwrap();
        assert!(try_acquire_in(dir.path(), "b").unwrap().is_some());
    }

    #[tokio::test]
    async fn waiting_for_a_lock_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let _held = try_acquire_in(dir.path(), "busy").unwrap().unwrap();
        let got = acquire_in(dir.path(), "busy", Duration::from_millis(600))
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn a_released_lock_is_picked_up_by_a_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let held = try_acquire_in(dir.path(), "handover").unwrap().unwrap();
        let path = dir.path().to_path_buf();
        let waiter =
            tokio::spawn(
                async move { acquire_in(&path, "handover", Duration::from_secs(5)).await },
            );
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(held);
        assert!(waiter.await.unwrap().unwrap().is_some());
    }
}
