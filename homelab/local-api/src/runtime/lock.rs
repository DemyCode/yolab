use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LOCK_DIR: &str = "/run/yolab/locks";

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

fn taken(attempt: Result<(), TryLockError>) -> std::io::Result<bool> {
    match attempt {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

pub fn try_acquire_in(dir: &Path, name: &str) -> std::io::Result<Option<LockGuard>> {
    let file = open(dir, name)?;
    let attempt = file.try_lock();
    guard(file, name, attempt)
}

fn guard(
    file: File,
    name: &str,
    attempt: Result<(), TryLockError>,
) -> std::io::Result<Option<LockGuard>> {
    Ok(taken(attempt)?.then(|| LockGuard {
        _file: file,
        name: name.to_string(),
    }))
}

pub async fn acquire(name: &str, timeout: Duration) -> std::io::Result<Option<LockGuard>> {
    acquire_in(&crate::storage::on_machine(LOCK_DIR), name, timeout).await
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
        assert!(try_acquire_in(dir.path(), "store").unwrap().is_none());
        drop(first);
        assert!(try_acquire_in(dir.path(), "store").unwrap().is_some());
    }

    #[test]
    fn a_lock_the_kernel_refuses_is_an_error_not_a_lock_someone_else_holds() {
        let refused = TryLockError::Error(std::io::Error::from(std::io::ErrorKind::Unsupported));
        assert_eq!(
            taken(Err(refused)).unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
        assert!(!taken(Err(TryLockError::WouldBlock)).unwrap());
        assert!(taken(Ok(())).unwrap());
    }

    #[test]
    fn a_lock_the_kernel_refuses_hands_out_no_guard() {
        let dir = tempfile::tempdir().unwrap();
        let file = open(dir.path(), "refused").unwrap();
        let refused = TryLockError::Error(std::io::Error::from(std::io::ErrorKind::Unsupported));
        assert_eq!(
            guard(file, "refused", Err(refused)).unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }

    #[tokio::test]
    async fn a_machine_lock_lives_in_the_machine_lock_folder() {
        let got = acquire("machine", Duration::ZERO).await.unwrap();
        assert!(got.is_some());
        assert!(crate::storage::on_machine(LOCK_DIR)
            .join("machine.lock")
            .exists());
    }

    #[tokio::test]
    async fn waiting_on_a_lock_folder_that_cannot_be_made_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder");
        std::fs::write(&file, "").unwrap();
        assert!(acquire_in(&file, "store", Duration::from_secs(1))
            .await
            .is_err());
    }

    #[test]
    fn a_lock_folder_that_cannot_be_made_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder");
        std::fs::write(&file, "").unwrap();
        assert!(try_acquire_in(&file, "store").is_err());
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
