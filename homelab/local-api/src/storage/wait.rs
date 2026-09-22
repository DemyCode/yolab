
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::time::Instant;

#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<T> {
    Ready(T),
    NotYet(String),
}

const FIRST_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(60);
const REMIND_EVERY: Duration = Duration::from_secs(600);

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
        assert_eq!(started.elapsed(), Duration::from_secs(75 + 4 * 60));
    }
}
