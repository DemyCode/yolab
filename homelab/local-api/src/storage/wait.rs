//! Boot steps that WAIT for their preconditions instead of falling back.
//!
//! The storage path a node boots through is a straight line: the system LV
//! becomes an OSD, the images pool and this node's RBD exist, the RBD is mounted
//! as containerd's data-root, k3s starts. Every step used to have a way out —
//! "no OSD yet, exit 0", "no image yet, stay on the root disk" — and each way
//! out needed something later to put the node back on the line: timers, then a
//! controller that stopped k3s every five minutes to move the store under it.
//!
//! There is no way out now. A step that cannot happen yet says why and tries
//! again, and the node boots when it can. What a person sees is one unit in
//! `activating` with its reason in the journal, not a node quietly running on the
//! wrong store.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::time::Instant;

/// One attempt at a boot step.
#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<T> {
    Ready(T),
    /// Not possible yet — a normal state while the cluster comes up — and why,
    /// in words a person reading the journal can act on.
    NotYet(String),
}

const FIRST_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(60);
/// An unchanged reason is repeated this often, so a node stuck for an hour
/// still says what it is waiting for at the end of the journal.
const REMIND_EVERY: Duration = Duration::from_secs(600);

/// Runs `attempt` until it is ready, logging each new reason once (and every
/// `REMIND_EVERY` while it persists). An `Err` is waited out like a `NotYet`: at
/// boot a failed command is almost always the cluster not being up yet, and a
/// step that gave up would leave the node with no way to finish booting.
pub async fn until_ready<T, F, Fut>(step: &str, mut attempt: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Attempt<T>>>,
{
    let mut delay = FIRST_DELAY;
    let mut last: Option<(String, Instant)> = None;
    loop {
        let why = match attempt().await {
            Ok(Attempt::Ready(value)) => {
                if last.is_some() {
                    tracing::info!("{step}: ready");
                }
                return value;
            }
            Ok(Attempt::NotYet(why)) => why,
            Err(e) => format!("{e:#}"),
        };
        let repeat = last
            .as_ref()
            .is_some_and(|(said, at)| *said == why && at.elapsed() < REMIND_EVERY);
        if !repeat {
            tracing::warn!("{step}: waiting — {why}");
            last = Some((why, Instant::now()));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn a_step_is_retried_until_ready_and_errors_are_waited_out() {
        let tries = AtomicU32::new(0);
        let value = until_ready("test", || async {
            match tries.fetch_add(1, Ordering::SeqCst) {
                0 => Ok::<_, anyhow::Error>(Attempt::NotYet("no OSD is up yet".to_string())),
                1 => Err(anyhow::anyhow!("ceph: connection refused")),
                _ => Ok(Attempt::Ready(42)),
            }
        })
        .await;
        assert_eq!(value, 42);
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_back_off_to_a_ceiling() {
        let tries = AtomicU32::new(0);
        let started = Instant::now();
        until_ready("test", || async {
            if tries.fetch_add(1, Ordering::SeqCst) < 8 {
                Ok::<_, anyhow::Error>(Attempt::NotYet("still waiting".to_string()))
            } else {
                Ok(Attempt::Ready(()))
            }
        })
        .await;
        // 5 + 10 + 20 + 40, then the 60s ceiling four times.
        assert_eq!(started.elapsed(), Duration::from_secs(75 + 4 * 60));
    }
}
