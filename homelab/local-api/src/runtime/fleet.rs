use std::future::Future;
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Rolled {
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub stopped_at: Option<String>,
    pub why: Option<String>,
}

pub trait Fleet: Send + Sync {
    fn act(&self, node: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn settled(&self, node: &str) -> impl Future<Output = bool> + Send;
    fn pause(&self, d: Duration) -> impl Future<Output = ()> + Send {
        async move { tokio::time::sleep(d).await }
    }
}

pub fn order(peers: &[String], me: &str) -> Vec<String> {
    let mut out: Vec<String> = peers.iter().filter(|p| *p != me).cloned().collect();
    out.sort();
    out.dedup();
    out
}

pub async fn rolling<F: Fleet>(
    fleet: &F,
    targets: &[String],
    settle_timeout: Duration,
    poll: Duration,
) -> Rolled {
    let mut rolled = Rolled::default();

    for (i, node) in targets.iter().enumerate() {
        if let Err(e) = fleet.act(node).await {
            rolled.stopped_at = Some(node.clone());
            rolled.why = Some(format!("{e:#}"));
            rolled.skipped = targets[i + 1..].to_vec();
            return rolled;
        }

        let mut waited = Duration::ZERO;
        loop {
            if fleet.settled(node).await {
                break;
            }
            if waited >= settle_timeout {
                rolled.stopped_at = Some(node.clone());
                rolled.why = Some(format!(
                    "{node} did not come back within {}s, so the machines after it were left alone",
                    settle_timeout.as_secs()
                ));
                rolled.skipped = targets[i + 1..].to_vec();
                return rolled;
            }
            fleet.pause(poll).await;
            waited += poll;
        }
        rolled.done.push(node.clone());
    }
    rolled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeFleet {
        acted: Mutex<Vec<String>>,
        settle_after: usize,
        polls: Mutex<std::collections::HashMap<String, usize>>,
        fail_on: Option<String>,
        never_settles: Option<String>,
    }

    impl Fleet for FakeFleet {
        async fn act(&self, node: &str) -> anyhow::Result<()> {
            self.acted.lock().unwrap().push(node.to_string());
            if self.fail_on.as_deref() == Some(node) {
                anyhow::bail!("{node} refused");
            }
            Ok(())
        }
        async fn settled(&self, node: &str) -> bool {
            if self.never_settles.as_deref() == Some(node) {
                return false;
            }
            let mut p = self.polls.lock().unwrap();
            let seen = p.entry(node.to_string()).or_insert(0);
            *seen += 1;
            *seen > self.settle_after
        }
        async fn pause(&self, _d: Duration) {}
    }

    fn nodes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_machine_running_the_request_is_never_one_of_the_targets() {
        let out = order(&nodes(&["b", "a", "me"]), "me");
        assert_eq!(out, nodes(&["a", "b"]));
    }

    #[test]
    fn a_duplicated_peer_is_only_acted_on_once() {
        assert_eq!(order(&nodes(&["a", "a", "b"]), "me"), nodes(&["a", "b"]));
    }

    #[test]
    fn a_single_machine_cluster_has_no_peers_to_roll() {
        assert!(order(&nodes(&["me"]), "me").is_empty());
    }

    #[tokio::test]
    async fn machines_are_taken_one_at_a_time_and_each_is_waited_for() {
        let fleet = FakeFleet {
            settle_after: 2,
            ..Default::default()
        };
        let targets = nodes(&["a", "b", "c"]);
        let out = rolling(
            &fleet,
            &targets,
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(out.done, targets);
        assert_eq!(out.stopped_at, None);
        assert_eq!(*fleet.acted.lock().unwrap(), targets);
    }

    #[tokio::test]
    async fn a_machine_that_never_comes_back_stops_the_rollout() {
        let fleet = FakeFleet {
            never_settles: Some("b".into()),
            ..Default::default()
        };
        let out = rolling(
            &fleet,
            &nodes(&["a", "b", "c"]),
            Duration::from_secs(10),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(out.done, nodes(&["a"]));
        assert_eq!(out.stopped_at.as_deref(), Some("b"));
        assert_eq!(out.skipped, nodes(&["c"]));
        assert!(
            !fleet.acted.lock().unwrap().contains(&"c".to_string()),
            "c was restarted while b was still down — that is the outage this exists to prevent"
        );
    }

    #[tokio::test]
    async fn a_machine_that_refuses_stops_the_rollout_before_touching_the_rest() {
        let fleet = FakeFleet {
            fail_on: Some("a".into()),
            ..Default::default()
        };
        let out = rolling(
            &fleet,
            &nodes(&["a", "b"]),
            Duration::from_secs(10),
            Duration::from_secs(1),
        )
        .await;
        assert!(out.done.is_empty());
        assert_eq!(out.stopped_at.as_deref(), Some("a"));
        assert_eq!(out.skipped, nodes(&["b"]));
        assert!(out.why.unwrap().contains("refused"));
    }

    #[tokio::test]
    async fn an_empty_fleet_is_a_clean_no_op() {
        let fleet = FakeFleet::default();
        let out = rolling(&fleet, &[], Duration::from_secs(1), Duration::from_secs(1)).await;
        assert_eq!(out, Rolled::default());
    }
}
