use std::time::Duration;

pub(crate) enum Step<T> {
    Done(T),
    Failed(anyhow::Error),
    Pending(anyhow::Error),
}

pub(crate) async fn until<T, F, Fut>(
    wait: Duration,
    every: Duration,
    mut check: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Step<T>>,
{
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let still = match check().await {
            Step::Done(value) => return Ok(value),
            Step::Failed(e) => return Err(e),
            Step::Pending(why) => why,
        };
        if deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .is_zero()
        {
            return Err(still);
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Duration = Duration::ZERO;

    #[tokio::test]
    async fn a_check_that_is_already_done_returns_its_value() {
        assert_eq!(
            until(NOW, NOW, || async { Step::Done(7) }).await.unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn a_failure_ends_the_wait_with_its_own_error() {
        let mut calls = 0;
        let e = until::<(), _, _>(Duration::from_secs(60), NOW, || {
            calls += 1;
            async { Step::Failed(anyhow::anyhow!("it broke")) }
        })
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "it broke");
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn running_out_of_time_reports_what_was_still_pending() {
        let e = until::<(), _, _>(NOW, NOW, || async {
            Step::Pending(anyhow::anyhow!("pvc-a is still there"))
        })
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "pvc-a is still there");
    }

    #[tokio::test]
    async fn it_checks_again_until_the_condition_holds() {
        let mut left = 3;
        let got = until(Duration::from_secs(60), NOW, || {
            left -= 1;
            let now_left = left;
            async move {
                if now_left == 0 {
                    Step::Done("ready")
                } else {
                    Step::Pending(anyhow::anyhow!("{now_left} to go"))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(got, "ready");
        assert_eq!(left, 0);
    }

    fn scripted(steps: Vec<Step<u8>>) -> impl FnMut() -> std::future::Ready<Step<u8>> {
        let mut steps = steps.into_iter();
        move || std::future::ready(steps.next().unwrap())
    }

    #[tokio::test]
    async fn the_same_check_can_finish_fail_or_run_out_of_time() {
        let minute = Duration::from_secs(60);
        assert_eq!(
            until(
                minute,
                NOW,
                scripted(vec![
                    Step::Pending(anyhow::anyhow!("not yet")),
                    Step::Done(1)
                ])
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            until(
                minute,
                NOW,
                scripted(vec![Step::Failed(anyhow::anyhow!("it broke"))])
            )
            .await
            .unwrap_err()
            .to_string(),
            "it broke"
        );
        assert_eq!(
            until(
                NOW,
                NOW,
                scripted(vec![Step::Pending(anyhow::anyhow!("still"))])
            )
            .await
            .unwrap_err()
            .to_string(),
            "still"
        );
    }

    #[tokio::test]
    async fn the_check_always_runs_at_least_once_even_with_no_time() {
        let mut calls = 0;
        let e = until::<(), _, _>(NOW, NOW, || {
            calls += 1;
            async { Step::Pending(anyhow::anyhow!("never")) }
        })
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "never");
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_check_is_the_one_made_exactly_at_the_deadline() {
        let mut calls = 0;
        let started = tokio::time::Instant::now();
        let e = until::<(), _, _>(Duration::from_secs(3), Duration::from_secs(1), || {
            calls += 1;
            async { Step::Pending(anyhow::anyhow!("not yet")) }
        })
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "not yet");
        assert_eq!(calls, 4);
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }
}
