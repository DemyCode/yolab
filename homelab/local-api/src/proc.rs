//! Subprocess streaming utilities shared across routers.

use tokio::process::Child;

/// Wraps a Child process and kills it when dropped.
/// This ensures kubectl log-follow processes are cleaned up when the SSE
/// client disconnects, preventing accumulation against kubectl's concurrency limit.
pub struct KillOnDrop(pub Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

